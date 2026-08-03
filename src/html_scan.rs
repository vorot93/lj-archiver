use html5ever::{
    tendril::StrTendril,
    tokenizer::{BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer},
};
use std::{
    cell::RefCell,
    collections::{HashMap, hash_map::Entry},
    ops::Range,
};

#[derive(Debug, thiserror::Error)]
#[error("could not align HTML token {token_index}: {reason}")]
pub(crate) struct ScanError {
    token_index: usize,
    reason: String,
}

#[derive(Debug)]
pub(crate) struct ScannedHtml {
    start_tags: Vec<ScannedStartTag>,
    element_spans: HashMap<String, Vec<Range<usize>>>,
}

#[derive(Debug)]
#[allow(dead_code)] // The complete scanner API is consumed by staged caller migrations.
pub(crate) struct ScannedStartTag {
    name: String,
    source_range: Range<usize>,
    attributes: Vec<ScannedAttribute>,
}

#[derive(Debug)]
#[allow(dead_code)] // The complete scanner API is consumed by staged caller migrations.
pub(crate) struct ScannedAttribute {
    name: String,
    decoded_value: String,
    value_range: Option<Range<usize>>,
    qualified: bool,
}

impl ScannedHtml {
    pub(crate) fn start_tags(&self) -> &[ScannedStartTag] {
        &self.start_tags
    }

    pub(crate) fn element_spans(&self, name: &str) -> &[Range<usize>] {
        self.element_spans
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

#[allow(dead_code)] // The complete scanner API is consumed by staged caller migrations.
impl ScannedStartTag {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn source_range(&self) -> Range<usize> {
        self.source_range.clone()
    }

    pub(crate) fn attributes(&self) -> &[ScannedAttribute] {
        &self.attributes
    }

    pub(crate) fn attribute(&self, name: &str) -> Option<&ScannedAttribute> {
        self.attributes
            .iter()
            .find(|attribute| attribute.name == name)
    }
}

#[allow(dead_code)] // The complete scanner API is consumed by staged caller migrations.
impl ScannedAttribute {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn decoded_value(&self) -> &str {
        &self.decoded_value
    }

    pub(crate) fn value_range(&self) -> Option<Range<usize>> {
        self.value_range.clone()
    }

    pub(crate) fn is_qualified(&self) -> bool {
        self.qualified
    }
}

struct RawAttribute {
    normalized_name: String,
    qualified: bool,
    value_range: Option<Range<usize>>,
}

struct RawStartTag {
    normalized_name: String,
    source_range: Range<usize>,
    attributes: Vec<RawAttribute>,
}

struct ParsedTag {
    normalized_name: String,
    source_range: Range<usize>,
    attributes: Vec<RawAttribute>,
    closing: bool,
}

enum TagScan {
    Invalid,
    Unclosed,
    Complete(ParsedTag),
}

// Mirrors html5ever's tag/attribute tokenizer transitions while retaining byte
// offsets into the immutable input. Only quoted attribute-value states hide `>`.
#[derive(Clone, Copy)]
enum TagState {
    TagName,
    BeforeAttributeName,
    AttributeName,
    AfterAttributeName,
    BeforeAttributeValue,
    AttributeValueDoubleQuoted,
    AttributeValueSingleQuoted,
    AttributeValueUnquoted,
    AfterAttributeValueQuoted,
    SelfClosingStartTag,
}

struct PendingAttribute {
    normalized_name: String,
    qualified: bool,
    value_range: Option<Range<usize>>,
}

#[derive(Debug)]
struct SemanticStartTag {
    name: String,
    attributes: Vec<(String, String)>,
    had_duplicate_attributes: bool,
}

#[derive(Default)]
struct SemanticSink {
    start_tags: RefCell<Vec<SemanticStartTag>>,
}

impl TokenSink for SemanticSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line_number: u64) -> TokenSinkResult<()> {
        let Token::TagToken(tag) = token else {
            return TokenSinkResult::Continue;
        };
        if tag.kind != TagKind::StartTag {
            return TokenSinkResult::Continue;
        }

        let name = tag.name.to_string();
        let attributes = tag
            .attrs
            .into_iter()
            .map(|attribute| {
                (
                    attribute.name.local.to_string(),
                    attribute.value.to_string(),
                )
            })
            .collect();
        self.start_tags.borrow_mut().push(SemanticStartTag {
            name: name.clone(),
            attributes,
            had_duplicate_attributes: tag.had_duplicate_attributes,
        });

        raw_data_result(&name)
    }
}

fn raw_data_result(name: &str) -> TokenSinkResult<()> {
    use html5ever::tokenizer::states::RawKind::{Rawtext, Rcdata, ScriptData};

    match name {
        "script" => TokenSinkResult::RawData(ScriptData),
        "style" | "xmp" | "iframe" | "noembed" | "noframes" => TokenSinkResult::RawData(Rawtext),
        "title" | "textarea" => TokenSinkResult::RawData(Rcdata),
        "plaintext" => TokenSinkResult::Plaintext,
        _ => TokenSinkResult::Continue,
    }
}

pub(crate) fn scan(source: &str) -> Result<ScannedHtml, ScanError> {
    scan_impl(source, None)
}

#[cfg(test)]
fn scan_with_source_steps(source: &str) -> Result<(ScannedHtml, usize), ScanError> {
    let mut steps = 0;
    let scanned = scan_impl(source, Some(&mut steps))?;
    Ok((scanned, steps))
}

fn scan_impl(source: &str, mut source_steps: Option<&mut usize>) -> Result<ScannedHtml, ScanError> {
    let (raw_start_tags, element_spans) = scan_source(source, &mut source_steps);
    let semantic_start_tags = tokenize(source);

    if semantic_start_tags.len() != raw_start_tags.len() {
        return Err(alignment_error(
            semantic_start_tags.len().min(raw_start_tags.len()),
            format!(
                "source has {} start tags but tokenizer has {}",
                raw_start_tags.len(),
                semantic_start_tags.len()
            ),
        ));
    }

    let mut start_tags = Vec::with_capacity(raw_start_tags.len());
    for (token_index, (semantic, raw)) in semantic_start_tags
        .into_iter()
        .zip(raw_start_tags)
        .enumerate()
    {
        if semantic.name != raw.normalized_name {
            return Err(alignment_error(
                token_index,
                format!(
                    "source tag {:?} does not match tokenizer tag {:?}",
                    raw.normalized_name, semantic.name
                ),
            ));
        }

        let mut first_raw_attributes = HashMap::with_capacity(raw.attributes.len());
        let mut raw_duplicate = false;
        for attribute in &raw.attributes {
            match first_raw_attributes.entry(attribute.normalized_name.as_str()) {
                Entry::Vacant(entry) => {
                    entry.insert(attribute);
                }
                Entry::Occupied(_) => raw_duplicate = true,
            }
        }
        if semantic.had_duplicate_attributes != raw_duplicate {
            return Err(alignment_error(
                token_index,
                "duplicate attribute state does not match source occurrences",
            ));
        }

        let mut attributes = Vec::with_capacity(semantic.attributes.len());
        for (name, decoded_value) in semantic.attributes {
            let Some(raw_attribute) = first_raw_attributes.get(name.as_str()).copied() else {
                return Err(alignment_error(
                    token_index,
                    format!("tokenizer attribute {name:?} has no source occurrence"),
                ));
            };

            validate_optional_range(source, raw_attribute.value_range.as_ref(), token_index)?;
            attributes.push(ScannedAttribute {
                name,
                decoded_value,
                value_range: raw_attribute.value_range.clone(),
                qualified: raw_attribute.qualified,
            });
        }

        let effective_raw_count = first_raw_attributes.len();
        if effective_raw_count != attributes.len() {
            return Err(alignment_error(
                token_index,
                format!(
                    "source has {effective_raw_count} effective attributes but tokenizer has {}",
                    attributes.len()
                ),
            ));
        }

        validate_range(source, &raw.source_range, token_index)?;
        start_tags.push(ScannedStartTag {
            name: semantic.name,
            source_range: raw.source_range,
            attributes,
        });
    }

    for spans in element_spans.values() {
        for span in spans {
            validate_range(source, span, start_tags.len())?;
        }
    }

    Ok(ScannedHtml {
        start_tags,
        element_spans,
    })
}

fn tokenize(source: &str) -> Vec<SemanticStartTag> {
    let input = BufferQueue::default();
    input.push_back(StrTendril::from_slice(source));
    let tokenizer = Tokenizer::new(SemanticSink::default(), Default::default());
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer.sink.start_tags.into_inner()
}

fn alignment_error(token_index: usize, reason: impl Into<String>) -> ScanError {
    ScanError {
        token_index,
        reason: reason.into(),
    }
}

fn validate_optional_range(
    source: &str,
    range: Option<&Range<usize>>,
    token_index: usize,
) -> Result<(), ScanError> {
    if let Some(range) = range {
        validate_range(source, range, token_index)?;
    }
    Ok(())
}

fn validate_range(source: &str, range: &Range<usize>, token_index: usize) -> Result<(), ScanError> {
    if range.start > range.end
        || range.end > source.len()
        || !source.is_char_boundary(range.start)
        || !source.is_char_boundary(range.end)
    {
        return Err(alignment_error(
            token_index,
            format!("invalid source byte range {range:?}"),
        ));
    }
    Ok(())
}

fn scan_source(
    source: &str,
    source_steps: &mut Option<&mut usize>,
) -> (Vec<RawStartTag>, HashMap<String, Vec<Range<usize>>>) {
    let bytes = source.as_bytes();
    let mut raw_start_tags = Vec::new();
    let mut slots: HashMap<String, Vec<Option<Range<usize>>>> = HashMap::new();
    let mut open_stacks: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    let mut cursor = 0;

    while cursor < bytes.len() {
        source_step(source_steps);
        if bytes[cursor] != b'<' {
            cursor += 1;
            continue;
        }

        let tag_start = cursor;
        if starts_with_at(bytes, tag_start, b"<!--") {
            cursor = find_comment_end(bytes, tag_start + 4, source_steps);
            continue;
        }
        if starts_with_at(bytes, tag_start, b"<?") {
            cursor = find_bogus_comment_end(bytes, tag_start + 2, source_steps);
            continue;
        }
        if starts_with_ascii_case_insensitive_at(bytes, tag_start, b"<!doctype") {
            cursor = find_doctype_end(bytes, tag_start + 9, source_steps);
            continue;
        }
        if starts_with_at(bytes, tag_start, b"<!") {
            cursor = find_bogus_comment_end(bytes, tag_start + 2, source_steps);
            continue;
        }

        match scan_tag(source, tag_start, source_steps) {
            TagScan::Invalid => cursor += 1,
            TagScan::Unclosed => break,
            TagScan::Complete(tag) => {
                cursor = tag.source_range.end;
                let name = tag.normalized_name.clone();

                if tag.closing {
                    if let Some((slot, open_start)) = open_stacks.get_mut(&name).and_then(Vec::pop)
                        && let Some(list) = slots.get_mut(&name)
                    {
                        list[slot] = Some(open_start..tag.source_range.end);
                    }
                    continue;
                }

                if is_void_tag(&name) {
                    slots
                        .entry(name.clone())
                        .or_default()
                        .push(Some(tag.source_range.clone()));
                } else {
                    let list = slots.entry(name.clone()).or_default();
                    let slot = list.len();
                    list.push(None);
                    open_stacks
                        .entry(name.clone())
                        .or_default()
                        .push((slot, tag.source_range.start));
                }

                let opaque = is_opaque_tag(&name);
                raw_start_tags.push(RawStartTag {
                    normalized_name: name.clone(),
                    source_range: tag.source_range,
                    attributes: tag.attributes,
                });

                if opaque {
                    if name == "plaintext" {
                        break;
                    }
                    cursor = find_opaque_close(source, cursor, &name, source_steps)
                        .unwrap_or(source.len());
                }
            }
        }
    }

    let element_spans = slots
        .into_iter()
        .map(|(name, list)| (name, list.into_iter().map_while(|slot| slot).collect()))
        .collect();
    (raw_start_tags, element_spans)
}

fn scan_tag(source: &str, start: usize, source_steps: &mut Option<&mut usize>) -> TagScan {
    let bytes = source.as_bytes();
    if bytes.get(start) != Some(&b'<') || matches!(bytes.get(start + 1), Some(b'!' | b'?')) {
        return TagScan::Invalid;
    }

    let mut cursor = start + 1;
    let closing = bytes.get(cursor) == Some(&b'/');
    if closing {
        cursor += 1;
    }
    if !bytes.get(cursor).is_some_and(u8::is_ascii_alphabetic) {
        return TagScan::Invalid;
    }

    let mut normalized_name = String::new();
    let mut attributes = Vec::new();
    let mut pending_attribute: Option<PendingAttribute> = None;
    let mut attribute_value_start = cursor;
    let mut state = TagState::TagName;

    while cursor < source.len() {
        let Some(current) = source[cursor..].chars().next() else {
            return TagScan::Unclosed;
        };
        let current_len = current.len_utf8();
        source_step(source_steps);

        match state {
            TagState::TagName => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => {
                    cursor += current_len;
                    state = TagState::BeforeAttributeName;
                }
                '/' => {
                    cursor += current_len;
                    state = TagState::SelfClosingStartTag;
                }
                '>' => {
                    cursor += current_len;
                    return complete_tag(
                        start,
                        cursor,
                        normalized_name,
                        attributes,
                        pending_attribute,
                        closing,
                    );
                }
                '\0' => {
                    normalized_name.push('\u{fffd}');
                    cursor += current_len;
                }
                _ => {
                    normalized_name.push(current.to_ascii_lowercase());
                    cursor += current_len;
                }
            },
            TagState::BeforeAttributeName => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => cursor += current_len,
                '/' => {
                    finish_pending_attribute(&mut attributes, &mut pending_attribute);
                    cursor += current_len;
                    state = TagState::SelfClosingStartTag;
                }
                '>' => {
                    finish_pending_attribute(&mut attributes, &mut pending_attribute);
                    cursor += current_len;
                    return complete_tag(start, cursor, normalized_name, attributes, None, closing);
                }
                _ => {
                    pending_attribute = Some(PendingAttribute {
                        normalized_name: normalized_char(current).to_string(),
                        qualified: current == ':',
                        value_range: None,
                    });
                    cursor += current_len;
                    state = TagState::AttributeName;
                }
            },
            TagState::AttributeName => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => {
                    cursor += current_len;
                    state = TagState::AfterAttributeName;
                }
                '/' => {
                    finish_pending_attribute(&mut attributes, &mut pending_attribute);
                    cursor += current_len;
                    state = TagState::SelfClosingStartTag;
                }
                '=' => {
                    cursor += current_len;
                    state = TagState::BeforeAttributeValue;
                }
                '>' => {
                    finish_pending_attribute(&mut attributes, &mut pending_attribute);
                    cursor += current_len;
                    return complete_tag(start, cursor, normalized_name, attributes, None, closing);
                }
                _ => {
                    if let Some(attribute) = pending_attribute.as_mut() {
                        attribute.normalized_name.push(normalized_char(current));
                        attribute.qualified |= current == ':';
                    }
                    cursor += current_len;
                }
            },
            TagState::AfterAttributeName => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => cursor += current_len,
                '/' => {
                    finish_pending_attribute(&mut attributes, &mut pending_attribute);
                    cursor += current_len;
                    state = TagState::SelfClosingStartTag;
                }
                '=' => {
                    cursor += current_len;
                    state = TagState::BeforeAttributeValue;
                }
                '>' => {
                    finish_pending_attribute(&mut attributes, &mut pending_attribute);
                    cursor += current_len;
                    return complete_tag(start, cursor, normalized_name, attributes, None, closing);
                }
                _ => {
                    finish_pending_attribute(&mut attributes, &mut pending_attribute);
                    state = TagState::BeforeAttributeName;
                }
            },
            TagState::BeforeAttributeValue => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => cursor += current_len,
                '"' => {
                    cursor += current_len;
                    attribute_value_start = cursor;
                    state = TagState::AttributeValueDoubleQuoted;
                }
                '\'' => {
                    cursor += current_len;
                    attribute_value_start = cursor;
                    state = TagState::AttributeValueSingleQuoted;
                }
                '>' => {
                    set_pending_value(&mut pending_attribute, cursor..cursor);
                    finish_pending_attribute(&mut attributes, &mut pending_attribute);
                    cursor += current_len;
                    return complete_tag(start, cursor, normalized_name, attributes, None, closing);
                }
                _ => {
                    attribute_value_start = cursor;
                    state = TagState::AttributeValueUnquoted;
                }
            },
            TagState::AttributeValueDoubleQuoted => {
                if current == '"' {
                    set_pending_value(&mut pending_attribute, attribute_value_start..cursor);
                    finish_pending_attribute(&mut attributes, &mut pending_attribute);
                    state = TagState::AfterAttributeValueQuoted;
                }
                cursor += current_len;
            }
            TagState::AttributeValueSingleQuoted => {
                if current == '\'' {
                    set_pending_value(&mut pending_attribute, attribute_value_start..cursor);
                    finish_pending_attribute(&mut attributes, &mut pending_attribute);
                    state = TagState::AfterAttributeValueQuoted;
                }
                cursor += current_len;
            }
            TagState::AttributeValueUnquoted => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => {
                    set_pending_value(&mut pending_attribute, attribute_value_start..cursor);
                    finish_pending_attribute(&mut attributes, &mut pending_attribute);
                    cursor += current_len;
                    state = TagState::BeforeAttributeName;
                }
                '>' => {
                    set_pending_value(&mut pending_attribute, attribute_value_start..cursor);
                    finish_pending_attribute(&mut attributes, &mut pending_attribute);
                    cursor += current_len;
                    return complete_tag(start, cursor, normalized_name, attributes, None, closing);
                }
                _ => cursor += current_len,
            },
            TagState::AfterAttributeValueQuoted => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => {
                    cursor += current_len;
                    state = TagState::BeforeAttributeName;
                }
                '/' => {
                    cursor += current_len;
                    state = TagState::SelfClosingStartTag;
                }
                '>' => {
                    cursor += current_len;
                    return complete_tag(start, cursor, normalized_name, attributes, None, closing);
                }
                _ => state = TagState::BeforeAttributeName,
            },
            TagState::SelfClosingStartTag => {
                if current == '>' {
                    finish_pending_attribute(&mut attributes, &mut pending_attribute);
                    cursor += current_len;
                    return complete_tag(start, cursor, normalized_name, attributes, None, closing);
                }
                state = TagState::BeforeAttributeName;
            }
        }
    }

    TagScan::Unclosed
}

fn normalized_char(character: char) -> char {
    if character == '\0' {
        '\u{fffd}'
    } else {
        character.to_ascii_lowercase()
    }
}

fn set_pending_value(attribute: &mut Option<PendingAttribute>, range: Range<usize>) {
    if let Some(attribute) = attribute {
        attribute.value_range = Some(range);
    }
}

fn finish_pending_attribute(
    attributes: &mut Vec<RawAttribute>,
    pending_attribute: &mut Option<PendingAttribute>,
) {
    let Some(attribute) = pending_attribute.take() else {
        return;
    };
    attributes.push(RawAttribute {
        normalized_name: attribute.normalized_name,
        qualified: attribute.qualified,
        value_range: attribute.value_range,
    });
}

fn complete_tag(
    start: usize,
    end: usize,
    normalized_name: String,
    mut attributes: Vec<RawAttribute>,
    mut pending_attribute: Option<PendingAttribute>,
    closing: bool,
) -> TagScan {
    finish_pending_attribute(&mut attributes, &mut pending_attribute);
    TagScan::Complete(ParsedTag {
        normalized_name,
        source_range: start..end,
        attributes,
        closing,
    })
}

fn find_opaque_close(
    source: &str,
    content_start: usize,
    name: &str,
    source_steps: &mut Option<&mut usize>,
) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = content_start;
    let closing_name_end_offset = 2 + name.len();

    while cursor + closing_name_end_offset <= bytes.len() {
        source_step(source_steps);
        let closing_name_end = cursor + closing_name_end_offset;
        let has_appropriate_boundary = matches!(
            bytes.get(closing_name_end),
            Some(byte) if byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>')
        );
        if has_appropriate_boundary
            && bytes.get(cursor..cursor + 2) == Some(b"</")
            && bytes[cursor + 2..closing_name_end].eq_ignore_ascii_case(name.as_bytes())
        {
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

fn find_comment_end(
    bytes: &[u8],
    content_start: usize,
    source_steps: &mut Option<&mut usize>,
) -> usize {
    if bytes.get(content_start) == Some(&b'>') {
        source_step(source_steps);
        return content_start + 1;
    }
    if bytes.get(content_start..content_start + 2) == Some(b"->") {
        source_step(source_steps);
        return content_start + 2;
    }

    let mut cursor = content_start;
    while cursor < bytes.len() {
        source_step(source_steps);
        if bytes.get(cursor..cursor + 3) == Some(b"-->") {
            return cursor + 3;
        }
        if bytes.get(cursor..cursor + 4) == Some(b"--!>") {
            return cursor + 4;
        }
        cursor += 1;
    }
    bytes.len()
}

fn find_bogus_comment_end(
    bytes: &[u8],
    content_start: usize,
    source_steps: &mut Option<&mut usize>,
) -> usize {
    let mut cursor = content_start;
    while cursor < bytes.len() {
        source_step(source_steps);
        if bytes[cursor] == b'>' {
            return cursor + 1;
        }
        cursor += 1;
    }
    bytes.len()
}

fn find_doctype_end(
    bytes: &[u8],
    content_start: usize,
    source_steps: &mut Option<&mut usize>,
) -> usize {
    let mut cursor = content_start;
    match bytes.get(cursor) {
        Some(b'>') => {
            source_step(source_steps);
            return cursor + 1;
        }
        Some(byte) if is_html_whitespace(*byte) => {}
        Some(_) => return find_bogus_comment_end(bytes, cursor, source_steps),
        None => return bytes.len(),
    }

    skip_html_whitespace(bytes, &mut cursor, source_steps);
    match bytes.get(cursor) {
        Some(b'>') => {
            source_step(source_steps);
            return cursor + 1;
        }
        Some(_) => {}
        None => return bytes.len(),
    }

    while bytes
        .get(cursor)
        .is_some_and(|byte| !is_html_whitespace(*byte) && *byte != b'>')
    {
        source_step(source_steps);
        cursor += 1;
    }
    if bytes.get(cursor) == Some(&b'>') {
        source_step(source_steps);
        return cursor + 1;
    }
    if cursor == bytes.len() {
        return bytes.len();
    }

    skip_html_whitespace(bytes, &mut cursor, source_steps);
    if bytes.get(cursor) == Some(&b'>') {
        source_step(source_steps);
        return cursor + 1;
    }

    let is_public = if starts_with_ascii_case_insensitive_at(bytes, cursor, b"public") {
        source_steps_add(source_steps, b"public".len());
        cursor += b"public".len();
        true
    } else if starts_with_ascii_case_insensitive_at(bytes, cursor, b"system") {
        source_steps_add(source_steps, b"system".len());
        cursor += b"system".len();
        false
    } else {
        return find_bogus_comment_end(bytes, cursor, source_steps);
    };

    skip_html_whitespace(bytes, &mut cursor, source_steps);
    let Some(quote @ (b'\'' | b'"')) = bytes.get(cursor).copied() else {
        return match bytes.get(cursor) {
            Some(b'>') => {
                source_step(source_steps);
                cursor + 1
            }
            Some(_) => find_bogus_comment_end(bytes, cursor, source_steps),
            None => bytes.len(),
        };
    };

    let mut cursor = match find_quoted_doctype_identifier_end(bytes, cursor, quote, source_steps) {
        QuotedDoctypeIdentifierEnd::Quote(cursor) => cursor,
        QuotedDoctypeIdentifierEnd::Declaration(end) => return end,
        QuotedDoctypeIdentifierEnd::Eof => return bytes.len(),
    };
    skip_html_whitespace(bytes, &mut cursor, source_steps);
    if !is_public || bytes.get(cursor) == Some(&b'>') {
        return finish_doctype(bytes, cursor, source_steps);
    }

    let Some(quote @ (b'\'' | b'"')) = bytes.get(cursor).copied() else {
        return match bytes.get(cursor) {
            Some(_) => find_bogus_comment_end(bytes, cursor, source_steps),
            None => bytes.len(),
        };
    };
    let cursor = match find_quoted_doctype_identifier_end(bytes, cursor, quote, source_steps) {
        QuotedDoctypeIdentifierEnd::Quote(cursor) => cursor,
        QuotedDoctypeIdentifierEnd::Declaration(end) => return end,
        QuotedDoctypeIdentifierEnd::Eof => return bytes.len(),
    };
    finish_doctype(bytes, cursor, source_steps)
}

enum QuotedDoctypeIdentifierEnd {
    Quote(usize),
    Declaration(usize),
    Eof,
}

fn find_quoted_doctype_identifier_end(
    bytes: &[u8],
    mut cursor: usize,
    quote: u8,
    source_steps: &mut Option<&mut usize>,
) -> QuotedDoctypeIdentifierEnd {
    source_step(source_steps);
    cursor += 1;
    while cursor < bytes.len() {
        source_step(source_steps);
        let current = bytes[cursor];
        cursor += 1;
        if current == b'>' {
            return QuotedDoctypeIdentifierEnd::Declaration(cursor);
        }
        if current == quote {
            return QuotedDoctypeIdentifierEnd::Quote(cursor);
        }
    }
    QuotedDoctypeIdentifierEnd::Eof
}

fn finish_doctype(bytes: &[u8], mut cursor: usize, source_steps: &mut Option<&mut usize>) -> usize {
    skip_html_whitespace(bytes, &mut cursor, source_steps);
    match bytes.get(cursor) {
        Some(b'>') => {
            source_step(source_steps);
            cursor + 1
        }
        Some(_) => find_bogus_comment_end(bytes, cursor, source_steps),
        None => bytes.len(),
    }
}

fn skip_html_whitespace(bytes: &[u8], cursor: &mut usize, source_steps: &mut Option<&mut usize>) {
    while bytes
        .get(*cursor)
        .is_some_and(|byte| is_html_whitespace(*byte))
    {
        source_step(source_steps);
        *cursor += 1;
    }
}

fn source_step(source_steps: &mut Option<&mut usize>) {
    source_steps_add(source_steps, 1);
}

fn source_steps_add(source_steps: &mut Option<&mut usize>, amount: usize) {
    if let Some(steps) = source_steps.as_deref_mut() {
        *steps += amount;
    }
}

fn is_void_tag(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_opaque_tag(name: &str) -> bool {
    matches!(
        name,
        "script"
            | "style"
            | "title"
            | "textarea"
            | "xmp"
            | "iframe"
            | "noembed"
            | "noframes"
            | "plaintext"
    )
}

fn is_html_whitespace(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

fn starts_with_at(bytes: &[u8], index: usize, needle: &[u8]) -> bool {
    index + needle.len() <= bytes.len() && &bytes[index..index + needle.len()] == needle
}

fn starts_with_ascii_case_insensitive_at(bytes: &[u8], index: usize, needle: &[u8]) -> bool {
    bytes
        .get(index..index + needle.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn bogus_declarations_and_processing_instructions_are_opaque() {
        for input in [
            "<!foo <div>fake</div>><div>real</div>",
            "<?foo <div>fake</div>><div>real</div>",
        ] {
            let real_start = input.find("<div>real</div>").unwrap();
            let scanned = scan(input).unwrap();

            assert_eq!(scanned.element_spans("div"), &[real_start..input.len()]);
            assert_eq!(scanned.start_tags().len(), 1);
            assert_eq!(scanned.start_tags()[0].name(), "div");
        }
    }

    #[test]
    fn decoded_attributes_retain_the_original_value_range() {
        let input = r#"<img src='https&colon;//cdn.example.test/p.jpg?x=&#38y=2'>"#;
        let scanned = scan(input).unwrap();
        let src = scanned.start_tags()[0].attribute("src").unwrap();
        let range = src.value_range().unwrap();

        assert_eq!(src.decoded_value(), "https://cdn.example.test/p.jpg?x=&y=2");
        assert_eq!(
            &input[range],
            "https&colon;//cdn.example.test/p.jpg?x=&#38y=2"
        );
    }

    #[test]
    fn raw_text_and_rcdata_hide_tag_looking_content() {
        let input = concat!(
            "<script><img src='/fake-script.jpg'></script>",
            "<textarea><img src='/fake-textarea.jpg'></textarea>",
            "<img src='/real.jpg'>",
        );
        let scanned = scan(input).unwrap();
        let images: Vec<_> = scanned
            .start_tags()
            .iter()
            .filter(|tag| tag.name() == "img")
            .collect();

        assert_eq!(images.len(), 1);
        assert_eq!(
            images[0].attribute("src").unwrap().decoded_value(),
            "/real.jpg"
        );
    }

    #[test]
    fn invalid_ampersands_and_bogus_markup_have_linear_source_work() {
        for size in [10_000, 20_000, 40_000, 80_000] {
            let input = format!(
                "<?probe {}><img src='https://cdn.example.test/p.jpg?{}'>",
                "<div>fake</div>".repeat(size / 20),
                "&invalid".repeat(size / 8),
            );
            let (scanned, steps) = scan_with_source_steps(&input).unwrap();

            assert_eq!(
                scanned
                    .start_tags()
                    .iter()
                    .filter(|tag| tag.name() == "img")
                    .count(),
                1
            );
            assert!(
                steps <= 12 * input.len(),
                "{steps} steps for {} bytes",
                input.len()
            );
        }
    }
}
