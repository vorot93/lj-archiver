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

pub(crate) fn neutralize_active_adoc_lines(text: &str) -> String {
    let mut output = String::new();

    for line in text.split_inclusive('\n') {
        let line_without_newline = line.trim_end_matches('\n');
        let trimmed = line_without_newline.trim_start();
        let neutralized_description_markers = neutralize_description_list_markers(trimmed);

        if starts_with_active_adoc_syntax(line_without_newline)
            && (neutralized_description_markers.is_none()
                || starts_with_non_description_active_adoc_syntax(line_without_newline))
        {
            output.push('\\');
        }

        if let Some(neutralized_description_markers) = neutralized_description_markers {
            let leading_len = line_without_newline.len() - trimmed.len();
            output.push_str(&line[..leading_len]);
            output.push_str(&neutralized_description_markers);
            output.push_str(&line[line_without_newline.len()..]);
        } else {
            output.push_str(line);
        }
    }

    output
}

fn neutralize_inline_adoc_syntax(text: &str) -> String {
    let text = neutralize_inline_attribute_references(text);
    let text = neutralize_inline_passthrough_macros(&text);
    let text = neutralize_inline_macros(&text);
    neutralize_inline_passthrough_delimiters(&text)
}

fn neutralize_inline_attribute_references(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::new();
    let mut cursor = 0;

    while cursor < bytes.len() {
        if bytes[cursor] == b'{'
            && !is_immediately_escaped(bytes, cursor)
            && is_attribute_reference_at(bytes, cursor)
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

fn is_attribute_reference_at(bytes: &[u8], start: usize) -> bool {
    let Some(first) = bytes.get(start + 1) else {
        return false;
    };
    if !is_attribute_name_start(*first) {
        return false;
    }

    let mut cursor = start + 2;
    while let Some(byte) = bytes.get(cursor) {
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
    start > 0 && bytes[start - 1] == b'\\'
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

    if start > 0 {
        let previous = bytes[start - 1];
        if previous.is_ascii_alphanumeric() || matches!(previous, b'_' | b'-') {
            return false;
        }
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
        || is_active_delimiter_only(trimmed)
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
    neutralize_plain_adoc_fragment_at_line_start(&normalized, false)
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
            &neutralize_macro_target_block_macro_prefix(&output),
        ),
    )
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

fn ends_with_inline_passthrough_macro_prefix(text: &str) -> bool {
    let Some(pass_start) = text.rfind("pass:") else {
        return false;
    };

    starts_inline_passthrough_macro(&format!("{}[", &text[pass_start..]), 0)
        && (pass_start == 0
            || text.as_bytes()[..pass_start]
                .last()
                .is_some_and(|previous| {
                    !previous.is_ascii_alphanumeric() && !matches!(previous, b'_' | b'-')
                }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
