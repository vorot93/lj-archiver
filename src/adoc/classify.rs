use scraper::ElementRef;

pub(crate) fn is_block(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "audio"
            | "canvas"
            | "center"
            | "details"
            | "dialog"
            | "dd"
            | "div"
            | "dl"
            | "dt"
            | "embed"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "header"
            | "hgroup"
            | "hr"
            | "main"
            | "menu"
            | "nav"
            | "noscript"
            | "object"
            | "script"
            | "section"
            | "style"
            | "summary"
            | "video"
    )
}

pub(crate) fn is_known_inline(name: &str) -> bool {
    matches!(
        name,
        "abbr"
            | "acronym"
            | "bdi"
            | "bdo"
            | "cite"
            | "data"
            | "dfn"
            | "font"
            | "kbd"
            | "mark"
            | "q"
            | "s"
            | "samp"
            | "small"
            | "span"
            | "strike"
            | "sub"
            | "sup"
            | "time"
            | "u"
            | "var"
            | "wbr"
            | "del"
            | "ins"
    )
}

pub(crate) fn is_top_level_inline(element: ElementRef<'_>) -> bool {
    let name = element.value().name();

    if name == "a" && has_block_output_descendant(element) {
        return false;
    }

    is_supported_inline(name)
        || (!is_top_level_block(name) && !has_block_or_passthrough_descendant(element))
}

fn is_top_level_block(name: &str) -> bool {
    matches!(name, "pre") || is_block_or_passthrough_boundary(name)
}

pub(crate) fn is_passthrough_block(element: ElementRef<'_>) -> bool {
    let name = element.value().name();
    matches!(name, "table" | "iframe") || is_block(name)
}

pub(crate) fn is_block_or_passthrough_boundary(name: &str) -> bool {
    matches!(
        name,
        "p" | "blockquote"
            | "ul"
            | "ol"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "table"
            | "iframe"
    ) || is_block(name)
}

pub(crate) fn has_block_or_passthrough_descendant(element: ElementRef<'_>) -> bool {
    element.descendants().skip(1).any(|descendant| {
        ElementRef::wrap(descendant)
            .is_some_and(|descendant| is_block_or_passthrough_boundary(descendant.value().name()))
    })
}

fn has_block_output_descendant(element: ElementRef<'_>) -> bool {
    element.descendants().skip(1).any(|descendant| {
        ElementRef::wrap(descendant).is_some_and(|descendant| {
            matches!(descendant.value().name(), "pre" | "script" | "style")
                || is_block_or_passthrough_boundary(descendant.value().name())
        })
    })
}

pub(crate) fn is_supported_inline(name: &str) -> bool {
    matches!(
        name,
        "a" | "br" | "img" | "strong" | "b" | "em" | "i" | "code"
    ) || is_known_inline(name)
}

/// LiveJournal chrome injected into the post body that is not post content:
/// the like/repost widget (`<div class="lj-like lj-like--v4" …>`) and the
/// generic user-mention userhead glyph (`<img class="i-ljuser-userhead">`).
pub(crate) fn is_stripped_chrome(element: ElementRef<'_>) -> bool {
    element.value().attr("class").is_some_and(|class| {
        class
            .split_whitespace()
            .any(|token| token == "lj-like" || token == "i-ljuser-userhead")
    })
}
