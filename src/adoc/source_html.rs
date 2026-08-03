use super::{RenderCtx, classify::is_stripped_chrome, render_specialcharacters_listing};
use ego_tree::NodeId;
use scraper::{ElementRef, Html, node::Node};
use std::{borrow::Cow, collections::HashMap, ops::Range};
use url::Url;

pub(crate) type SourceMap = HashMap<NodeId, Range<usize>>;

const SAFE_ELEMENTS: &[&str] = &[
    "a",
    "abbr",
    "address",
    "article",
    "aside",
    "b",
    "bdi",
    "bdo",
    "blockquote",
    "br",
    "caption",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "del",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "main",
    "mark",
    "nav",
    "ol",
    "p",
    "pre",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "section",
    "small",
    "span",
    "strong",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "time",
    "tr",
    "u",
    "ul",
    "var",
    "wbr",
];

const SAFE_GLOBAL_ATTRIBUTES: &[&str] = &["class", "id", "title", "lang", "dir", "role"];

pub(crate) fn render_passthrough(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> String {
    if is_stripped_chrome(element) {
        return String::new();
    }

    let html = source_preserving_html(element, ctx);

    if !passthrough_is_safe(html.as_ref()) {
        return render_source_listing(html.as_ref());
    }

    let delimiter = safe_block_delimiter(html.as_ref(), '+');

    format!("{delimiter}\n{html}\n{delimiter}\n\n")
}

fn passthrough_is_safe(source_html: &str) -> bool {
    let Ok(scanned) = crate::html_scan::scan(source_html) else {
        return false;
    };

    scanned.start_tags().iter().all(|tag| {
        SAFE_ELEMENTS.contains(&tag.name())
            && tag.attributes().iter().all(|attribute| {
                passthrough_attribute_is_safe(
                    tag.name(),
                    attribute.name(),
                    attribute.is_qualified(),
                ) && (!matches!(attribute.name(), "href" | "src")
                    || has_safe_passthrough_url(attribute.decoded_value()))
            })
    })
}

pub(super) fn render_source_listing(content: &str) -> String {
    render_specialcharacters_listing(content)
}

fn passthrough_attribute_is_safe(tag: &str, attribute: &str, qualified: bool) -> bool {
    if qualified || attribute.starts_with("on") || matches!(attribute, "style" | "srcdoc") {
        return false;
    }

    SAFE_GLOBAL_ATTRIBUTES.contains(&attribute)
        || attribute.starts_with("aria-")
        || match tag {
            "a" => attribute == "href",
            "img" => matches!(attribute, "src" | "alt" | "width" | "height"),
            "col" | "colgroup" => attribute == "span",
            "ol" => matches!(attribute, "start" | "reversed" | "type"),
            "li" => attribute == "value",
            "td" | "th" => {
                matches!(
                    attribute,
                    "colspan" | "rowspan" | "headers" | "scope" | "abbr"
                )
            }
            "time" | "del" | "ins" => attribute == "datetime",
            _ => false,
        }
}

pub(super) fn passthrough_element_name_is_safe(name: &str) -> bool {
    SAFE_ELEMENTS.contains(&name)
}

fn has_safe_passthrough_url(value: &str) -> bool {
    match Url::parse(value) {
        Ok(url) => matches!(url.scheme(), "http" | "https") && url.has_host(),
        Err(url::ParseError::RelativeUrlWithoutBase) => Url::parse("https://source.invalid/")
            .and_then(|base| base.join(value))
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host()),
        Err(_) => false,
    }
}

pub(crate) fn safe_block_delimiter(content: &str, character: char) -> String {
    safe_block_delimiter_impl(content, character, None)
}

#[cfg(test)]
pub(super) fn safe_block_delimiter_with_steps(content: &str, character: char) -> (String, usize) {
    let mut inspected = 0;
    let delimiter = safe_block_delimiter_impl(content, character, Some(&mut inspected));

    (delimiter, inspected)
}

fn safe_block_delimiter_impl(
    content: &str,
    character: char,
    mut inspected: Option<&mut usize>,
) -> String {
    let mut maximum = 3;

    for line in content.lines() {
        let mut count = 0;
        let mut only_delimiter_characters = true;

        for candidate in line.chars() {
            count += 1;
            only_delimiter_characters &= candidate == character;
            if let Some(steps) = inspected.as_deref_mut() {
                *steps += 1;
            }
        }
        if let Some(steps) = inspected.as_deref_mut() {
            *steps += 1;
        }
        if only_delimiter_characters {
            maximum = maximum.max(count);
        }
    }

    character.to_string().repeat(maximum.max(3) + 1)
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
pub(crate) fn build_source_map(
    document: &Html,
    scanned: &crate::html_scan::ScannedHtml,
) -> SourceMap {
    let mut counters: HashMap<String, usize> = HashMap::new();
    let mut source_map = HashMap::new();

    for node in document.tree.root().descendants() {
        let Some(element) = ElementRef::wrap(node) else {
            continue;
        };

        let name = element.value().name().to_ascii_lowercase();
        let ordinal = *counters.get(&name).unwrap_or(&0);

        if let Some(span) = scanned.element_spans(&name).get(ordinal) {
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
    use super::render_source_listing;

    #[test]
    fn source_listing_declares_specialcharacters_only_substitutions() {
        assert_eq!(
            render_source_listing("pass:[<img src=x onerror=alert(1)>]"),
            concat!(
                "[subs=specialcharacters]\n",
                "----\n",
                "pass:[<img src=x onerror=alert(1)>]\n",
                "----\n\n",
            )
        );
    }

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
