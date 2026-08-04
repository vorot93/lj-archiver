use super::{RenderCtx, classify::is_stripped_chrome, render_specialcharacters_listing};
use ego_tree::NodeId;
use scraper::{ElementRef, Html, node::Node};
use std::{collections::HashMap, ops::Range};
use url::Url;

/// Maps semantic DOM nodes to scanner-owned original byte ranges through the
/// synthetic identity attribute carried by the instrumented parse. Because the
/// identity is read from the node itself rather than inferred from a per-name
/// ordinal, DOM reordering (html5ever foster parenting, adoption agency
/// cloning) can no longer exchange same-name spans.
#[derive(Debug, Default)]
pub(crate) struct SourceIndex {
    ranges: HashMap<NodeId, Range<usize>>,
    identities: HashMap<NodeId, usize>,
}

impl SourceIndex {
    /// The original byte range of the whole element the node was parsed from,
    /// present only when the source closed it.
    pub(crate) fn range(&self, node: NodeId) -> Option<Range<usize>> {
        self.ranges.get(&node).cloned()
    }

    /// The source identity the node was parsed from. Present whenever the source
    /// opened the element — including when it never closed it, so there is no
    /// [`Self::range`] but the start tag's own offset is still known.
    pub(crate) fn identity(&self, node: NodeId) -> Option<usize> {
        self.identities.get(&node).copied()
    }
}

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

    let Some(html) = source_preserving_html(element, ctx) else {
        return render_enclosing_source_listing(element, ctx);
    };

    if !passthrough_is_safe(html) {
        return render_source_listing(html);
    }

    let delimiter = safe_block_delimiter(html, '+');

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

fn source_preserving_html<'a>(element: ElementRef<'_>, ctx: RenderCtx<'a>) -> Option<&'a str> {
    ctx.source_index
        .range(element.id())
        .map(|range| &ctx.source_html[range])
}

/// The original byte offset of the start tag this node was parsed from. Unlike
/// [`SourceIndex::range`] it survives an element the source never closed, which
/// is exactly when the caller needs it.
fn source_start_offset(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> Option<usize> {
    let identity = ctx.source_index.identity(element.id())?;

    Some(ctx.scanned.start_tags().get(identity)?.source_range().start)
}

/// A node with no scanner-owned range must never be re-serialized: the
/// serialization would carry the synthetic identity attribute and would not be
/// the archived bytes. Emit original bytes running from the element's own start
/// tag to the end of the nearest enclosing known range instead, as one inert
/// listing — the same fail-closed response the renderer already gives to scanner
/// ambiguity.
///
/// Starting at the element's *own* start tag is load-bearing. An enclosing range
/// begins before the element, so listing all of it re-emits preceding siblings
/// the renderer has already rendered, and prints LiveJournal chrome that
/// [`is_stripped_chrome`] deliberately removed. An unclosed or self-closed
/// non-void `<div>` is ordinary in real post bodies, so this path is routine
/// rather than exotic.
fn render_enclosing_source_listing(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> String {
    let enclosing = element
        .ancestors()
        .find_map(|ancestor| ctx.source_index.range(ancestor.id()));
    let start = source_start_offset(element, ctx)
        .or_else(|| enclosing.as_ref().map(|range| range.start))
        .unwrap_or(0);
    // A range that does not cover the start tag cannot bound it: run to the end
    // of the input rather than emit an empty listing and lose the content.
    let end = enclosing
        .filter(|range| range.contains(&start))
        .map_or(ctx.source_html.len(), |range| range.end);

    render_source_listing(&ctx.source_html[start..end])
}

/// Builds the DOM-node → original-source index from the instrumented parse with
/// one walk of the DOM, instead of rescanning the tree and source once per
/// passthrough (which was O(n^2) for deeply nested content).
///
/// The instrumented copy adds one synthetic identity attribute per *source*
/// start tag, so a node gets an identity exactly when the source opened it.
/// Parser-inserted nodes (`<tbody>`) carry none and get no entry at all. A
/// source element the document never closed — which includes a self-closing
/// non-void tag such as `<div/>`, since html5ever nests the following flow
/// content inside it rather than closing it — keeps its identity but gets no
/// *range*, so [`render_passthrough`] fails closed to a listing rather than
/// re-serializing a node whose serialization would carry the synthetic attribute
/// and would not be the archived bytes. The identity is still what bounds that
/// listing at the element's own start tag, so it is recorded either way.
///
/// The fragment parser's own `<html>` root is skipped: it is not an archived
/// element, but an authored `<html>` start tag merges its attributes — including
/// the synthetic identity — onto it. Indexing it would hand every unmapped node
/// an "enclosing" range that need not enclose it at all, silently dropping
/// everything the authored `<html>` element did not cover.
pub(crate) fn build_source_index(
    document: &Html,
    scanned: &crate::html_scan::ScannedHtml,
) -> SourceIndex {
    let attribute = scanned.source_identity_attribute();
    let fragment_root = document.root_element().id();
    let mut index = SourceIndex::default();

    for node in document.tree.root().descendants() {
        let Some(element) = ElementRef::wrap(node).filter(|element| element.id() != fragment_root)
        else {
            continue;
        };
        let Some(identity) = element
            .value()
            .attr(attribute)
            .and_then(|identity| identity.parse::<usize>().ok())
        else {
            continue;
        };

        index.identities.insert(element.id(), identity);
        if let Some(range) = scanned.element_range(identity) {
            index.ranges.insert(element.id(), range);
        }
    }

    index
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
    use super::*;

    /// One entry per node in document order, carrying nesting depth, element
    /// name and attributes, or text. `ignored` is the synthetic identity
    /// attribute — the single difference the instrumented parse may have.
    fn dom_shape(document: &Html, ignored: &str) -> Vec<String> {
        document
            .tree
            .root()
            .descendants()
            .filter_map(|node| {
                let depth = node.ancestors().count();
                match node.value() {
                    Node::Element(element) => {
                        let attributes = element
                            .attrs()
                            .filter(|(name, _)| *name != ignored)
                            .map(|(name, value)| format!(" {name}={value:?}"))
                            .collect::<String>();
                        Some(format!("{depth}:<{}{attributes}>", element.name()))
                    }
                    Node::Text(text) => Some(format!("{depth}:#{}", &**text)),
                    _ => None,
                }
            })
            .collect()
    }

    /// The instrumented parse is only trustworthy if adding the synthetic
    /// attribute changes nothing else. Every attribute the HTML tree
    /// construction algorithm branches on is a specific authored name (`type` on
    /// `<input>` in a table, `color`/`face`/`size` on `<font>` in foreign
    /// content, `encoding` on `<annotation-xml>`), and the synthetic name is
    /// none of them — nor any authored name, by construction — so it can neither
    /// move a node nor shadow an authored attribute.
    #[test]
    fn instrumenting_changes_nothing_but_the_synthetic_attribute() {
        for source in [
            r#"<table><tr><td><div>CELL</div></td></tr><div>FOSTER</div></table>"#,
            r#"<table><input type="hidden" name="x"><tr><td>c</td></tr></table>"#,
            r#"<p><b>a<i>b</p>c</i>d"#,
            r#"<a href="/1">1<a href="/2">2"#,
            r#"<svg><font color="red">x</font><circle/>tail</svg>"#,
            r#"<math><annotation-xml encoding="text/html"><div>x</div></annotation-xml></math>"#,
            r#"<div/>trailing"#,
            r#"<select><option>a<option>b</select>"#,
            r#"<html lang="en"><body class="b"><p>x</p>"#,
            r#"<table><caption><table><tr><td>inner</td></tr></table></caption></table>"#,
            r#"<div data-lj-archiver-source-id="9"><table><tr><td>c</td></tr></table></div>"#,
        ] {
            let scanned = crate::html_scan::scan(source).unwrap();
            let identity = scanned.source_identity_attribute();
            let plain = Html::parse_fragment(source);
            let instrumented = Html::parse_fragment(&scanned.instrumented_html(source));

            assert_eq!(
                dom_shape(&instrumented, identity),
                dom_shape(&plain, identity),
                "instrumenting changed the tree for {source}"
            );
        }
    }

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
        let fragment = Html::parse_fragment(&scanned.instrumented_html(&html));
        let index = build_source_index(&fragment, &scanned);

        let divs: Vec<_> = fragment
            .tree
            .root()
            .descendants()
            .filter_map(ElementRef::wrap)
            .filter(|element| element.value().name() == "div")
            .map(|element| index.range(element.id()).expect("div has a source range"))
            .collect();

        assert_eq!(divs.len(), depth);
        assert_eq!(divs[0], 0..html.len());
        assert!(divs.windows(2).all(|pair| pair[0].start < pair[1].start));
    }
}
