pub(crate) fn normalize_inline_text(text: &str) -> String {
    let mut output = String::new();
    let mut in_whitespace = false;

    for character in text.chars() {
        if character.is_whitespace() {
            if !in_whitespace {
                output.push(' ');
                in_whitespace = true;
            }
        } else {
            output.push(character);
            in_whitespace = false;
        }
    }

    output
}

pub(crate) fn neutralize_plain_adoc_fragment_at_line_start(
    text: &str,
    is_line_start: bool,
) -> String {
    let text = neutralize_inline_adoc_syntax(text);

    if is_line_start {
        neutralize_active_adoc_lines(&text)
    } else {
        text
    }
}

/// Escapes every line of emitted *content* that Asciidoctor's preprocessor
/// would read as a live directive, in both directive families.
///
/// The preprocessor runs inside every block, `[subs=specialcharacters]\n----`
/// listings and `++++` passthrough alike, so a line at column zero in an
/// archived post body is a live directive when the reader converts the archive:
/// `include::` embeds a local file into the output, and an unmatched conditional
/// discards the rest of the document at exit status 0. Degrading to a listing is
/// *not* a defence — the listing's own content is preprocessed too.
///
/// A leading backslash is Asciidoctor's own escape and is **consumed** on
/// conversion, so the rendered text is unchanged. (A leading space deactivates a
/// directive as well, but stays visible in the output.)
pub(crate) fn neutralize_preprocessor_directive_lines(text: &str) -> String {
    neutralize_directive_lines(text, is_preprocessor_directive_line)
}

/// [`neutralize_preprocessor_directive_lines`] restricted to `include::`, for
/// content emitted inside a `++++` passthrough.
///
/// The conditional family is deliberately **not** escaped here. `Reader#process_line`
/// tests a conditional's escape *before* it tests whether the reader is skipping
/// (verified on 2.0.26), so an escaped `endif::[]` inside a generated
/// `ifdef::lj-archiver-book[]` wrapper survives the skip and emits one line of
/// the *non-selected* variant as a stray paragraph. An escaped `include::` is
/// dropped by the skip correctly, because its branch sits after the skip test.
/// Conditional lines therefore keep failing closed to a listing, where nothing
/// is ever skipped and both families are escaped.
pub(crate) fn neutralize_include_directive_lines(text: &str) -> String {
    neutralize_directive_lines(text, is_include_directive_line)
}

fn neutralize_directive_lines(text: &str, is_directive: fn(&str) -> bool) -> String {
    let mut output = String::with_capacity(text.len());

    for line in text.split_inclusive('\n') {
        if is_directive(line.trim_end_matches(is_asciidoctor_line_padding)) {
            output.push('\\');
        }
        output.push_str(line);
    }

    output
}

fn is_preprocessor_directive_line(line: &str) -> bool {
    is_include_directive_line(line) || is_conditional_directive_line(line)
}

/// Asciidoctor reads a source line through Ruby's `String#rstrip` — regex `\s`
/// plus the NUL — so trailing whitespace never saves a directive line from being
/// recognized. A *bare* carriage return is not a line terminator there and is
/// left alone, which is why lines are split on `\n` only.
fn is_asciidoctor_line_padding(character: char) -> bool {
    character == '\0' || is_directive_regex_space(character)
}

/// Ruby's regex `\s`, which the directive patterns are written in: ASCII only,
/// and it includes the vertical tab but not the NUL. Neither Rust's
/// `char::is_whitespace` (Unicode) nor `char::is_ascii_whitespace` (no vertical
/// tab) is this set, and the difference is load-bearing rather than pedantic: a
/// no-break space is an ordinary target character to Asciidoctor, so
/// `include::&nbsp;secret[]` — period LiveJournal HTML is full of `&nbsp;` — is
/// a live directive that a Unicode-whitespace test would wave through.
fn is_directive_regex_space(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ')
}

/// Asciidoctor's `IncludeDirectiveRx`:
/// `^(\\)?include::([^\s\[](?:[^\[]*[^\s\[])?)\[(.+)?\]$`. The target is
/// non-empty, holds no `[`, and neither starts nor ends with whitespace —
/// though it may contain some.
///
/// The regex's optional leading backslash is the escape this module inserts, so
/// a line that already carries one simply does not match here. That is also what
/// keeps an already-escaped line from being escaped a second time, which would
/// put a visible backslash into the converted output.
fn is_include_directive_line(line: &str) -> bool {
    let Some(target) = line
        .strip_prefix("include::")
        .and_then(directive_argument_target)
    else {
        return false;
    };

    !target.is_empty()
        && !target.starts_with(is_directive_regex_space)
        && !target.ends_with(is_directive_regex_space)
}

/// Asciidoctor's `ConditionalDirectiveRx`:
/// `^(\\)?(ifdef|ifndef|ifeval|endif)::(\S*?(?:([,+])\S*?)?)\[(.+)?\]$`. The
/// target alternation accepts exactly a run of non-whitespace, the empty run
/// included — `endif::[]` and `ifeval::[expr]` carry no target at all.
fn is_conditional_directive_line(line: &str) -> bool {
    const NAMES: [&str; 4] = ["ifdef", "ifndef", "ifeval", "endif"];

    NAMES.iter().any(|name| {
        line.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix("::"))
            .and_then(directive_argument_target)
            .is_some_and(|target| !target.contains(is_directive_regex_space))
    })
}

/// The directive target: everything before the `[` opening the trailing
/// `[...]` argument, or `None` when the line carries no such argument. Both
/// directive shapes exclude `[` from the target and anchor the closing `]` at
/// the end of the line, so the *first* `[` is the only candidate opener.
fn directive_argument_target(rest: &str) -> Option<&str> {
    let open = rest.find('[')?;

    (rest.ends_with(']') && open + 1 < rest.len()).then(|| &rest[..open])
}

pub(crate) fn neutralize_active_adoc_lines(text: &str) -> String {
    let mut output = String::new();

    for line in text.split_inclusive('\n') {
        let line_without_newline = line.trim_end_matches('\n');
        let trimmed = line_without_newline.trim_start();
        let neutralized_description_markers = neutralize_description_list_markers(trimmed);

        if is_complete_block_metadata_line(trimmed) {
            let leading_len = line_without_newline.len() - trimmed.len();
            output.push_str(&line[..leading_len]);
            output.push('\\');
            output.push_str(&line[leading_len..]);
            continue;
        }

        let protect_active_syntax = starts_with_active_adoc_syntax(line_without_newline)
            && (neutralized_description_markers.is_none()
                || starts_with_non_description_active_adoc_syntax(line_without_newline));

        if let Some(neutralized_description_markers) = neutralized_description_markers {
            let leading_len = line_without_newline.len() - trimmed.len();
            output.push_str(&line[..leading_len]);
            if protect_active_syntax {
                output.push('\\');
            }
            output.push_str(&neutralized_description_markers);
            output.push_str(&line[line_without_newline.len()..]);
        } else if protect_active_syntax {
            let leading_len = line_without_newline.len() - trimmed.len();
            output.push_str(&line[..leading_len]);
            let protected = &line[leading_len..];
            if leading_len > 0 && starts_with_additional_block_construct(trimmed) {
                // Appending at a generated line start trims the retained space.
                // Encode the first marker glyph so it stays inert without
                // exposing a backslash after AsciiDoc conversion.
                let first = protected.chars().next().expect("active syntax is nonempty");
                output.push_str(&format!("&#{};", u32::from(first)));
                output.push_str(&protected[first.len_utf8()..]);
            } else {
                output.push('\\');
                output.push_str(protected);
            }
        } else {
            output.push_str(line);
        }
    }

    output
}

fn is_complete_block_metadata_line(text: &str) -> bool {
    let trimmed = text.trim_end();

    trimmed.starts_with('[') && trimmed.ends_with(']')
}

fn neutralize_inline_adoc_syntax(text: &str) -> String {
    let text = neutralize_inline_attribute_references(text);
    let text = neutralize_inline_passthrough_macros(&text);
    let text = neutralize_inline_macros(&text);
    neutralize_inline_passthrough_delimiters(&text)
}

fn neutralize_inline_attribute_references(text: &str) -> String {
    let mut scan_steps = AttributeScanSteps::default();
    neutralize_inline_attribute_references_scanned(text, &mut scan_steps)
}

#[cfg(test)]
fn neutralize_inline_attribute_references_with_scan_steps(text: &str) -> (String, usize) {
    let mut scan_steps = AttributeScanSteps::default();
    let output = neutralize_inline_attribute_references_scanned(text, &mut scan_steps);
    (output, scan_steps.count)
}

fn neutralize_inline_attribute_references_scanned(
    text: &str,
    scan_steps: &mut AttributeScanSteps,
) -> String {
    let bytes = text.as_bytes();
    // Special set/counter forms may contain arbitrary values. A reverse pass
    // makes their same-line closing-brace check O(1) per opening candidate.
    let closing_brace_before_line_break = closing_brace_before_line_break(bytes, scan_steps);
    let mut output = String::new();
    let mut cursor = 0;

    while cursor < bytes.len() {
        scan_steps.add(1);
        if bytes[cursor] == b'{'
            && !is_immediately_escaped(bytes, cursor)
            && is_attribute_reference_at(
                bytes,
                cursor,
                &closing_brace_before_line_break,
                scan_steps,
            )
        {
            output.push('\\');
        }

        let character = text[cursor..]
            .chars()
            .next()
            .expect("cursor should remain on a character boundary");
        output.push(character);
        cursor += character.len_utf8();
    }

    output
}

fn is_attribute_reference_at(
    bytes: &[u8],
    start: usize,
    closing_brace_before_line_break: &[bool],
    scan_steps: &mut AttributeScanSteps,
) -> bool {
    if is_special_attribute_reference_at(bytes, start, closing_brace_before_line_break, scan_steps)
    {
        return true;
    }

    let Some(first) = bytes.get(start + 1) else {
        return false;
    };
    if !is_attribute_name_start(*first) {
        return false;
    }

    let mut cursor = start + 2;
    while let Some(byte) = bytes.get(cursor) {
        scan_steps.add(1);
        if *byte == b'}' {
            return true;
        }
        if !is_attribute_name_byte(*byte) {
            return false;
        }
        cursor += 1;
    }

    false
}

fn is_special_attribute_reference_at(
    bytes: &[u8],
    start: usize,
    closing_brace_before_line_break: &[bool],
    scan_steps: &mut AttributeScanSteps,
) -> bool {
    const SPECIAL_PREFIXES: [&[u8]; 3] = [b"set:", b"counter:", b"counter2:"];
    let content = &bytes[start + 1..];

    SPECIAL_PREFIXES.iter().any(|prefix| {
        scan_steps.add(1);
        content.starts_with(prefix) && closing_brace_before_line_break[start + 1 + prefix.len()]
    })
}

fn closing_brace_before_line_break(bytes: &[u8], scan_steps: &mut AttributeScanSteps) -> Vec<bool> {
    let mut result = vec![false; bytes.len() + 1];
    let mut closing_brace_is_available = false;

    for (cursor, byte) in bytes.iter().enumerate().rev() {
        scan_steps.add(1);
        match *byte {
            b'\n' | b'\r' => closing_brace_is_available = false,
            b'}' => closing_brace_is_available = true,
            _ => {}
        }
        result[cursor] = closing_brace_is_available;
    }

    result
}

#[derive(Default)]
struct AttributeScanSteps {
    #[cfg(test)]
    count: usize,
}

impl AttributeScanSteps {
    fn add(&mut self, count: usize) {
        #[cfg(test)]
        {
            self.count += count;
        }
        #[cfg(not(test))]
        let _ = count;
    }
}

fn is_attribute_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_attribute_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
}

fn neutralize_inline_macros(text: &str) -> String {
    let mut scan_steps = MacroScanSteps::default();
    neutralize_inline_macros_scanned(text, &mut scan_steps)
}

pub(crate) fn strip_obsolete_rendered_macro_protection(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut scan_steps = MacroScanSteps::default();
    let bracket_before_whitespace = bracket_before_whitespace(bytes, &mut scan_steps);
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;

    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            let macro_start = cursor + 1;

            if bytes.get(macro_start).is_some_and(u8::is_ascii_alphabetic) {
                let name_end = inline_macro_name_end(bytes, macro_start, &mut scan_steps);
                let is_macro = is_inline_macro_token_boundary(bytes, cursor)
                    && is_inline_macro_at(bytes, macro_start, name_end, &bracket_before_whitespace);
                let is_passthrough = is_inline_passthrough_token_boundary(bytes, cursor)
                    && text[macro_start..].starts_with("pass:")
                    && starts_inline_passthrough_macro(text, macro_start);

                if is_macro || is_passthrough {
                    // Authored literal backslashes are rendered as `&#92;`.
                    // A raw slash guarding a recognized rendered macro is
                    // obsolete once xref-label normalization removes its
                    // bracket delimiters and makes the macro inert.
                    cursor += 1;
                    continue;
                }
            }
        }

        let character = text[cursor..]
            .chars()
            .next()
            .expect("cursor should remain on a character boundary");
        output.push(character);
        cursor += character.len_utf8();
    }

    output
}

#[cfg(test)]
fn neutralize_inline_macros_with_scan_steps(text: &str) -> (String, usize) {
    let mut scan_steps = MacroScanSteps::default();
    let output = neutralize_inline_macros_scanned(text, &mut scan_steps);
    (output, scan_steps.count)
}

fn neutralize_inline_macros_scanned(text: &str, scan_steps: &mut MacroScanSteps) -> String {
    let bytes = text.as_bytes();
    // One reverse pass makes each candidate's "`[` before whitespace" target
    // check O(1); token-boundary name runs do not overlap in the forward pass.
    let bracket_before_whitespace = bracket_before_whitespace(bytes, scan_steps);
    let mut output = String::new();
    let mut cursor = 0;

    while cursor < bytes.len() {
        scan_steps.add(1);

        if bytes[cursor].is_ascii_alphabetic() && is_inline_macro_token_boundary(bytes, cursor) {
            let name_end = inline_macro_name_end(bytes, cursor, scan_steps);
            scan_steps.add(1);

            if is_inline_macro_at(bytes, cursor, name_end, &bracket_before_whitespace)
                && !is_immediately_escaped(bytes, cursor)
            {
                output.push('\\');
            }
        }

        let character = text[cursor..]
            .chars()
            .next()
            .expect("cursor should remain on a character boundary");
        output.push(character);
        cursor += character.len_utf8();
    }

    output
}

#[derive(Default)]
struct MacroScanSteps {
    #[cfg(test)]
    count: usize,
}

impl MacroScanSteps {
    fn add(&mut self, count: usize) {
        #[cfg(test)]
        {
            self.count += count;
        }
        #[cfg(not(test))]
        let _ = count;
    }
}

fn bracket_before_whitespace(bytes: &[u8], scan_steps: &mut MacroScanSteps) -> Vec<bool> {
    let mut result = vec![false; bytes.len() + 1];
    let mut bracket_is_next = false;

    for (cursor, byte) in bytes.iter().enumerate().rev() {
        scan_steps.add(1);
        if *byte == b'[' {
            bracket_is_next = true;
        } else if byte.is_ascii_whitespace() {
            bracket_is_next = false;
        }
        result[cursor] = bracket_is_next;
    }

    result
}

fn inline_macro_name_end(bytes: &[u8], start: usize, scan_steps: &mut MacroScanSteps) -> usize {
    let mut cursor = start;

    while matches!(bytes.get(cursor), Some(byte) if is_adoc_macro_name_byte(*byte)) {
        scan_steps.add(1);
        cursor += 1;
    }

    cursor
}

fn is_inline_macro_token_boundary(bytes: &[u8], start: usize) -> bool {
    start == 0
        || !bytes[start - 1].is_ascii_alphanumeric()
            && !matches!(bytes[start - 1], b'_' | b'.' | b'-')
}

fn is_inline_macro_at(
    bytes: &[u8],
    start: usize,
    name_end: usize,
    bracket_before_whitespace: &[bool],
) -> bool {
    if bytes.get(name_end) != Some(&b':') {
        return false;
    }

    let target_start = name_end + 1;
    if bytes.get(target_start) == Some(&b'[') {
        return is_targetless_builtin_macro(&bytes[start..name_end]);
    }

    bracket_before_whitespace
        .get(target_start)
        .copied()
        .unwrap_or(false)
}

fn is_targetless_builtin_macro(name: &[u8]) -> bool {
    matches!(name, b"footnote" | b"kbd" | b"btn")
}

fn is_immediately_escaped(bytes: &[u8], start: usize) -> bool {
    bytes[..start]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        == 1
}

fn neutralize_inline_passthrough_macros(text: &str) -> String {
    let mut output = String::new();
    let mut cursor = 0;

    while let Some(relative_start) = text[cursor..].find("pass:") {
        let start = cursor + relative_start;

        if starts_inline_passthrough_macro(text, start)
            && !is_immediately_escaped(text.as_bytes(), start)
        {
            let next = start + "pass:".len();
            output.push_str(&text[cursor..start]);
            output.push('\\');
            output.push_str(&text[start..next]);
            cursor = next;
        } else {
            let next = start + "pass:".len();
            output.push_str(&text[cursor..next]);
            cursor = next;
        }
    }

    output.push_str(&text[cursor..]);
    output
}

fn starts_inline_passthrough_macro(text: &str, start: usize) -> bool {
    let bytes = text.as_bytes();

    if !is_inline_passthrough_token_boundary(bytes, start) {
        return false;
    }

    let mut cursor = start + "pass:".len();

    if bytes.get(cursor) == Some(&b'[') {
        return true;
    }

    let qualifier_start = cursor;
    while matches!(bytes.get(cursor), Some(byte) if is_inline_passthrough_qualifier_byte(*byte)) {
        cursor += 1;
    }

    cursor > qualifier_start && bytes.get(cursor) == Some(&b'[')
}

fn is_inline_passthrough_token_boundary(bytes: &[u8], start: usize) -> bool {
    start == 0
        || !bytes[start - 1].is_ascii_alphanumeric() && !matches!(bytes[start - 1], b'_' | b'-')
}

fn is_inline_passthrough_qualifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b',' | b'_' | b'-')
}

fn neutralize_inline_passthrough_delimiters(text: &str) -> String {
    let mut output = String::new();
    let mut cursor = 0;

    while let Some(relative_start) = text[cursor..].find("+++") {
        let start = cursor + relative_start;
        let content_start = start + 3;
        let Some(relative_end) = text[content_start..].find("+++") else {
            break;
        };
        let end = content_start + relative_end + 3;

        output.push_str(&text[cursor..start]);
        if is_immediately_escaped(text.as_bytes(), start) {
            output.push_str("+++");
        } else {
            output.push_str(r"\+++");
        }
        output.push_str(&text[content_start..end]);
        cursor = end;
    }

    output.push_str(&text[cursor..]);
    output
}

fn starts_with_active_adoc_syntax(text: &str) -> bool {
    starts_with_non_description_active_adoc_syntax(text)
        || starts_with_description_list_marker(text.trim_start())
}

fn starts_with_non_description_active_adoc_syntax(text: &str) -> bool {
    let trimmed = text.trim_start();

    starts_with_block_macro(trimmed)
        || starts_with_document_attribute(trimmed)
        || starts_with_heading(trimmed)
        || starts_with_list_marker(trimmed)
        || starts_with_admonition_label(trimmed)
        || starts_with_line_comment(trimmed)
        || starts_with_hyphen_list_marker(trimmed)
        || starts_with_additional_block_construct(trimmed)
        || is_active_delimiter_only(trimmed)
}

fn starts_with_additional_block_construct(text: &str) -> bool {
    let trimmed = text.trim();
    matches!(
        trimmed,
        "<<<" | "'''" | "--" | "|===" | ",===" | ":===" | "!==="
    ) || trimmed
        .strip_prefix('.')
        .and_then(|rest| rest.chars().next())
        .is_some_and(|character| !character.is_whitespace())
}

fn starts_with_block_macro(text: &str) -> bool {
    let Some(macro_separator) = text.find("::") else {
        return false;
    };

    if macro_separator == 0 {
        return false;
    }

    text[..macro_separator]
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.'))
        && text[macro_separator + 2..].contains('[')
}

fn starts_with_document_attribute(text: &str) -> bool {
    let Some(rest) = text.strip_prefix(':') else {
        return false;
    };

    rest.chars()
        .next()
        .is_some_and(|character| !character.is_whitespace())
        && rest.contains(':')
}

fn starts_with_heading(text: &str) -> bool {
    let equals_count = text
        .chars()
        .take_while(|character| *character == '=')
        .count();

    equals_count > 0 && text.as_bytes().get(equals_count) == Some(&b' ')
}

fn starts_with_list_marker(text: &str) -> bool {
    let Some(marker) = text.as_bytes().first() else {
        return false;
    };

    if !matches!(marker, b'*' | b'.') {
        return false;
    }

    let marker_count = text.bytes().take_while(|byte| byte == marker).count();

    text.as_bytes()
        .get(marker_count)
        .is_some_and(|byte| byte.is_ascii_whitespace())
}

fn starts_with_admonition_label(text: &str) -> bool {
    const ADMONITION_LABELS: [&str; 5] = ["CAUTION", "IMPORTANT", "NOTE", "TIP", "WARNING"];

    ADMONITION_LABELS.iter().any(|label| {
        text.strip_prefix(label)
            .and_then(|rest| rest.strip_prefix(':'))
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    })
}

fn starts_with_line_comment(text: &str) -> bool {
    text.starts_with("//")
}

fn starts_with_hyphen_list_marker(text: &str) -> bool {
    text.strip_prefix('-')
        .and_then(|rest| rest.chars().next())
        .is_some_and(char::is_whitespace)
}

fn starts_with_description_list_marker(text: &str) -> bool {
    description_list_marker_position(text).is_some()
}

fn description_list_marker_position(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut has_term = false;
    let mut cursor = 0;

    while cursor < bytes.len() {
        if bytes.get(cursor..cursor + 2) == Some(b"::")
            && has_term
            && bytes.get(cursor + 2).is_none_or(u8::is_ascii_whitespace)
        {
            return Some(cursor);
        }
        has_term |= !bytes[cursor].is_ascii_whitespace();
        cursor += 1;
    }

    None
}

fn neutralize_description_list_markers(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut output = String::new();
    let mut has_term = false;
    let mut cursor = 0;
    let mut copied_until = 0;
    let mut changed = false;

    while cursor < bytes.len() {
        if bytes.get(cursor..cursor + 2) == Some(b"::")
            && has_term
            && bytes.get(cursor + 2).is_none_or(u8::is_ascii_whitespace)
        {
            output.push_str(&text[copied_until..cursor + 1]);
            output.push_str("pass:[:]");
            copied_until = cursor + 2;
            changed = true;
            cursor += 2;
            continue;
        }
        has_term |= !bytes[cursor].is_ascii_whitespace();
        cursor += 1;
    }

    if changed {
        output.push_str(&text[copied_until..]);
        Some(output)
    } else {
        None
    }
}

fn is_delimiter_only(text: &str, delimiter: char) -> bool {
    text.chars().count() >= 4 && text.chars().all(|character| character == delimiter)
}

fn is_active_delimiter_only(text: &str) -> bool {
    let Some(delimiter) = text.chars().next() else {
        return false;
    };

    matches!(delimiter, '+' | '-' | '.' | '*' | '=' | '_' | '|' | '/')
        && is_delimiter_only(text, delimiter)
}

pub(crate) fn render_inline_wrapper(text: &str, prefix: &str, suffix: &str) -> String {
    if should_preserve_unwrapped_inline_output(text) {
        return text.to_string();
    }

    render_inline_wrapper_with_label(text, text.trim(), prefix, suffix)
}

pub(crate) fn render_inline_wrapper_with_label(
    text: &str,
    label: &str,
    prefix: &str,
    suffix: &str,
) -> String {
    let mut output = String::new();

    if label.is_empty() {
        return output;
    }

    if text.starts_with(' ') {
        output.push(' ');
    }

    output.push_str(prefix);
    output.push_str(label);
    output.push_str(suffix);

    if text.ends_with(' ') {
        output.push(' ');
    }

    output
}

pub(crate) fn should_preserve_unwrapped_inline_output(text: &str) -> bool {
    let trimmed = text.trim();

    !trimmed.is_empty()
        && (text.contains("\n\n")
            || trimmed
                .lines()
                .any(|line| is_delimiter_only(line, '+') || is_delimiter_only(line, '-')))
}

pub(crate) fn sanitize_macro_label(text: &str) -> String {
    let normalized = normalize_inline_text(&text.replace(['[', ']', '\n', '\r'], " "))
        .trim()
        .to_string();
    neutralize_plain_adoc_fragment_at_line_start(&encode_macro_label_delimiters(&normalized), false)
}

pub(crate) fn sanitize_rendered_macro_label(text: &str) -> String {
    let normalized = normalize_inline_text(&text.replace(['[', ']', '\n', '\r'], " "))
        .trim()
        .to_string();
    let trailing_backslashes = normalized
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'\\')
        .count();
    let safe_prefix_len = normalized.len() - trailing_backslashes;
    let mut encoded = normalized[..safe_prefix_len].replace(',', "&#44;");

    for _ in 0..trailing_backslashes {
        encoded.push_str("&#92;");
    }

    neutralize_plain_adoc_fragment_at_line_start(&encoded, false)
}

pub(crate) fn encode_authored_backslashes(text: &str) -> String {
    text.replace('\\', "&#92;")
}

fn encode_macro_label_delimiters(text: &str) -> String {
    let mut output = String::with_capacity(text.len());

    for character in text.chars() {
        match character {
            ',' => output.push_str("&#44;"),
            '\\' => output.push_str("&#92;"),
            character => output.push(character),
        }
    }

    output
}

pub(crate) fn sanitize_macro_target(text: &str) -> String {
    let mut output = String::new();

    for character in text.trim().chars() {
        match character {
            '[' => output.push_str("%5B"),
            ']' => output.push_str("%5D"),
            '\n' => output.push_str("%0A"),
            '\r' => output.push_str("%0D"),
            '\t' => output.push_str("%09"),
            character if character.is_control() => {
                for byte in character.to_string().as_bytes() {
                    output.push_str(&format!("%{byte:02X}"));
                }
            }
            character => output.push(character),
        }
    }

    neutralize_macro_target_inline_passthrough_prefix(
        &neutralize_macro_target_inline_passthrough_delimiters(
            &neutralize_macro_target_block_macro_prefix(&neutralize_macro_target_leading_colon(
                &output,
            )),
        ),
    )
}

/// A target beginning with `:` composes a spurious colon run once spliced
/// into a macro invocation: an inline image's single-colon `image:` prefix
/// plus a `:foo` target reads as the block-macro prefix `image::`, and a
/// bare image's `image::` prefix plus the same target reads as the malformed
/// `image:::`. Percent-encoding only the leading colon breaks that
/// composition while every other colon in the target — a URL scheme, a port,
/// or anything past position zero — stays untouched, so ordinary absolute
/// targets are unaffected.
fn neutralize_macro_target_leading_colon(text: &str) -> String {
    match text.strip_prefix(':') {
        Some(rest) => format!("%3A{rest}"),
        None => text.to_string(),
    }
}

fn neutralize_macro_target_block_macro_prefix(text: &str) -> String {
    let Some(separator) = text.find("::") else {
        return text.to_string();
    };

    if separator == 0 || !text[..separator].bytes().all(is_adoc_macro_name_byte) {
        return text.to_string();
    }

    format!("{}%3A%3A{}", &text[..separator], &text[separator + 2..])
}

fn neutralize_macro_target_inline_passthrough_prefix(text: &str) -> String {
    if starts_inline_passthrough_macro(&format!("{text}["), 0) {
        text.replacen(':', "%3A", 1)
    } else {
        text.to_string()
    }
}

fn neutralize_macro_target_inline_passthrough_delimiters(text: &str) -> String {
    let mut output = String::new();
    let mut cursor = 0;

    while let Some(relative_start) = text[cursor..].find("+++") {
        let start = cursor + relative_start;
        let content_start = start + 3;
        let Some(relative_end) = text[content_start..].find("+++") else {
            break;
        };
        let end = content_start + relative_end + 3;

        output.push_str(&text[cursor..start]);
        output.push_str("%2B%2B%2B");
        output.push_str(&text[content_start..end - 3]);
        output.push_str("%2B%2B%2B");
        cursor = end;
    }

    output.push_str(&text[cursor..]);
    output
}

fn is_adoc_macro_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

/// A trusted empty inline passthrough. Inserted only by the renderer at a
/// logical line start, it changes AsciiDoc's parsing context for that line
/// without escaping or exposing the renderer-owned formatting delimiters that
/// follow it. Authored `pass:` macros remain neutralized by
/// [`neutralize_inline_passthrough_macros`], so this can never be forged from
/// post text.
pub(crate) const GENERATED_INLINE_LINE_GUARD: &str = "pass:[]";

/// True when appending renderer-owned inline output turned an inert logical
/// line into one AsciiDoc parses as active block syntax.
///
/// `before` is the current logical line as it stood before the append;
/// `composed` is the same line afterwards. A line that was *already* active
/// keeps whatever protection it already carries — only a newly completed
/// construct earns a guard, so safe prose never gains one.
pub(crate) fn generated_inline_completes_active_line(
    before: &str,
    composed: &str,
    fragment: &str,
) -> bool {
    // A bare `<img>` renders to a genuine block macro (`image::src[alt]`).
    // Standing alone on its line that is the renderer's own deliberate output,
    // not an authored prefix activating generated delimiters — guarding it
    // would convert a working image into literal text. This is the *only*
    // exemption: a generated fragment that merely happens to start a line
    // (an anchor whose target begins with `.`, say) still earns its guard.
    if before.trim().is_empty() && fragment.trim_start().starts_with("image::") {
        return false;
    }

    !line_is_active_adoc_syntax(before) && line_is_active_adoc_syntax(composed)
}

/// Neutralizes a logical line that an appended run of *authored* text newly
/// turned into active block syntax, returning the replacement for the whole
/// line, or `None` when the line needs no change.
///
/// [`neutralize_active_adoc_lines`] only ever sees a run that starts a line, so
/// it cannot see this: a generated inline fragment splits an authored run in
/// two, and neither half starts the line the two halves compose
/// (`<p>[pass<i></i>]</p>` composes the block-metadata line `[pass]` out of two
/// inert halves). A line that was *already* active keeps whatever protection it
/// already carries, so an escaped line is never escaped twice.
///
/// Two remedies, because neither covers the other's construct:
///
/// * [`GENERATED_INLINE_LINE_GUARD`] at the line start deactivates every
///   construct AsciiDoc anchors at column zero — block metadata, headings, list
///   markers, admonition labels, block macros (`include::` among them, which is
///   also a live preprocessor directive), document attributes, block titles and
///   delimiter lines.
/// * A description-list marker is positional rather than anchored, so the guard
///   does not reach it: `pass:[]term:: def` still converts to a `<dl>` (verified
///   on Asciidoctor 2.0.26). The marker itself is neutralized instead, exactly
///   as the line-start pass neutralizes the same authored text.
///
/// Rewriting the composed line is safe precisely because `before` was inert: a
/// marker needs `::` followed by whitespace or the line end, and appending only
/// ever extends the line, so any `::` already on the line that is followed by a
/// non-space stays disqualified. Every marker this rewrites therefore owns at
/// least one colon from the authored text just appended.
pub(crate) fn neutralize_line_completed_by_authored_text(
    before: &str,
    composed: &str,
) -> Option<String> {
    if line_is_active_adoc_syntax(before) || !line_is_active_adoc_syntax(composed) {
        return None;
    }

    let neutralized = neutralize_description_list_markers(composed);
    let line = neutralized.as_deref().unwrap_or(composed);

    if line_is_anchored_active_adoc_syntax(line) {
        return Some(format!("{GENERATED_INLINE_LINE_GUARD}{line}"));
    }

    neutralized
}

/// The full active-line vocabulary: headings, ordered/unordered/hyphen/
/// description lists, admonitions, block macros, document attributes, line
/// comments, block titles, page and thematic breaks, open/table block
/// delimiters, every delimiter-only line, and complete `[...]` block metadata.
fn line_is_active_adoc_syntax(line: &str) -> bool {
    line_is_anchored_active_adoc_syntax(line)
        || starts_with_description_list_marker(line.trim_start())
}

/// The part of that vocabulary AsciiDoc anchors at the start of a line, and so
/// exactly the part a line-start guard deactivates.
fn line_is_anchored_active_adoc_syntax(line: &str) -> bool {
    is_complete_block_metadata_line(line.trim_start())
        || starts_with_non_description_active_adoc_syntax(line)
}

pub(crate) fn neutralize_pending_plain_text_before_generated_inline(output: &mut String) {
    if output.is_empty() {
        return;
    }

    let line_start = output.rfind('\n').map_or(0, |index| index + 1);
    let line = &output[line_start..];

    if let Some(prefix) = line.strip_suffix(":pass:[:]") {
        let prefix = prefix.to_string();
        if is_block_macro_prefix(&prefix) {
            // This call site is only reached immediately before appending a
            // renderer-generated inline fragment. That fragment makes `::`
            // ineligible as a description-list delimiter; preserve the
            // existing escaped macro-prefix output without applying this to
            // user text before a line break.
            output.truncate(line_start);
            output.push('\\');
            output.push_str(&prefix);
            output.push_str("::");
            return;
        }
    }

    if ends_with_block_macro_prefix(line) || ends_with_inline_passthrough_macro_prefix(line) {
        output.insert(line_start, '\\');
    }
}

fn ends_with_block_macro_prefix(text: &str) -> bool {
    let Some(prefix) = text.strip_suffix("::") else {
        return false;
    };

    is_block_macro_prefix(prefix)
}

fn is_block_macro_prefix(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
}

/// True when `text` ends with an inline passthrough macro prefix — `pass:`, or
/// `pass:` plus a qualifier list — that the `[` opening the generated fragment
/// about to be appended would complete.
///
/// The prefix must run to the *end* of the line. A `pass:` macro that already
/// closed earlier on it (the code renderer's own `` `pass:c[x]` ``, the line
/// guard, or a neutralized description-list marker) is complete rather than
/// dangling, and escaping the line for one puts a visible backslash in front of
/// ordinary prose.
fn ends_with_inline_passthrough_macro_prefix(text: &str) -> bool {
    let Some(pass_start) = text.rfind("pass:") else {
        return false;
    };

    is_inline_passthrough_token_boundary(text.as_bytes(), pass_start)
        && text[pass_start + "pass:".len()..]
            .bytes()
            .all(is_inline_passthrough_qualifier_byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case here was checked against Asciidoctor 2.0.26 by putting the
    /// line inside a `[subs=specialcharacters]` listing and seeing whether the
    /// directive fired. The matcher mirrors `IncludeDirectiveRx` and
    /// `ConditionalDirectiveRx`, so a target may hold internal whitespace and
    /// even a `[`, and trailing whitespace does not save a line — Asciidoctor
    /// reads its source through `String#rstrip`.
    #[test]
    fn escapes_exactly_the_lines_asciidoctor_reads_as_directives() {
        for directive in [
            "include::secret.txt[]",
            "include::/etc/hostname[]",
            "include::secret.txt[leveloffset=+1]",
            "include::my file.txt[]",
            "include::sec[ret.txt[]",
            "include::secret.txt[]]",
            "include::secret.txt[]   ",
            "include::secret.txt[]\t",
            "ifdef::never[]",
            "ifndef::never[]",
            "ifeval::[1==1]",
            "endif::[]",
            "ifdef::a,b[]",
            "ifdef::a+b[]",
            // Ruby's `\s` is ASCII-only, so a no-break space is an ordinary
            // target character — and `&nbsp;` is everywhere in period HTML.
            "include::\u{a0}secret.txt[]",
            "include::secret.txt\u{a0}[]",
            "ifdef::a\u{a0}b[]",
            "endif::\u{a0}[]",
        ] {
            assert_eq!(
                neutralize_preprocessor_directive_lines(directive),
                format!("\\{directive}"),
                "{directive:?} was left live"
            );
        }
    }

    #[test]
    fn leaves_every_line_asciidoctor_does_not_read_as_a_directive_alone() {
        for inert in [
            // Leading whitespace deactivates both families outright.
            " include::secret.txt[]",
            "\tinclude::secret.txt[]",
            "\tifdef::never[]",
            // The target may not begin or end with whitespace.
            "include:: secret.txt[]",
            "include::secret.txt []",
            // The `[...]` argument must close the line.
            "include::secret.txt[] tail",
            "include::secret.txt[]x",
            "include::secret.txt",
            "endif::[",
            // A conditional target is a run of non-whitespace — and the
            // vertical tab counts, where the no-break space above does not.
            "ifdef::a b[]",
            "ifdef::a\u{b}b[]",
            "include::\u{b}secret.txt[]",
            // An include target is never empty; a directive name is exact.
            "include::[]",
            "INCLUDE::secret.txt[]",
            "ifdefx::a[]",
            "xinclude::secret.txt[]",
            "the include::secret.txt[] macro reads a file",
            "#ifdef _WIN32",
            // Already inert: a second backslash would become visible output.
            "\\include::secret.txt[]",
            "\\endif::[]",
            "\\\\include::secret.txt[]",
        ] {
            assert_eq!(
                neutralize_preprocessor_directive_lines(inert),
                inert,
                "{inert:?} was rewritten"
            );
        }
    }

    /// Line structure is preserved exactly: the escape goes at column zero of
    /// the offending line and nothing else moves. A CRLF pair is one line break
    /// to Asciidoctor, while a bare carriage return is ordinary content.
    #[test]
    fn escapes_in_place_without_disturbing_line_structure() {
        assert_eq!(
            neutralize_preprocessor_directive_lines("a\ninclude::x[]\nb\n"),
            "a\n\\include::x[]\nb\n"
        );
        assert_eq!(
            neutralize_preprocessor_directive_lines("a\r\ninclude::x[]\r\nb"),
            "a\r\n\\include::x[]\r\nb"
        );
        assert_eq!(
            neutralize_preprocessor_directive_lines("a\rinclude::x[]\rb"),
            "a\rinclude::x[]\rb"
        );
        assert_eq!(neutralize_preprocessor_directive_lines(""), "");
    }

    /// The passthrough entry point covers `include::` only; the conditional
    /// family fails closed to a listing instead, because an escaped conditional
    /// survives a skipped region and would leak a line of the non-selected
    /// variant out of a generated conditional.
    #[test]
    fn passthrough_escaping_covers_includes_only() {
        assert_eq!(
            neutralize_include_directive_lines("include::x[]\nendif::[]\nifdef::never[]"),
            "\\include::x[]\nendif::[]\nifdef::never[]"
        );
    }

    #[test]
    fn neutralizes_page_theme_title_open_and_table_block_lines() {
        for authored in [
            "<<<",
            "'''",
            ".Authored title",
            "--",
            "|===",
            ",===",
            ":===",
            "!===",
        ] {
            assert_eq!(
                neutralize_plain_adoc_fragment_at_line_start(authored, true),
                format!(r"\{authored}")
            );
        }
    }

    #[test]
    fn places_active_line_protection_after_retained_leading_whitespace() {
        assert_eq!(
            neutralize_plain_adoc_fragment_at_line_start(" <<< ", true),
            " &#60;<< "
        );
        assert_eq!(
            neutralize_plain_adoc_fragment_at_line_start("<<<", true),
            "\\<<<"
        );
    }

    #[test]
    fn incomplete_special_attribute_candidates_have_linear_scan_work() {
        let candidates = 25_000;
        let input = "{set:".repeat(candidates);

        let (actual, scan_steps) = neutralize_inline_attribute_references_with_scan_steps(&input);

        assert_eq!(actual, input);
        assert!(
            scan_steps <= 4 * input.len(),
            "attribute scanner took {scan_steps} steps for {} input bytes",
            input.len()
        );
    }

    #[test]
    fn overlapping_macro_candidates_have_linear_scan_work() {
        let candidates = 25_000;
        let input = format!("{}z[]", "a:".repeat(candidates));
        let expected = format!("{}z[]", "\\a:".repeat(candidates));

        let (actual, scan_steps) = neutralize_inline_macros_with_scan_steps(&input);

        assert_eq!(actual, expected);
        assert!(
            scan_steps <= 4 * input.len(),
            "macro scanner took {scan_steps} steps for {} input bytes",
            input.len()
        );
    }
}
