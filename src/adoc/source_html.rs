use ego_tree::NodeId;
use scraper::{ElementRef, Html, node::Node};
use std::{borrow::Cow, collections::HashMap, ops::Range};

use super::{MAX_RENDER_DEPTH, RenderCtx, classify::is_stripped_chrome};

pub(crate) type SourceMap = HashMap<NodeId, Range<usize>>;

pub(crate) fn render_passthrough(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> String {
    if is_stripped_chrome(element) {
        return String::new();
    }

    let html = source_preserving_html(element, ctx);

    if contains_script_or_style_tag(html.as_ref()) {
        return render_source_listing(html.as_ref());
    }

    let delimiter = safe_block_delimiter(html.as_ref(), '+');

    format!("{delimiter}\n{html}\n{delimiter}\n\n")
}

fn render_source_listing(content: &str) -> String {
    let delimiter = safe_block_delimiter(content, '-');

    format!("{delimiter}\n{content}\n{delimiter}\n\n")
}

fn contains_script_or_style_tag(source_html: &str) -> bool {
    let mut search_start = 0;

    while let Some(relative_start) = source_html[search_start..].find('<') {
        let start = search_start + relative_start;
        let Some(tag) = parse_source_tag(source_html, start) else {
            search_start = start + 1;
            continue;
        };

        if !tag.is_closing && matches!(tag.name.to_ascii_lowercase().as_str(), "script" | "style") {
            return true;
        }

        search_start = tag.end;
    }

    false
}

pub(crate) fn safe_block_delimiter(content: &str, character: char) -> String {
    let mut delimiter = character.to_string().repeat(4);

    while content.lines().any(|line| line == delimiter) {
        delimiter.push(character);
    }

    delimiter
}

fn source_preserving_html<'a>(element: ElementRef<'_>, ctx: RenderCtx<'a>) -> Cow<'a, str> {
    if let Some(range) = ctx.source_map.get(&element.id()) {
        return Cow::Borrowed(&ctx.source_html[range.clone()]);
    }

    let html = element.html();
    if element.value().name() == "table" {
        let without_inserted_tbody = html.replace("<tbody>", "").replace("</tbody>", "");
        if ctx.source_html.contains(&without_inserted_tbody) {
            return Cow::Owned(without_inserted_tbody);
        }
    }

    Cow::Owned(html)
}

/// Precompute the original source byte range of every element in the document
/// with one linear scan of the source plus one walk of the DOM, instead of
/// rescanning the whole tree and source once per passthrough (which was O(n^2)
/// for deeply nested content).
///
/// Each element maps to the source range of the k-th source element of the
/// same (lowercased) tag name, where k is the element's document-order ordinal
/// among elements of that name. Because the DOM is parsed from the source, the
/// k-th element of a given name in DOM document order is the k-th of that name in
/// source document order, so this reproduces the former
/// `(name, serialized-html)` matching byte-for-byte: the html refinement only
/// partitioned each name's elements without reordering them, and source/DOM order
/// already align 1:1 per name. Parser-inserted elements (e.g. `<tbody>`) have no
/// source counterpart, so the DOM holds more elements of that name than the
/// source recovered; the counter runs past the recovered spans and those
/// elements get no entry, falling back through `source_preserving_html` exactly
/// as before. No `element.html()` and no per-candidate re-parse are performed.
///
/// One intentional, content-preserving deviation from "byte-for-byte": a
/// self-closing tag on a non-void element (e.g. `<div/>`) is *not* treated as
/// closed here (see `collect_source_spans`), because html5ever does not treat
/// it as closed either — the following flow content is nested inside it. Such
/// an element's slot is left open, so it gets no entry and falls back to
/// `source_preserving_html`'s `element.html()`, trading a raw `<div/>` for a
/// normalized `<div>...</div>` rather than silently dropping the nested
/// content.
pub(crate) fn build_source_map(document: &Html, source_html: &str) -> SourceMap {
    let source_by_name = collect_source_spans(source_html);
    let mut counters: HashMap<String, usize> = HashMap::new();
    let mut source_map = HashMap::new();

    for node in document.tree.root().descendants() {
        let Some(element) = ElementRef::wrap(node) else {
            continue;
        };

        let name = element.value().name().to_ascii_lowercase();
        let ordinal = *counters.get(&name).unwrap_or(&0);

        if let Some(span) = source_by_name
            .get(&name)
            .and_then(|spans| spans.get(ordinal))
        {
            source_map.insert(element.id(), span.clone());
        }

        *counters.entry(name).or_insert(0) += 1;
    }

    source_map
}

/// Recover, in one linear scan, the original source byte range of every source
/// element grouped by lowercased tag name in document (opening) order.
///
/// A per-name stack pairs each opening tag with its matching closing tag exactly
/// as the former `find_source_element_end` did — same-name depth counting, other
/// tags ignored — but for every element at once in O(source length) rather than
/// rescanning the remaining source once per opening tag. Only genuinely void
/// tags (`is_source_void_tag`) are recorded immediately. A self-closing tag on
/// a non-void element is deliberately *not* treated as immediately closed: HTML5
/// (and html5ever) ignores the trailing `/` on non-void elements and parses the
/// following content as its children, so treating it as closed here would make
/// the recovered span omit content the DOM actually nests inside the element.
/// Such a tag is instead pushed onto the opening-tag branch below like a plain
/// opening tag, so it only resolves once (if) a real closing tag is found.
/// Comments and raw-text/RCDATA children are advanced over as opaque regions so
/// tag-looking text inside them does not consume element ordinals.
///
/// The former scan abandoned a tag name for the rest of the source once an
/// opening tag had no matching close, yielding `None` for that name from the
/// first unbalanced element onward. That is reproduced by keeping each name's
/// spans in opening order with one slot reserved per opening tag (empty until its
/// close is seen), then keeping only the leading run of slots that were closed:
/// the first never-closed opening tag, and everything after it, is dropped.
fn collect_source_spans(source_html: &str) -> HashMap<String, Vec<Range<usize>>> {
    // Per name, one slot per opening tag in opening order; `None` until the
    // matching close is found (and permanently `None` if it never is).
    let mut slots: HashMap<String, Vec<Option<(usize, usize)>>> = HashMap::new();
    // Per name, stack of (slot index, opening-tag start) awaiting a close.
    let mut open_stacks: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    let mut search_start = 0;

    while let Some(relative_start) = source_html[search_start..].find('<') {
        let start = search_start + relative_start;

        if source_html[start..].starts_with("<!--") {
            search_start = find_source_comment_end(source_html.as_bytes(), start + 4);
            continue;
        }

        let Some(tag) = parse_source_tag(source_html, start) else {
            search_start = start + 1;
            continue;
        };

        let name = tag.name.to_ascii_lowercase();
        let raw_text_name =
            (!tag.is_closing && is_source_raw_text_tag(&name)).then(|| name.clone());

        if tag.is_closing {
            if let Some((slot, open_start)) = open_stacks.get_mut(&name).and_then(Vec::pop)
                && let Some(list) = slots.get_mut(&name)
            {
                list[slot] = Some((open_start, tag.end));
            }
        } else if is_source_void_tag(&name) {
            slots.entry(name).or_default().push(Some((start, tag.end)));
        } else {
            let list = slots.entry(name.clone()).or_default();
            let slot = list.len();
            list.push(None);
            open_stacks.entry(name).or_default().push((slot, start));
        }

        if let Some(raw_text_name) = raw_text_name {
            if raw_text_name == "plaintext" {
                search_start = source_html.len();
            } else {
                let closing_prefix = format!("</{raw_text_name}");
                search_start = find_ascii_case_insensitive_tag_prefix(
                    source_html.as_bytes(),
                    tag.end,
                    closing_prefix.as_bytes(),
                )
                .unwrap_or(source_html.len());
            }
            continue;
        }

        search_start = tag.end;
    }

    slots
        .into_iter()
        .map(|(name, list)| {
            (
                name,
                list.into_iter()
                    .map_while(|slot| slot)
                    .map(|(start, end)| start..end)
                    .collect(),
            )
        })
        .collect()
}

/// Return the first byte after an HTML-tokenizer-recognized comment end. `>` and
/// `->` are abrupt endings only in the two states immediately after `<!--`;
/// `-->` and the parse-error form `--!>` end a comment from its body.
fn find_source_comment_end(haystack: &[u8], content_start: usize) -> usize {
    if haystack.get(content_start) == Some(&b'>') {
        return content_start + 1;
    }
    if haystack.get(content_start..content_start + 2) == Some(b"->") {
        return content_start + 2;
    }

    let mut cursor = content_start;

    while cursor < haystack.len() {
        if haystack.get(cursor..cursor + 3) == Some(b"-->") {
            return cursor + 3;
        }
        if haystack.get(cursor..cursor + 4) == Some(b"--!>") {
            return cursor + 4;
        }
        cursor += 1;
    }

    haystack.len()
}

fn find_ascii_case_insensitive_tag_prefix(
    haystack: &[u8],
    start: usize,
    needle: &[u8],
) -> Option<usize> {
    let mut cursor = start;

    while cursor + needle.len() <= haystack.len() {
        let candidate_end = cursor + needle.len();
        let has_appropriate_boundary = matches!(
            haystack.get(candidate_end),
            Some(byte) if byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>')
        );

        if has_appropriate_boundary && haystack[cursor..candidate_end].eq_ignore_ascii_case(needle)
        {
            return Some(cursor);
        }
        cursor += 1;
    }

    None
}

struct SourceTag {
    name: String,
    is_closing: bool,
    end: usize,
}

fn parse_source_tag(source_html: &str, start: usize) -> Option<SourceTag> {
    let bytes = source_html.as_bytes();

    if bytes.get(start) != Some(&b'<') || matches!(bytes.get(start + 1), Some(b'!' | b'?')) {
        return None;
    }

    let end = find_source_tag_end(source_html, start)? + 1;
    let mut cursor = start + 1;
    let is_closing = bytes.get(cursor) == Some(&b'/');
    if is_closing {
        cursor += 1;
    }

    let first = *bytes.get(cursor)?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let name_start = cursor;
    while matches!(bytes.get(cursor), Some(byte) if !byte.is_ascii_whitespace() && !matches!(byte, b'/' | b'>' | 0))
    {
        cursor += 1;
    }
    let name = source_html[name_start..cursor].to_ascii_lowercase();

    Some(SourceTag {
        name,
        is_closing,
        end,
    })
}

fn find_source_tag_end(source_html: &str, start: usize) -> Option<usize> {
    let mut quote = None;

    for (offset, byte) in source_html.as_bytes()[start + 1..].iter().enumerate() {
        match (quote, *byte) {
            (Some(active), current) if active == current => quote = None,
            (None, b'\'' | b'"') => quote = Some(*byte),
            (None, b'>') => return Some(start + 1 + offset),
            _ => {}
        }
    }

    None
}

fn is_source_void_tag(name: &str) -> bool {
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

fn is_source_raw_text_tag(name: &str) -> bool {
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

pub(crate) fn raw_text(element: ElementRef<'_>) -> String {
    raw_text_at(element, 0)
}

fn raw_text_at(element: ElementRef<'_>, depth: usize) -> String {
    // Bound this self-recursion at the same depth as `render_element`. At the cap
    // we stop recursing and return the text accumulated so far, so deeply nested
    // `<pre>`/`<code>` content degrades gracefully (truncated) instead of
    // overflowing the stack.
    if depth >= MAX_RENDER_DEPTH {
        return String::new();
    }

    let mut output = String::new();

    for child in element.children() {
        match child.value() {
            Node::Text(text) => output.push_str(text),
            Node::Element(_) => {
                if let Some(child_element) = ElementRef::wrap(child) {
                    output.push_str(&raw_text_at(child_element, depth + 1));
                }
            }
            _ => {}
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, ops::Range};

    #[test]
    fn nested_source_index_stores_one_range_per_element() {
        let depth = 4_000;
        let html = format!("{}x{}", "<div>".repeat(depth), "</div>".repeat(depth));
        let spans: HashMap<String, Vec<Range<usize>>> = collect_source_spans(&html);
        let divs = spans.get("div").unwrap();
        assert_eq!(divs.len(), depth);
        assert_eq!(divs[0], 0..html.len());
        assert!(divs.windows(2).all(|pair| pair[0].start < pair[1].start));
    }
}
