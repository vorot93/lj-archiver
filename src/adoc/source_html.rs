use super::{RenderCtx, classify::is_stripped_chrome};
use ego_tree::NodeId;
use scraper::{ElementRef, Html, node::Node};
use std::{borrow::Cow, collections::HashMap, ops::Range};
use url::Url;

pub(crate) type SourceMap = HashMap<NodeId, Range<usize>>;

pub(crate) fn render_passthrough(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> String {
    if is_stripped_chrome(element) {
        return String::new();
    }

    let html = source_preserving_html(element, ctx);

    if contains_script_or_style_tag(html.as_ref())
        || contains_rejected_url_attribute(html.as_ref(), ctx)
    {
        return render_source_listing(html.as_ref());
    }

    let delimiter = safe_block_delimiter(html.as_ref(), '+');

    format!("{delimiter}\n{html}\n{delimiter}\n\n")
}

fn contains_rejected_url_attribute(source_html: &str, ctx: RenderCtx<'_>) -> bool {
    let fragment = Html::parse_fragment(source_html);

    fragment
        .tree
        .root()
        .descendants()
        .filter_map(ElementRef::wrap)
        .any(|element| {
            element.value().attrs().any(|(local_name, value)| {
                matches!(local_name, "href" | "src") && !is_safe_url_reference(value, ctx)
            })
        })
}

fn is_safe_url_reference(value: &str, ctx: RenderCtx<'_>) -> bool {
    if let Some(rewriter) = ctx.link_rewriter {
        return rewriter.absolute_target(value).is_some();
    }

    Url::parse("https://source.invalid/")
        .expect("static URL is valid")
        .join(value)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
}

fn render_source_listing(content: &str) -> String {
    let delimiter = safe_block_delimiter(content, '-');

    format!("{delimiter}\n{content}\n{delimiter}\n\n")
}

fn contains_script_or_style_tag(source_html: &str) -> bool {
    crate::html_scan::scan(source_html).map_or(true, |scanned| {
        scanned
            .start_tags()
            .iter()
            .any(|tag| matches!(tag.name(), "script" | "style"))
    })
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
/// closed here, because html5ever does not treat it as closed either — the
/// following flow content is nested inside it. Such an element's slot is left
/// open, so it gets no entry and falls back to `source_preserving_html`'s
/// `element.html()`, trading a raw `<div/>` for a normalized
/// `<div>...</div>` rather than silently dropping the nested content.
pub(crate) fn build_source_map(document: &Html, source_html: &str) -> SourceMap {
    let scanned = crate::html_scan::scan(source_html).ok();
    let mut counters: HashMap<String, usize> = HashMap::new();
    let mut source_map = HashMap::new();

    for node in document.tree.root().descendants() {
        let Some(element) = ElementRef::wrap(node) else {
            continue;
        };

        let name = element.value().name().to_ascii_lowercase();
        let ordinal = *counters.get(&name).unwrap_or(&0);

        if let Some(span) = scanned
            .as_ref()
            .and_then(|html| html.element_spans(&name).get(ordinal))
        {
            source_map.insert(element.id(), span.clone());
        }

        *counters.entry(name).or_insert(0) += 1;
    }

    source_map
}

pub(crate) fn raw_text(element: ElementRef<'_>) -> String {
    let mut output = String::new();
    let mut stack: Vec<_> = element.children().rev().collect();

    while let Some(node) = stack.pop() {
        match node.value() {
            Node::Text(text) => output.push_str(text),
            Node::Element(_) => stack.extend(node.children().rev()),
            _ => {}
        }
    }

    output
}

#[cfg(test)]
mod tests {
    #[test]
    fn nested_source_index_stores_one_range_per_element() {
        let depth = 4_000;
        let html = format!("{}x{}", "<div>".repeat(depth), "</div>".repeat(depth));
        let scanned = crate::html_scan::scan(&html).unwrap();
        let divs = scanned.element_spans("div");
        assert_eq!(divs.len(), depth);
        assert_eq!(divs[0], 0..html.len());
        assert!(divs.windows(2).all(|pair| pair[0].start < pair[1].start));
    }
}
