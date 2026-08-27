/// The exact lexical nesting limit. A script that nests template literals and
/// interpolations deeper than this fails closed (no candidates), so metadata
/// falls back to the older trusted sources rather than being guessed from a
/// partially understood script.
const MAX_LEXICAL_NESTING: usize = 256;

const ANCHOR: &str = "Site";
const MEMBER: &str = "entry";

/// Where the lexer currently is. Only `Code` can contain an executable
/// assignment; every other state is inert text as far as `Site.entry` is
/// concerned.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lexical {
    Code,
    LineComment,
    BlockComment,
    SingleQuoted,
    DoubleQuoted,
    TemplateRaw,
    RegexLiteral,
    RegexClass,
}

/// Byte offsets of the `{` opening each executable `Site.entry = {` assignment,
/// in source order.
///
/// Iterative, single-pass, and allocation-light: no recursion, one explicit
/// stack for template/interpolation nesting. Returns an empty vector when the
/// nesting limit is exceeded, so ambiguity never yields a guessed candidate.
pub(crate) fn site_entry_object_starts(script: &str) -> Vec<usize> {
    let bytes = script.as_bytes();
    let mut starts = Vec::new();
    let mut state = Lexical::Code;
    // Each entry is the brace depth of one `${ … }` interpolation. Entering a
    // template pushes nothing; entering `${` pushes 0; a `}` at depth 0 leaves
    // the interpolation and returns to the enclosing template.
    let mut interpolations: Vec<usize> = Vec::new();
    // Templates currently open, innermost last.
    let mut templates: usize = 0;
    // Whether a `/` here starts a regex literal rather than a division.
    let mut regex_allowed = true;
    let mut cursor = 0;

    while cursor < bytes.len() {
        if templates + interpolations.len() > MAX_LEXICAL_NESTING {
            return Vec::new();
        }

        match state {
            Lexical::Code => {
                match bytes[cursor] {
                    b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                        state = Lexical::LineComment;
                        cursor += 2;
                        continue;
                    }
                    b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                        state = Lexical::BlockComment;
                        cursor += 2;
                        continue;
                    }
                    b'/' if regex_allowed => {
                        state = Lexical::RegexLiteral;
                        cursor += 1;
                        continue;
                    }
                    b'\'' => {
                        state = Lexical::SingleQuoted;
                        regex_allowed = false;
                        cursor += 1;
                        continue;
                    }
                    b'"' => {
                        state = Lexical::DoubleQuoted;
                        regex_allowed = false;
                        cursor += 1;
                        continue;
                    }
                    b'`' => {
                        templates += 1;
                        state = Lexical::TemplateRaw;
                        regex_allowed = false;
                        cursor += 1;
                        continue;
                    }
                    b'{' => {
                        if let Some(depth) = interpolations.last_mut() {
                            *depth += 1;
                        }
                        regex_allowed = true;
                        cursor += 1;
                        continue;
                    }
                    b'}' => {
                        match interpolations.last_mut() {
                            Some(0) => {
                                interpolations.pop();
                                state = Lexical::TemplateRaw;
                            }
                            Some(depth) => *depth -= 1,
                            None => {}
                        }
                        regex_allowed = true;
                        cursor += 1;
                        continue;
                    }
                    _ => {}
                }

                if let Some(object_start) = site_entry_assignment_at(script, cursor) {
                    starts.push(object_start);
                    // Continue *after* the anchor, not after the object: a
                    // malformed or wrong-id candidate must not stop the scan.
                    cursor += ANCHOR.len();
                    regex_allowed = false;
                    continue;
                }

                let character = decode(script, cursor);
                regex_allowed = regex_allowed_after(character, regex_allowed);
                cursor += character.len_utf8();
            }
            Lexical::LineComment => {
                if matches!(bytes[cursor], b'\n' | b'\r') {
                    state = Lexical::Code;
                }
                cursor += 1;
            }
            Lexical::BlockComment => {
                if bytes[cursor] == b'*' && bytes.get(cursor + 1) == Some(&b'/') {
                    state = Lexical::Code;
                    cursor += 2;
                    continue;
                }
                cursor += 1;
            }
            Lexical::SingleQuoted | Lexical::DoubleQuoted => {
                let quote = if state == Lexical::SingleQuoted {
                    b'\''
                } else {
                    b'"'
                };
                match bytes[cursor] {
                    b'\\' => cursor += 2,
                    byte if byte == quote => {
                        state = Lexical::Code;
                        cursor += 1;
                    }
                    _ => cursor += 1,
                }
            }
            Lexical::TemplateRaw => match bytes[cursor] {
                b'\\' => cursor += 2,
                b'`' => {
                    templates = templates.saturating_sub(1);
                    state = Lexical::Code;
                    // The template just closed is a complete value, exactly
                    // like a closing string or regex literal, so `/` right
                    // after divides. Without this, an interpolation's `}`
                    // (which sets this true, since `}` could also end a
                    // block) would otherwise leak past the template's own
                    // close.
                    regex_allowed = false;
                    cursor += 1;
                }
                b'$' if bytes.get(cursor + 1) == Some(&b'{') => {
                    interpolations.push(0);
                    state = Lexical::Code;
                    regex_allowed = true;
                    cursor += 2;
                }
                _ => cursor += 1,
            },
            Lexical::RegexLiteral => match bytes[cursor] {
                b'\\' => cursor += 2,
                b'[' => {
                    state = Lexical::RegexClass;
                    cursor += 1;
                }
                b'/' => {
                    state = Lexical::Code;
                    regex_allowed = false;
                    cursor += 1;
                }
                b'\n' | b'\r' => {
                    // An unterminated regex cannot span a line; recover in code.
                    state = Lexical::Code;
                    cursor += 1;
                }
                _ => cursor += 1,
            },
            Lexical::RegexClass => match bytes[cursor] {
                b'\\' => cursor += 2,
                b']' => {
                    state = Lexical::RegexLiteral;
                    cursor += 1;
                }
                b'\n' | b'\r' => {
                    state = Lexical::Code;
                    cursor += 1;
                }
                _ => cursor += 1,
            },
        }
    }

    starts
}

fn decode(script: &str, cursor: usize) -> char {
    script[cursor..]
        .chars()
        .next()
        .expect("cursor stays on a character boundary")
}

/// Whether a `/` immediately after `character` starts a regex literal. A
/// value, or a `)`/`]` that closes a call, grouping, or index expression,
/// ends an expression, so a following `/` divides; everything else
/// (operators, punctuators, and keywords) is a regex position.
///
/// This is a character-class heuristic, not a real tokenizer: it classifies a
/// byte, not the token or construct it belongs to, so it has two known,
/// narrow blind spots.
///
/// False accept — a regex literal is misread as code, so its contents
/// (which never execute) could contain false `Site.entry = {` text:
/// - After a keyword that ends in an alphanumeric byte but expects an operand
///   next (`return`, `typeof`, `case`, `new`, `delete`, `throw`, `else`,
///   `do`, `in`, `of`, `instanceof`, `yield`, ...), this heuristic treats it
///   like a plain identifier — a value — so `/` divides instead of opening a
///   regex.
/// - After the `)` that closes an `if`/`while`/`for` header, a regex is
///   valid (a fresh statement follows), but this heuristic treats every
///   closing `)` alike, as if it closed a call or grouping, where `/` really
///   does divide (`f(1) / 2`). `if (a) /Site.entry = {"ditemid":N,"poster":
///   "evil"}/.test(s);` reports a candidate as a result.
///
/// False reject — code is misread as a regex, which can swallow a real
/// assignment up to the next `/` or line end:
/// - After a postfix `++`/`--`, which ends a value so `/` should divide, this
///   heuristic sees the same trailing non-alphanumeric, non-`)`/`]` byte as
///   an operator awaiting an operand, and opens a regex instead.
///
/// Neither is a viable attack on this tool. The only thing an attacker
/// actually controls on an archived page is *data* — a post's title, body,
/// or similar — and data only ever reaches a script as the contents of a
/// string or template literal, which this lexer handles correctly (fuzz
/// tested extensively: there is no route from inside a quoted literal back
/// out to bare code). Triggering either blind spot above needs raw, unquoted
/// source with a regex or postfix operator in just the wrong place, which is
/// a strictly more powerful capability than directly writing `Site.entry =
/// {...}` as real code — so it adds no exposure beyond what arbitrary script
/// injection already grants.
fn regex_allowed_after(character: char, previous: bool) -> bool {
    match character {
        character if character.is_whitespace() => previous,
        character if character.is_alphanumeric() || matches!(character, '_' | '$') => false,
        ')' | ']' => false,
        _ => true,
    }
}

/// Recognizes `Site.entry = {` at `cursor`, in code, at exact identifier and
/// member boundaries, and returns the offset of the `{`.
fn site_entry_assignment_at(script: &str, cursor: usize) -> Option<usize> {
    let bytes = script.as_bytes();

    if !script[cursor..].starts_with(ANCHOR) {
        return None;
    }
    // `MySite.entry` and `x$Site.entry` are different identifiers.
    if cursor > 0 {
        let previous = bytes[cursor - 1];
        if is_identifier_byte(previous) {
            return None;
        }
        // `Other.Site.entry` and `window?.Site.entry` are member chains, not
        // the global assignment. `?.` always ends in `.`, so checking for a
        // trailing `.` alone (after trimming whitespace, so `Other .Site` and
        // `window?. Site` are also caught) covers both spellings.
        let before = script[..cursor].trim_end();
        if before.ends_with('.') {
            return None;
        }
    }

    let mut position = skip_whitespace(bytes, cursor + ANCHOR.len());
    if bytes.get(position) != Some(&b'.') {
        return None;
    }
    position = skip_whitespace(bytes, position + 1);
    if !script[position..].starts_with(MEMBER) {
        return None;
    }
    position += MEMBER.len();
    // `Site.entryPoint` is a different member.
    if bytes.get(position).copied().is_some_and(is_identifier_byte) {
        return None;
    }

    position = skip_whitespace(bytes, position);
    // Must be exactly `=` here. This also rejects every compound-assignment
    // and comparison operator (`!=`, `<=`, `>=`, `+=`, `-=`, `*=`, `/=`,
    // `%=`, `&=`, `|=`, `^=`, ...): each has its own operator byte at this
    // exact position instead of `=`, so none of them ever reach this check
    // as a bare `=` at all.
    if bytes.get(position) != Some(&b'=') {
        return None;
    }
    // `==`, `===`, and `=>` are not assignments.
    if matches!(bytes.get(position + 1), Some(b'=' | b'>')) {
        return None;
    }

    position = skip_whitespace(bytes, position + 1);
    (bytes.get(position) == Some(&b'{')).then_some(position)
}

fn skip_whitespace(bytes: &[u8], mut position: usize) -> usize {
    while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
        position += 1;
    }
    position
}

/// ASCII-only by design: this looks back exactly one *byte*, so a real JS
/// identifier that continues with a Unicode letter (e.g. `caféSite.entry`) is
/// not recognized as one longer identifier, and `Site` is wrongly read as a
/// fresh anchor. This isn't for lack of a Unicode-aware primitive —
/// `char::is_alphanumeric` (used in `regex_allowed_after` above) already
/// classifies a *character* correctly; what a one-byte lookback lacks is
/// knowing where the *previous character* starts, since UTF-8 is
/// variable-width and this cursor only ever looks forward. LiveJournal serves
/// ASCII identifiers, so this gap is not reachable in practice.
fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn starts(script: &str) -> Vec<usize> {
        site_entry_object_starts(script)
    }

    #[test]
    fn finds_the_object_brace_of_an_executable_assignment() {
        let script = r#"Site.entry = {"a":1};"#;
        assert_eq!(starts(script), vec![script.find('{').unwrap()]);
    }

    #[test]
    fn ignores_every_inert_lexical_context() {
        for script in [
            "// Site.entry = {}",
            "/* Site.entry = {} */",
            "'Site.entry = {}'",
            "\"Site.entry = {}\"",
            "`Site.entry = {}`",
            "`a ${`Site.entry = {}`} b`",
            "/Site.entry = \\{\\}/",
            "/[Site.entry = {}]/",
        ] {
            assert!(starts(script).is_empty(), "{script}");
        }
    }

    #[test]
    fn distinguishes_division_from_a_regex_literal() {
        assert_eq!(starts(r#"var q = a / b; Site.entry = {};"#).len(), 1);
        assert_eq!(starts(r#"var r = /a/g; Site.entry = {};"#).len(), 1);
        // A call's closing `)` ends a value, so `/` divides here — unlike
        // the `)` that closes an `if`/`while`/`for` header, which this
        // heuristic cannot distinguish (see the blind spots documented on
        // `regex_allowed_after`).
        assert_eq!(starts(r#"f(1) / 2; Site.entry = {};"#).len(), 1);
    }

    #[test]
    fn documents_known_regex_heuristic_false_accept_fronts() {
        // Pins today's behavior for the two false-accept blind spots
        // documented on `regex_allowed_after`, so a future change to that
        // heuristic can't silently swap which class of false positive exists
        // without a test noticing. Neither is reachable by an attacker (see
        // that doc comment); both are accepted here, not rejected.
        for script in [
            // Keyword-fronted: `return` is misread as a value, so `/` is
            // read as division and the regex body as code.
            r#"function f(){ return /Site.entry = {"a":1}/; }"#,
            // Bracket-fronted: the `)` closing an `if` header is misread the
            // same way a call's closing `)` would be.
            r#"if (a) /Site.entry = {"a":1}/.test(s);"#,
        ] {
            assert_eq!(starts(script).len(), 1, "{script}");
        }
    }

    #[test]
    fn finds_an_assignment_inside_a_template_interpolation() {
        assert_eq!(starts("`a ${ Site.entry = {} } b`").len(), 1);
    }

    #[test]
    fn rejects_member_chains_larger_identifiers_and_comparisons() {
        for script in [
            "Other.Site.entry = {};",
            "window?.Site.entry = {};",
            "MySite.entry = {};",
            "Site.entryPoint = {};",
            "Site.entry == {};",
            "Site.entry === {};",
            "Site.entry => {};",
            "Site.entry != {};",
            "Site.entry += {};",
        ] {
            assert!(starts(script).is_empty(), "{script}");
        }
    }

    #[test]
    fn reports_every_candidate_so_a_bad_one_does_not_hide_a_later_good_one() {
        let script = r#"Site.entry = {bad; Site.entry = {"a":1};"#;
        assert_eq!(starts(script).len(), 2);
    }

    #[test]
    fn fails_closed_past_the_exact_lexical_nesting_limit() {
        let inside = MAX_LEXICAL_NESTING / 2;
        let ok = format!(
            "{}Site.entry = {{}}{}",
            "`${".repeat(inside),
            "}`".repeat(inside)
        );
        assert_eq!(starts(&ok).len(), 1);

        // The boundary is exact, not just "somewhere at or beyond 256": one
        // repetition more than the 128 that just succeeded already fails
        // closed, since each `` `${ `` repetition costs 2 units of nesting
        // (one template, one interpolation) and 128 * 2 = 256 == the limit.
        let first_to_fail = inside + 1;
        let just_over = format!(
            "{}Site.entry = {{}}{}",
            "`${".repeat(first_to_fail),
            "}`".repeat(first_to_fail)
        );
        assert!(starts(&just_over).is_empty());

        let too_deep = MAX_LEXICAL_NESTING + 1;
        let over = format!(
            "{}Site.entry = {{}}{}",
            "`${".repeat(too_deep),
            "}`".repeat(too_deep)
        );
        assert!(starts(&over).is_empty());
    }

    #[test]
    fn resets_regex_allowed_when_a_closed_template_contained_an_interpolation() {
        // A template literal, once closed, is a complete value, so `/` right
        // after should divide — matching how closing a string or regex
        // literal already resets regex_allowed to false. The interpolation's
        // `}` sets regex_allowed to true (a `}` could also end a block), and
        // without also resetting on the closing backtick, that leaks through:
        // the `/` here would be wrongly read as opening a regex, swallowing
        // the rest of the line and hiding the real Site.entry after it.
        assert_eq!(starts("`x${1}y` / 2; Site.entry = {};").len(), 1);
    }

    #[test]
    fn scans_a_large_input_without_recursion_or_quadratic_work() {
        let script = format!(
            "{}Site.entry = {{\"a\":1}};",
            "var x = 'a/b'; // c\n".repeat(50_000)
        );
        assert_eq!(starts(&script).len(), 1);
    }
}
