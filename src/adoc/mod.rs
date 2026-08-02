use crate::{
    links::LinkRewriter,
    models::{Comment, Post},
};
use chrono::SecondsFormat;
use scraper::{ElementRef, Html, node::Node};

mod classify;
mod escape;
mod source_html;

use classify::*;
use escape::*;
use source_html::*;

pub(crate) use escape::sanitize_macro_target;

pub fn render_post(
    post: &Post,
    include_comments: bool,
    link_rewriter: Option<&LinkRewriter>,
) -> String {
    let mut output = String::new();

    output.push_str("= ");
    output.push_str(&escape_title(&post.title));
    output.push_str("\n\n");

    push_attribute(&mut output, "lj-id", &post.id.to_string());
    push_attribute(&mut output, "lj-url", post.url.as_str());
    push_attribute(&mut output, "lj-author", &post.author);

    if let Some(poster) = &post.poster {
        push_attribute(&mut output, "lj-poster", poster);
    }

    if let Some(published_at) = post.published_at {
        push_attribute(&mut output, "lj-date", &format_datetime(published_at));
    }

    if !post.tags.is_empty() {
        push_attribute(&mut output, "lj-tags", &post.tags.join(", "));
    }

    output.push('\n');
    push_body(
        &mut output,
        &html_to_adoc_with_links(&post.body_html, link_rewriter),
    );

    if include_comments {
        for comment in &post.comments {
            render_comment(&mut output, comment, link_rewriter);
        }
    }

    output
}

fn render_comment(output: &mut String, comment: &Comment, link_rewriter: Option<&LinkRewriter>) {
    // Each block terminates with a single newline, so separate this comment from
    // the preceding block (the post body or an earlier comment) with a blank
    // line.
    if !output.ends_with("\n\n") {
        output.push('\n');
    }

    let title = comment_title(comment);

    if comment.depth <= 3 {
        let level = 3 + comment.depth;
        output.push_str(&"=".repeat(level));
        output.push(' ');
        output.push_str(&title);
        output.push_str("\n\n");
    } else {
        output.push('.');
        output.push_str(&title);
        output.push_str("\n\n");
    }

    if comment.deleted {
        output.push_str("_Deleted comment._\n");
    } else {
        push_body(
            output,
            &html_to_adoc_with_links(&comment.body_html, link_rewriter),
        );
    }
}

fn comment_title(comment: &Comment) -> String {
    let mut title = escape_title(&comment.author);

    if let Some(posted_at) = comment.posted_at {
        title.push_str(", ");
        title.push_str(&format_datetime(posted_at));
    }

    if let Some(subject) = &comment.subject {
        let subject = escape_title(subject);
        if !subject.is_empty() {
            title.push_str(" - ");
            title.push_str(&subject);
        }
    }

    title
}

fn push_attribute(output: &mut String, name: &str, value: &str) {
    output.push(':');
    output.push_str(name);
    output.push_str(": ");
    output.push_str(&escape_attribute_value(value));
    output.push('\n');
}

/// Append a rendered body as a block, terminated by a single newline. Blocks
/// are separated by a blank line inserted *before* each one (see `render_post`
/// and `render_comment`), so the composed document ends with exactly one
/// newline by construction — no trailing trim of the finished `.adoc`.
fn push_body(output: &mut String, body: &str) {
    output.push_str(&trim_trailing_hard_breaks(body));
    output.push('\n');
}

fn format_datetime(datetime: chrono::DateTime<chrono::Utc>) -> String {
    datetime.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn escape_title(value: &str) -> String {
    let title = value.replace(['\r', '\n'], " ");

    neutralize_plain_adoc_fragment_at_line_start(title.trim(), false)
}

pub(crate) fn sanitize_xref_label(value: &str) -> String {
    let normalized = value
        .split(|ch: char| matches!(ch, '|' | '[' | ']' | '\n' | '\r') || ch.is_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    neutralize_plain_adoc_fragment_at_line_start(&normalized, false)
}

fn escape_attribute_value(value: &str) -> String {
    let normalized = value.replace(['\r', '\n'], " ");
    neutralize_plain_adoc_fragment_at_line_start(&normalized, false)
}

pub fn html_to_adoc(html: &str) -> String {
    html_to_adoc_with_links(html, None)
}

pub fn html_to_adoc_with_links(html: &str, link_rewriter: Option<&LinkRewriter>) -> String {
    let fragment = Html::parse_fragment(html);
    let source_map = build_source_map(&fragment, html);
    let mut output = String::new();
    let mut inline_output = String::new();
    let mut plain_text = String::new();
    let ctx = RenderCtx {
        source_html: html,
        source_map: &source_map,
        depth: 0,
        list_depth: 0,
        link_rewriter,
    };

    let mut pending_breaks: usize = 0;

    for child in fragment.root_element().children() {
        match child.value() {
            Node::Text(text) => {
                if !text.trim().is_empty() {
                    flush_pending_line_breaks(
                        &mut inline_output,
                        &mut plain_text,
                        &mut pending_breaks,
                    );
                    append_normalized_text_fragment(&mut plain_text, text);
                }
            }
            Node::Element(_) => {
                if let Some(element) = ElementRef::wrap(child) {
                    if element.value().name() == "br" {
                        pending_breaks += 1;
                        continue;
                    }

                    flush_pending_line_breaks(
                        &mut inline_output,
                        &mut plain_text,
                        &mut pending_breaks,
                    );

                    let rendered = render_element(element, ctx);
                    if is_top_level_inline(element) {
                        if let Some(text) = plain_text_inline_content(element) {
                            append_inline_fragment(&mut plain_text, &text);
                        } else {
                            flush_plain_text_run(&mut inline_output, &mut plain_text);
                            neutralize_pending_plain_text_block_macro(&mut inline_output);
                            append_inline_fragment(&mut inline_output, &rendered);
                        }
                    } else {
                        flush_plain_text_run(&mut inline_output, &mut plain_text);
                        flush_root_inline(&mut output, &mut inline_output, true);
                        output.push_str(&rendered);
                    }
                }
            }
            _ => {}
        }
    }

    flush_plain_text_run(&mut inline_output, &mut plain_text);
    flush_root_inline(&mut output, &mut inline_output, false);

    output
}

const MAX_RENDER_DEPTH: usize = 256;

#[derive(Clone, Copy)]
struct RenderCtx<'a> {
    source_html: &'a str,
    source_map: &'a SourceMap,
    depth: usize,
    list_depth: usize,
    link_rewriter: Option<&'a LinkRewriter>,
}

impl RenderCtx<'_> {
    fn deeper(self) -> Self {
        Self {
            depth: self.depth + 1,
            ..self
        }
    }

    fn in_list(self) -> Self {
        Self {
            list_depth: self.list_depth + 1,
            ..self
        }
    }
}

fn render_element(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> String {
    if ctx.depth >= MAX_RENDER_DEPTH {
        return render_passthrough(element, ctx);
    }

    if is_stripped_chrome(element) {
        return String::new();
    }

    let name = element.value().name();

    match name {
        "p" => render_paragraph(element, ctx),
        "br" => " +\n".to_string(),
        "a" => render_link(element, ctx),
        "img" => render_image(element, ctx),
        "strong" | "b" => render_inline_wrapper(&render_inline_children(element, ctx), "*", "*"),
        "em" | "i" => render_inline_wrapper(&render_inline_children(element, ctx), "_", "_"),
        "code" => render_inline_code(element),
        "pre" => render_listing(element),
        "script" | "style" => render_safe_listing(element),
        "blockquote" => render_blockquote(element, ctx),
        "ul" => render_list(element, "*", ctx),
        "ol" => render_list(element, ".", ctx),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => render_heading(element, name, ctx),
        "table" | "iframe" => render_passthrough(element, ctx),
        _ if is_block(name) => render_passthrough(element, ctx),
        _ if !is_supported_inline(name) && has_block_or_passthrough_descendant(element) => {
            render_passthrough(element, ctx)
        }
        _ => render_inline_children(element, ctx),
    }
}

fn render_paragraph(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> String {
    let content = render_inline_children(element, ctx);
    let trimmed = trim_trailing_hard_breaks(content.trim_start());

    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n\n")
    }
}

fn render_inline_children(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> String {
    render_inline_children_with_options(element, ctx, false)
}

fn render_inline_children_with_options(
    element: ElementRef<'_>,
    ctx: RenderCtx<'_>,
    skip_direct_lists: bool,
) -> String {
    let mut output = String::new();
    let mut plain_text = String::new();
    let mut pending_breaks: usize = 0;

    for child in element.children() {
        match child.value() {
            Node::Text(text) => {
                if text.trim().is_empty() && pending_breaks > 0 {
                    continue;
                }
                flush_pending_line_breaks(&mut output, &mut plain_text, &mut pending_breaks);
                append_normalized_text_fragment(&mut plain_text, text);
            }
            Node::Element(_) => {
                if let Some(child_element) = ElementRef::wrap(child) {
                    if skip_direct_lists && matches!(child_element.value().name(), "ul" | "ol") {
                        continue;
                    }

                    if child_element.value().name() == "br" {
                        pending_breaks += 1;
                        continue;
                    }

                    flush_pending_line_breaks(&mut output, &mut plain_text, &mut pending_breaks);

                    if matches!(child_element.value().name(), "script" | "style") {
                        flush_plain_text_run(&mut output, &mut plain_text);
                        append_inline_block(
                            &mut output,
                            &render_element(child_element, ctx.deeper()),
                        );
                    } else if is_passthrough_block(child_element) {
                        flush_plain_text_run(&mut output, &mut plain_text);
                        append_inline_block(&mut output, &render_passthrough(child_element, ctx));
                    } else if is_block_or_passthrough_boundary(child_element.value().name()) {
                        flush_plain_text_run(&mut output, &mut plain_text);
                        append_inline_block(
                            &mut output,
                            &render_element(child_element, ctx.deeper()),
                        );
                    } else if !is_supported_inline(child_element.value().name())
                        && has_block_or_passthrough_descendant(child_element)
                    {
                        flush_plain_text_run(&mut output, &mut plain_text);
                        append_inline_block(&mut output, &render_passthrough(child_element, ctx));
                    } else if let Some(text) = plain_text_inline_content(child_element) {
                        append_inline_fragment(&mut plain_text, &text);
                    } else {
                        flush_plain_text_run(&mut output, &mut plain_text);
                        neutralize_pending_plain_text_block_macro(&mut output);
                        append_inline_fragment(
                            &mut output,
                            &render_element(child_element, ctx.deeper()),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    flush_plain_text_run(&mut output, &mut plain_text);

    output
}

fn render_block_children(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> String {
    let mut output = String::new();

    for child in element.children() {
        match child.value() {
            Node::Text(text) => {
                if !text.trim().is_empty() {
                    append_plain_text_fragment(&mut output, text);
                }
            }
            Node::Element(_) => {
                if let Some(child_element) = ElementRef::wrap(child) {
                    output.push_str(&render_element(child_element, ctx.deeper()));
                }
            }
            _ => {}
        }
    }

    output
}

fn render_link(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> String {
    if let Some(href) = element.attr("href")
        && let Some(image) = lone_image_child(element)
    {
        return render_linked_image(image, href, ctx);
    }

    let text = render_inline_children(element, ctx);

    match element.attr("href") {
        Some(href) => {
            if should_preserve_unwrapped_inline_output(&text) {
                return text;
            }

            let macro_target = ctx.link_rewriter.map_or_else(
                || Some(sanitize_macro_target(href)),
                |rewriter| {
                    rewriter
                        .xref_target(href)
                        .or_else(|| rewriter.absolute_target(href))
                },
            );

            let Some(macro_target) = macro_target else {
                return sanitize_macro_label(&text);
            };

            render_inline_wrapper_with_label(
                &text,
                &sanitize_macro_label(&text),
                &format!("{macro_target}["),
                "]",
            )
        }
        None => text,
    }
}

fn render_image(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> String {
    match element.attr("src") {
        Some(src) => {
            let alt = sanitize_macro_label(element.attr("alt").unwrap_or_default());
            match image_macro_target(src, ctx) {
                Some(target) => format!("image::{target}[{alt}]"),
                None if !alt.is_empty() => alt,
                None => sanitize_macro_label(src),
            }
        }
        None => String::new(),
    }
}

fn image_macro_target(src: &str, ctx: RenderCtx<'_>) -> Option<String> {
    match ctx.link_rewriter {
        Some(_) if src.starts_with("../assets/") => Some(sanitize_macro_target(src)),
        Some(rewriter) => rewriter.absolute_target(src),
        None => Some(sanitize_macro_target(src)),
    }
}

/// The sole `<img>` an anchor wraps, ignoring whitespace-only text. Returns
/// `None` for anything else — multiple elements, a non-image element, a
/// stripped-chrome image, or an image without a `src` — so those anchors fall
/// through to normal link handling (a chrome-only anchor then renders empty).
fn lone_image_child(element: ElementRef<'_>) -> Option<ElementRef<'_>> {
    let mut image = None;

    for child in element.children() {
        match child.value() {
            Node::Text(text) if text.trim().is_empty() => continue,
            Node::Element(_) => {
                let child = ElementRef::wrap(child)?;
                if image.is_none()
                    && child.value().name() == "img"
                    && child.value().attr("src").is_some()
                    && !is_stripped_chrome(child)
                {
                    image = Some(child);
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }

    image
}

/// Renders a link that wraps a single image as an inline image with a `link=`
/// attribute (`image:target[alt,link=href]`). The link target additionally
/// percent-encodes commas so the URL cannot split the macro's attribute list.
fn render_linked_image(image: ElementRef<'_>, href: &str, ctx: RenderCtx<'_>) -> String {
    let src = image.value().attr("src").unwrap_or_default();
    let alt = sanitize_macro_label(image.value().attr("alt").unwrap_or_default());
    let Some(target) = image_macro_target(src, ctx) else {
        return if alt.is_empty() {
            sanitize_macro_label(src)
        } else {
            alt
        };
    };

    let link = ctx.link_rewriter.map_or_else(
        || Some(sanitize_macro_target(href)),
        |rewriter| {
            rewriter
                .xref_target(href)
                .or_else(|| rewriter.absolute_target(href))
        },
    );

    let Some(link) = link else {
        return format!("image:{target}[{alt}]");
    };
    let link = link.replace(',', "%2C");

    format!("image:{target}[{alt},link={link}]")
}

fn render_blockquote(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> String {
    let children = render_block_children(element, ctx);
    let content = trim_trailing_hard_breaks(&trim_leading_hard_breaks(&children));
    let delimiter = safe_block_delimiter(&content, '_');

    format!("[quote]\n{delimiter}\n{content}\n{delimiter}\n\n")
}

fn render_listing(element: ElementRef<'_>) -> String {
    let content = raw_text(element).trim_matches('\n').to_string();
    let delimiter = safe_block_delimiter(&content, '-');

    format!("{delimiter}\n{content}\n{delimiter}\n\n")
}

fn render_safe_listing(element: ElementRef<'_>) -> String {
    let content = neutralize_active_adoc_lines(raw_text(element).trim_matches('\n'));
    let delimiter = safe_block_delimiter(&content, '-');

    format!("{delimiter}\n{content}\n{delimiter}\n\n")
}

fn render_inline_code(element: ElementRef<'_>) -> String {
    let content = raw_text(element).trim_matches('\n').to_string();

    if !content.contains('`') {
        return format!("`{content}`");
    }

    if !content.contains('+') {
        return format!("+{content}+");
    }

    if !content.contains(']') {
        return format!("pass:q[{content}]");
    }

    format!("pass:q[{}]", content.replace(']', "\\]"))
}

fn render_list(element: ElementRef<'_>, marker: &str, ctx: RenderCtx<'_>) -> String {
    let mut output = String::new();
    let marker = marker.repeat(ctx.list_depth + 1);

    for child in element.children() {
        let Some(child_node) = child.value().as_element() else {
            continue;
        };

        if child_node.name() == "li" {
            let Some(child_element) = ElementRef::wrap(child) else {
                continue;
            };

            output.push_str(&marker);
            output.push(' ');
            output.push_str(&trim_trailing_hard_breaks(
                render_inline_children_with_options(child_element, ctx, true).trim_start(),
            ));
            output.push('\n');

            for list_child in child_element.children() {
                let Some(list_element) = ElementRef::wrap(list_child) else {
                    continue;
                };

                match list_element.value().name() {
                    "ul" => output.push_str(&render_element(list_element, ctx.in_list().deeper())),
                    "ol" => output.push_str(&render_element(list_element, ctx.in_list().deeper())),
                    _ => {}
                }
            }
        }
    }

    if !output.is_empty() && !output.ends_with("\n\n") {
        output.push('\n');
    }

    output
}

fn render_heading(element: ElementRef<'_>, name: &str, ctx: RenderCtx<'_>) -> String {
    let level = name
        .strip_prefix('h')
        .and_then(|level| level.parse::<usize>().ok())
        .unwrap_or(1)
        + 1;

    format!(
        "{} {}\n\n",
        "=".repeat(level),
        trim_trailing_hard_breaks(render_inline_children(element, ctx).trim_start())
    )
}

fn plain_text_inline_content(element: ElementRef<'_>) -> Option<String> {
    plain_text_inline_content_at(element, 0)
}

fn plain_text_inline_content_at(element: ElementRef<'_>, depth: usize) -> Option<String> {
    // Bound this self-recursion the same way `render_element` is bounded. At the
    // cap we return `None`, so both callers (`render_inline_children` and
    // `html_to_adoc`) fall through to the already-capped `render_element` path
    // instead of overflowing the stack on deeply nested inline wrappers.
    if depth >= MAX_RENDER_DEPTH
        || is_block_or_passthrough_boundary(element.value().name())
        || !is_plain_text_inline_wrapper(element.value().name())
    {
        return None;
    }

    let mut output = String::new();

    for child in element.children() {
        match child.value() {
            Node::Text(text) => append_normalized_text_fragment(&mut output, text),
            Node::Element(_) => {
                let child_element = ElementRef::wrap(child)?;
                let text = plain_text_inline_content_at(child_element, depth + 1)?;
                append_inline_fragment(&mut output, &text);
            }
            _ => {}
        }
    }

    Some(output)
}

fn is_plain_text_inline_wrapper(name: &str) -> bool {
    is_known_inline(name) || (!is_supported_inline(name) && !is_block_or_passthrough_boundary(name))
}

fn flush_root_inline(output: &mut String, inline_output: &mut String, before_block: bool) {
    let inline = inline_output.trim();
    if inline.is_empty() {
        inline_output.clear();
        return;
    }

    if output.is_empty() {
        output.push_str(inline);
        if before_block {
            output.push_str("\n\n");
        }
    } else {
        let trimmed_output = output.trim_end().len();
        output.truncate(trimmed_output);
        output.push_str("\n\n");
        output.push_str(inline);
        output.push_str("\n\n");
    }

    inline_output.clear();
}

fn append_inline_fragment(output: &mut String, fragment: &str) {
    let mut characters = fragment.chars().peekable();

    if output.ends_with(' ') || output.ends_with('\n') {
        while matches!(characters.peek(), Some(' ')) {
            characters.next();
        }
    }

    output.extend(characters);
}

fn append_plain_text_fragment(output: &mut String, text: &str) {
    let mut plain_text = String::new();
    append_normalized_text_fragment(&mut plain_text, text);
    flush_plain_text_run(output, &mut plain_text);
}

fn append_normalized_text_fragment(output: &mut String, text: &str) {
    let normalized = normalize_inline_text(text);

    append_inline_fragment(output, &normalized);
}

fn flush_plain_text_run(output: &mut String, plain_text: &mut String) {
    if plain_text.is_empty() {
        return;
    }

    let fragment = neutralize_plain_adoc_fragment_at_line_start(
        plain_text,
        output.is_empty() || output.ends_with('\n'),
    );

    append_inline_fragment(output, &fragment);
    plain_text.clear();
}

/// Flush a run of `<br>` elements buffered by the child-iteration loops in
/// `html_to_adoc` and `render_inline_children`. LiveJournal uses a lone `<br>`
/// for an in-paragraph line break and a run of two or more (possibly with
/// whitespace-only text nodes between them) for a paragraph gap, so a lone
/// pending break reproduces today's hard-break emission (` +\n`) exactly,
/// while a run of two or more collapses into a single paragraph break (a
/// blank line, no trailing `+`).
fn flush_pending_line_breaks(
    output: &mut String,
    plain_text: &mut String,
    pending_breaks: &mut usize,
) {
    if *pending_breaks == 0 {
        return;
    }

    flush_plain_text_run(output, plain_text);

    if *pending_breaks == 1 {
        // A hard break separates content; with nothing before it on the current
        // line (`output` empty, or already ended by a newline) it would leave a
        // stray ` +`/`+` — its leading space is later trimmed to a bare `+`.
        // Drop it, symmetric to trailing-hard-break trimming. The following
        // content still lands at a line start, so block-macro neutralization
        // (which keys off the same line-start condition) is unaffected.
        if !output.is_empty() && !output.ends_with('\n') {
            neutralize_pending_plain_text_block_macro(output);
            append_inline_fragment(output, " +\n");
        }
    } else {
        let trimmed_len = output.trim_end().len();
        output.truncate(trimmed_len);
        output.push_str("\n\n");
    }

    *pending_breaks = 0;
}

/// Trim trailing whitespace and any trailing AsciiDoc hard-break markers (` +`,
/// or a lone `+` line) that end `text` with no following content. A `+` counts
/// as a hard break only when preceded by a space or a line start, so real
/// trailing text such as `C++` is kept, and a hard break that is followed by
/// content is untouched because it is not trailing.
fn trim_trailing_hard_breaks(text: &str) -> String {
    let mut end = text.trim_end();

    while let Some(stripped) = end.strip_suffix('+') {
        if stripped.is_empty() || stripped.ends_with([' ', '\n']) {
            end = stripped.trim_end();
        } else {
            break;
        }
    }

    end.to_string()
}

/// Trim leading whitespace and any leading AsciiDoc hard-break markers (a lone
/// `+` line) that begin `text` with no preceding content — symmetric to
/// [`trim_trailing_hard_breaks`]. A `+` is a hard break only when it is alone on
/// a line (immediately followed by a newline), so real leading text such as
/// `+1` is kept. Used on block-children output (blockquotes), which emits hard
/// breaks directly rather than through [`flush_pending_line_breaks`].
fn trim_leading_hard_breaks(text: &str) -> String {
    let mut start = text.trim_start();

    while let Some(rest) = start.strip_prefix('+') {
        if rest.starts_with('\n') {
            start = rest.trim_start();
        } else {
            break;
        }
    }

    start.to_string()
}

fn append_inline_block(output: &mut String, block: &str) {
    let block = block.trim_end();
    if block.is_empty() {
        return;
    }

    let trimmed_output = output.trim_end().len();
    output.truncate(trimmed_output);

    if !output.is_empty() {
        output.push_str("\n\n");
    }

    output.push_str(block);
    output.push_str("\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::links::LinkRewriter;
    use std::collections::HashSet;
    use url::Url;

    const USER_FRAGMENT: &str = "image:evil.png[] xref:1.adoc[x] {secret}";

    #[test]
    fn converts_basic_inline_html() {
        let html = r#"<p>Hello <strong>bold</strong> and <em>em</em> <a href="https://example.com">link</a>.</p>"#;
        assert_eq!(
            html_to_adoc(html),
            "Hello *bold* and _em_ https://example.com[link].\n\n"
        );
    }

    #[test]
    fn converts_images_lists_blockquotes_and_pre() {
        let html = r#"
            <blockquote><p>quoted</p></blockquote>
            <ul><li>one</li><li>two</li></ul>
            <pre>let x = 1;</pre>
            <p><img src="../assets/166511/image-1.jpg" alt="alt text"></p>
        "#;
        let adoc = html_to_adoc(html);
        assert!(adoc.contains("[quote]\n____\nquoted\n____"));
        assert!(adoc.contains("* one\n* two"));
        assert!(adoc.contains("----\nlet x = 1;\n----"));
        assert!(adoc.contains("image::../assets/166511/image-1.jpg[alt text]"));
    }

    #[test]
    fn preserves_nested_unordered_list_depth() {
        assert_eq!(
            html_to_adoc("<ul><li>outer<ul><li>inner</li></ul></li></ul>"),
            "* outer\n** inner\n\n"
        );
    }

    #[test]
    fn preserves_nested_ordered_list_depth() {
        assert_eq!(
            html_to_adoc("<ol><li>outer<ol><li>inner</li></ol></li></ol>"),
            ". outer\n.. inner\n\n"
        );
    }

    #[test]
    fn preserves_nested_mixed_unordered_ordered_list_depth() {
        assert_eq!(
            html_to_adoc("<ul><li>outer<ol><li>inner</li></ol></li></ul>"),
            "* outer\n.. inner\n\n"
        );
    }

    #[test]
    fn preserves_nested_mixed_ordered_unordered_list_depth() {
        assert_eq!(
            html_to_adoc("<ol><li>outer<ul><li>inner</li></ul></li></ol>"),
            ". outer\n** inner\n\n"
        );
    }

    #[test]
    fn uses_safe_listing_delimiter_when_pre_contains_default_delimiter() {
        let adoc = html_to_adoc("<pre>before\n----\nafter</pre>");

        assert!(adoc.contains("before\n----\nafter"));
        assert!(adoc.contains("-----\nbefore\n----\nafter\n-----"));
        assert!(!adoc.starts_with("----\nbefore\n----\nafter\n----"));
    }

    #[test]
    fn uses_safe_quote_delimiter_when_blockquote_contains_default_delimiter() {
        let adoc = html_to_adoc("<blockquote><pre>____\ninclude::secret[]</pre></blockquote>");

        assert!(adoc.starts_with("[quote]\n_____\n----\n____\ninclude::secret[]\n----"));
        assert!(adoc.ends_with("\n_____\n\n"));

        let lines = adoc.lines().collect::<Vec<_>>();
        let closing_delimiter = lines
            .iter()
            .rposition(|line| *line == "_____")
            .expect("quote should use a grown delimiter");
        assert!(
            !lines
                .iter()
                .skip(closing_delimiter + 1)
                .any(|line| line.starts_with("include::secret[]"))
        );
        assert!(!adoc.starts_with("[quote]\n____\n----\n____\ninclude::secret[]"));
    }

    #[test]
    fn uses_safe_quote_delimiter_for_trimmed_blockquote_content() {
        let adoc = html_to_adoc("<blockquote> ____ </blockquote>");

        assert_eq!(adoc, "[quote]\n_____\n____\n_____\n\n");
    }

    #[test]
    fn preserves_tables_as_passthrough() {
        let html = "<table><tr><td>x</td></tr></table>";
        assert_eq!(
            html_to_adoc(html),
            "++++\n<table><tr><td>x</td></tr></table>\n++++\n\n"
        );
    }

    #[test]
    fn comment_tags_do_not_shift_passthrough_source_spans() {
        let html = "<p><!-- <div>fake</div> -->intro</p><div>real</div>";
        assert_eq!(
            html_to_adoc(html),
            "intro\n\n++++\n<div>real</div>\n++++\n\n"
        );
    }

    #[test]
    fn malformed_space_after_tag_open_does_not_shift_real_div_source() {
        assert_eq!(
            html_to_adoc("< div>fake</div><div data-real='yes'>real</div>"),
            "< div>fake\n\n++++\n<div data-real='yes'>real</div>\n++++\n\n"
        );
    }

    #[test]
    fn tokenizer_valid_punctuation_in_tag_name_does_not_shift_real_div_source() {
        assert_eq!(
            html_to_adoc("<div!>fake</div!><div data-real='yes'>real</div>"),
            "fake\n\n++++\n<div data-real='yes'>real</div>\n++++\n\n"
        );
    }

    #[test]
    fn quote_in_attribute_name_does_not_hide_tag_end_or_shift_real_div_source() {
        let input = "<div \">fake</div><div data-real='yes'>real</div>";
        let expected = concat!(
            "++++\n<div \">fake</div>\n++++\n\n",
            "++++\n<div data-real='yes'>real</div>\n++++\n\n"
        );
        let adoc = html_to_adoc(input);

        assert_eq!(adoc, expected);
        assert_eq!(adoc.matches("fake").count(), 1);
        assert_eq!(adoc.matches(">real</div>").count(), 1);
    }

    #[test]
    fn nul_in_tag_name_does_not_shift_following_real_div_source() {
        let input = "<div\0x>fake</div\0x><div data-real='yes'>real</div>";
        let expected = "fake\n\n++++\n<div data-real='yes'>real</div>\n++++\n\n";
        let adoc = html_to_adoc(input);

        assert_eq!(adoc, expected);
        assert_eq!(adoc.matches("fake").count(), 1);
        assert_eq!(adoc.matches(">real</div>").count(), 1);
    }

    #[test]
    fn raw_text_tags_do_not_shift_passthrough_source_spans() {
        let html = concat!(
            "<script>const x = \"<table>fake</table>\";</script>",
            "<table><tr><td>real</td></tr></table>"
        );
        let adoc = html_to_adoc(html);
        assert_eq!(
            adoc,
            "----\nconst x = \"<table>fake</table>\";\n----\n\n++++\n<table><tr><td>real</td></tr></table>\n++++\n\n"
        );
    }

    #[test]
    fn raw_text_end_tag_prefix_requires_appropriate_boundary() {
        let html = concat!(
            "<script>const x = \"</scripture><table>fake</table>\";</script>",
            "<table data-kind='real'><tr><td>real</td></tr></table>"
        );
        assert_eq!(
            html_to_adoc(html),
            concat!(
                "----\n",
                "const x = \"</scripture><table>fake</table>\";\n",
                "----\n\n",
                "++++\n",
                "<table data-kind='real'><tr><td>real</td></tr></table>\n",
                "++++\n\n"
            )
        );
    }

    #[test]
    fn abrupt_comment_end_preserves_following_source_span() {
        let html = "<!--><div data-kind='abrupt'>real</div>";
        assert_eq!(
            html_to_adoc(html),
            "++++\n<div data-kind='abrupt'>real</div>\n++++\n\n"
        );
    }

    #[test]
    fn bang_comment_end_preserves_following_source_span() {
        let html = "<!-- comment --!><div data-kind='bang'>real</div>";
        assert_eq!(
            html_to_adoc(html),
            "++++\n<div data-kind='bang'>real</div>\n++++\n\n"
        );
    }

    #[test]
    fn uses_safe_passthrough_delimiter_when_html_contains_default_delimiter() {
        let html = "<table><tr><td>before\n++++\nafter</td></tr></table>";
        let adoc = html_to_adoc(html);

        assert!(adoc.contains("<td>before\n++++\nafter</td>"));
        assert!(
            adoc.contains("+++++\n<table><tr><td>before\n++++\nafter</td></tr></table>\n+++++")
        );
        assert!(
            !adoc.starts_with("++++\n<table><tr><td>before\n++++\nafter</td></tr></table>\n++++")
        );
    }

    #[test]
    fn preserves_table_tbody_as_passthrough() {
        let html = "<table><tbody><tr><td>x</td></tr></tbody></table>";
        assert_eq!(
            html_to_adoc(html),
            "++++\n<table><tbody><tr><td>x</td></tr></tbody></table>\n++++\n\n"
        );
    }

    #[test]
    fn collapses_inline_whitespace_across_element_boundaries() {
        let html = "<p>Hello <span> bold </span> world</p>";
        assert_eq!(html_to_adoc(html), "Hello bold world\n\n");
    }

    #[test]
    fn collapses_top_level_inline_whitespace_across_element_boundaries() {
        let html = "<strong>Hello </strong> world";
        let adoc = html_to_adoc(html);

        assert_eq!(adoc, "*Hello* world");
        assert!(!adoc.contains("  "));
    }

    #[test]
    fn keeps_top_level_inline_before_block_on_block_boundary() {
        let html = "<strong>intro</strong><p>body</p>";

        assert_eq!(html_to_adoc(html), "*intro*\n\nbody\n\n");
    }

    #[test]
    fn keeps_top_level_text_before_block_on_block_boundary() {
        let html = "text<p>body</p>";

        assert_eq!(html_to_adoc(html), "text\n\nbody\n\n");
    }

    #[test]
    fn keeps_trailing_root_text_between_blocks_on_block_boundary() {
        let html = "<p>one</p> trailing <p>two</p>";

        assert_eq!(html_to_adoc(html), "one\n\ntrailing\n\ntwo\n\n");
    }

    #[test]
    fn renders_top_level_unknown_inline_html_as_text() {
        let adoc = html_to_adoc("<abbr>x</abbr>");

        assert!(adoc.contains('x'));
        assert!(!adoc.contains("++++"));
    }

    #[test]
    fn renders_top_level_font_html_as_text() {
        let adoc = html_to_adoc(r#"<font color="red">text</font>"#);

        assert!(adoc.contains("text"));
        assert!(!adoc.contains("++++"));
    }

    #[test]
    fn preserves_backticks_inside_inline_code() {
        let adoc = html_to_adoc("<p><code>a`b</code></p>");

        assert_eq!(adoc, "+a`b+\n\n");
    }

    #[test]
    fn preserves_iframe_source_attribute_quotes_as_passthrough() {
        let html = "<iframe src='x'></iframe>";

        assert_eq!(
            html_to_adoc(html),
            "++++\n<iframe src='x'></iframe>\n++++\n\n"
        );
    }

    #[test]
    fn preserves_embed_source_attribute_quotes_as_passthrough() {
        let html = "<embed src='x'>";

        assert_eq!(html_to_adoc(html), "++++\n<embed src='x'>\n++++\n\n");
    }

    #[test]
    fn preserves_repeated_embeds_as_distinct_passthrough() {
        let html = "<embed src='one'><embed src='two'>";
        let adoc = html_to_adoc(html);

        assert_eq!(adoc.matches("<embed src='one'>").count(), 1);
        assert_eq!(adoc.matches("<embed src='two'>").count(), 1);
        assert!(!adoc.contains("<embed src='one'>\n++++\n\n++++\n<embed src='one'>"));
    }

    #[test]
    fn preserves_mixed_quote_repeated_iframes_as_distinct_passthrough() {
        let html = "<iframe src='x'></iframe><iframe src=\"x\"></iframe>";
        let adoc = html_to_adoc(html);

        assert_eq!(adoc.matches("<iframe src='x'></iframe>").count(), 1);
        assert_eq!(adoc.matches("<iframe src=\"x\"></iframe>").count(), 1);
        assert_ne!(adoc.matches("<iframe src=\"x\"></iframe>").count(), 2);
        assert!(
            !adoc.contains("<iframe src='x'></iframe>\n++++\n\n++++\n<iframe src='x'></iframe>")
        );
    }

    #[test]
    fn preserves_repeated_single_quoted_iframes_as_distinct_passthrough() {
        let html = "<iframe src='first'></iframe><iframe src='second'></iframe>";
        let adoc = html_to_adoc(html);

        assert_eq!(adoc.matches("<iframe src='first'></iframe>").count(), 1);
        assert_eq!(adoc.matches("<iframe src='second'></iframe>").count(), 1);
        assert!(!adoc.contains(
            "<iframe src='first'></iframe>\n++++\n\n++++\n<iframe src='first'></iframe>"
        ));
    }

    #[test]
    fn preserves_table_source_entities_as_passthrough() {
        let html = "<table><tr><td>&#160;</td></tr></table>";

        assert_eq!(
            html_to_adoc(html),
            "++++\n<table><tr><td>&#160;</td></tr></table>\n++++\n\n"
        );
    }

    #[test]
    fn self_closing_nonvoid_tag_preserves_following_content() {
        // `<div/>` is not self-closing in HTML5; html5ever nests the following
        // content into the div. Source recovery must not drop it.
        let out = html_to_adoc(r#"<div class="clear"/><p>keepme</p>"#);
        assert!(
            out.contains("keepme"),
            "content after a self-closing non-void tag must survive: {out:?}"
        );
    }

    #[test]
    fn strips_lj_like_widget_from_body() {
        let html = r#"<p>Real body.</p><div class="lj-like lj-like--v4" data-ditemid="32020" data-url="https://x.livejournal.com/32020.html" lj-screenable="social:likes:parse"></div>"#;
        let adoc = html_to_adoc(html);
        assert!(adoc.contains("Real body."));
        assert!(!adoc.contains("lj-like"));
        assert!(!adoc.contains("++++"));
    }

    #[test]
    fn strips_nested_lj_like_widget_reached_via_render_inline_children() {
        // `<strong>` is not closed by a following `<div>` the way `<p>` is, so with
        // no wrapping `<p>`, html5ever keeps the div genuinely nested inside
        // `<strong>`. render_element("strong") -> render_inline_children(strong)
        // then hits the `is_passthrough_block` branch for the div child and calls
        // render_passthrough directly, bypassing render_element's guard entirely -
        // so render_passthrough needs its own is_stripped_chrome guard too.
        let html = r#"<strong>before<div class="lj-like lj-like--v4"></div>after</strong>"#;
        let adoc = html_to_adoc(html);
        assert!(!adoc.contains("lj-like"), "got: {adoc:?}");
        assert!(!adoc.contains("++++"), "got: {adoc:?}");
        // Stripping the widget must not leave a spurious block gap or drop the
        // surrounding inline wrapper (append_inline_block skips empty content).
        assert_eq!(adoc, "*beforeafter*");
    }

    #[test]
    fn renders_user_mention_as_clean_username_link() {
        let html = r#"<span class="ljuser i-ljuser i-ljuser-type-P" data-ljuser="evo_lutio"><a href="https://evo-lutio.livejournal.com/profile/" class="i-ljuser-profile"><img class="i-ljuser-userhead" src="https://l-stat.livejournal.net/img/userinfo_v8.svg"></a><a href="https://evo-lutio.livejournal.com/" class="i-ljuser-username"><b>evo_lutio</b></a></span>"#;
        assert_eq!(
            html_to_adoc(html),
            "https://evo-lutio.livejournal.com/[*evo_lutio*]"
        );
    }

    #[test]
    fn renders_paragraph_unknown_inline_html_as_text() {
        let html = r#"<p>A <abbr title="example">x</abbr></p>"#;
        assert_eq!(html_to_adoc(html), "A x\n\n");
    }

    #[test]
    fn renders_top_level_plain_unknown_inline_html_as_text() {
        let adoc = html_to_adoc("<foo>x</foo>");

        assert_eq!(adoc, "x");
        assert!(!adoc.contains("++++"));
    }

    #[test]
    fn renders_top_level_custom_embed_like_unknown_inline_as_text() {
        let adoc = html_to_adoc("<lj-embed data-id='1'>x</lj-embed>");

        assert_eq!(adoc, "x");
        assert!(!adoc.contains("++++"));
    }

    #[test]
    fn renders_attributed_unknown_inline_html_as_text() {
        let adoc = html_to_adoc(r#"<foo data-x="1">x</foo>"#);

        assert_eq!(adoc, "x");
        assert!(!adoc.contains("++++"));
    }

    #[test]
    fn keeps_top_level_plain_unknown_inline_child_text_inline() {
        assert_eq!(html_to_adoc("a<foo>b</foo>c"), "abc");
    }

    #[test]
    fn keeps_top_level_attributed_unknown_inline_child_text_inline() {
        assert_eq!(html_to_adoc("a<foo data-x='1'>b</foo>c"), "abc");
    }

    #[test]
    fn keeps_top_level_custom_unknown_inline_child_text_inline() {
        assert_eq!(html_to_adoc("a<lj-embed data-id='1'>b</lj-embed>c"), "abc");
    }

    #[test]
    fn preserves_separator_before_strong_text() {
        let html = "<p>Hello<strong> world</strong></p>";
        assert_eq!(html_to_adoc(html), "Hello *world*\n\n");
    }

    #[test]
    fn preserves_separator_before_link_text() {
        let html = r#"<p>Hello<a href="https://example.com"> world</a></p>"#;
        assert_eq!(html_to_adoc(html), "Hello https://example.com[world]\n\n");
    }

    #[test]
    fn preserves_unknown_block_elements_as_passthrough() {
        let html = "<ul><li>before<section><p>unknown block</p></section>after</li></ul>";
        let adoc = html_to_adoc(html);

        assert!(adoc.contains("* before"));
        assert!(adoc.contains("++++\n<section><p>unknown block</p></section>\n++++"));
        assert!(adoc.contains("after"));
    }

    #[test]
    fn preserves_additional_standard_block_elements_as_passthrough() {
        for html in [
            "<menu><li>x</li></menu>",
            "<noscript><p>x</p></noscript>",
            "<hgroup><h1>x</h1></hgroup>",
            r#"<canvas data-id="1">x</canvas>"#,
        ] {
            let adoc = html_to_adoc(html);

            assert_eq!(adoc, format!("++++\n{html}\n++++\n\n"));
        }
    }

    #[test]
    fn keeps_unknown_inline_elements_as_text() {
        for html in [
            r#"<lj-embed data-id="1">x</lj-embed>"#,
            r#"<foo data-id="1">x</foo>"#,
        ] {
            let adoc = html_to_adoc(html);

            assert_eq!(adoc, "x");
            assert!(!adoc.contains("++++"));
        }
    }

    #[test]
    fn preserves_table_passthrough_inside_inline_wrapper() {
        let html = "<p><span><table><tr><td>x</td></tr></table></span></p>";
        let adoc = html_to_adoc(html);

        assert_eq!(adoc, "++++\n<table><tr><td>x</td></tr></table>\n++++\n\n");
        assert_ne!(adoc, "x\n\n");
    }

    #[test]
    fn preserves_table_passthrough_inside_strong_wrapper() {
        let html = "<p><strong><table><tr><td>x</td></tr></table></strong></p>";
        let adoc = html_to_adoc(html);

        assert!(adoc.lines().any(|line| line == "++++"));
        assert!(!adoc.lines().any(|line| line == "*++++" || line == "++++*"));
        assert_eq!(adoc, "++++\n<table><tr><td>x</td></tr></table>\n++++\n\n");
    }

    #[test]
    fn preserves_iframe_passthrough_inside_inline_wrapper() {
        let html = r#"<p><span><iframe src="x"></iframe></span></p>"#;
        let adoc = html_to_adoc(html);

        assert_eq!(adoc, "++++\n<iframe src=\"x\"></iframe>\n++++\n\n");
        assert_ne!(adoc, "\n\n");
    }

    #[test]
    fn preserves_iframe_passthrough_inside_em_wrapper() {
        let html = r#"<p><em><iframe src="x"></iframe></em></p>"#;
        let adoc = html_to_adoc(html);

        assert!(adoc.lines().any(|line| line == "++++"));
        assert!(!adoc.lines().any(|line| line == "_++++" || line == "++++_"));
        assert_eq!(adoc, "++++\n<iframe src=\"x\"></iframe>\n++++\n\n");
    }

    #[test]
    fn preserves_script_listing_inside_strong_wrapper() {
        let html = "<p><strong><script>include::x[]</script></strong></p>";
        let adoc = html_to_adoc(html);

        assert!(adoc.lines().any(|line| line == "----"));
        assert!(!adoc.lines().any(|line| line == "*----" || line == "----*"));
        assert!(!adoc.lines().any(|line| line.starts_with("include::x[]")));
        assert!(adoc.contains("----\n\\include::x[]\n----"));
    }

    #[test]
    fn preserves_table_passthrough_inside_link_wrapper() {
        let html = r#"<a href="x"><table><tr><td>y</td></tr></table></a>"#;
        let adoc = html_to_adoc(html);

        assert_eq!(adoc, "++++\n<table><tr><td>y</td></tr></table>\n++++\n\n");
        assert!(!adoc.contains("x[++++"));
        assert!(!adoc.contains("++++]"));
    }

    #[test]
    fn preserves_iframe_passthrough_inside_link_wrapper() {
        let html = r#"<a href="x"><iframe src="y"></iframe></a>"#;
        let adoc = html_to_adoc(html);

        assert_eq!(adoc, "++++\n<iframe src=\"y\"></iframe>\n++++\n\n");
        assert!(!adoc.contains("x[++++"));
        assert!(!adoc.contains("++++]"));
    }

    #[test]
    fn preserves_script_listing_inside_link_wrapper() {
        let html = r#"<a href="x"><script>include::x[]</script></a>"#;
        let adoc = html_to_adoc(html);

        assert_eq!(adoc, "----\n\\include::x[]\n----\n\n");
        assert!(!adoc.contains("x[----"));
        assert!(!adoc.lines().any(|line| line.starts_with("include::x[]")));
    }

    #[test]
    fn keeps_normal_inline_link_behavior() {
        let html = r#"<a href="https://example.com">x</a>"#;

        assert_eq!(html_to_adoc(html), "https://example.com[x]");
    }

    #[test]
    fn renders_linked_image_as_inline_image_with_link() {
        let html = r#"<a href="https://ic.pics.livejournal.com/evo_lutio/1/original.jpg"><img src="../assets/1d3802f42770.jpg" alt="photo"></a>"#;
        assert_eq!(
            html_to_adoc(html),
            "image:../assets/1d3802f42770.jpg[photo,link=https://ic.pics.livejournal.com/evo_lutio/1/original.jpg]"
        );
    }

    #[test]
    fn renders_linked_icon_without_alt_as_inline_image() {
        let html = r#"<a href="https://facebook.com/psychoalchemy.ru/"><img src="../assets/8f13bfc694cf.bin"></a>"#;
        assert_eq!(
            html_to_adoc(html),
            "image:../assets/8f13bfc694cf.bin[,link=https://facebook.com/psychoalchemy.ru/]"
        );
    }

    #[test]
    fn keeps_normal_strong_inline_formatting() {
        assert_eq!(html_to_adoc("<strong>bold</strong>"), "*bold*");
    }

    #[test]
    fn renders_list_block_inside_inline_wrapper() {
        let html = "<p><span><ul><li>x</li></ul></span></p>";
        let adoc = html_to_adoc(html);

        assert_eq!(adoc, "* x\n\n");
        assert_ne!(adoc, "x\n\n");
    }

    #[test]
    fn preserves_unknown_element_with_block_child_as_passthrough() {
        let html = "<foo><p>x</p></foo>";
        let adoc = html_to_adoc(html);

        assert_eq!(adoc, "++++\n<foo><p>x</p></foo>\n++++\n\n");
        assert_ne!(adoc, "x");
    }

    #[test]
    fn keeps_iframe_passthrough_on_block_boundaries() {
        let html = r#"<p>before <iframe src="x"></iframe> after</p>"#;
        let adoc = html_to_adoc(html);

        assert!(!adoc.contains("before ++++\n"));
        assert!(!adoc.contains("++++\n after"));
        assert!(adoc.contains("\n++++\n<iframe src=\"x\"></iframe>\n++++\n"));
    }

    #[test]
    fn keeps_center_passthrough_on_block_boundaries() {
        let html = "<p>before<center>x</center>after</p>";
        let adoc = html_to_adoc(html);

        assert!(!adoc.contains("beforexafter"));
        assert!(adoc.contains("before\n\n++++\n<center>x</center>\n++++\n\nafter"));
    }

    #[test]
    fn keeps_object_passthrough_on_block_boundaries() {
        let html = r#"<p>before<object data="x"></object>after</p>"#;
        let adoc = html_to_adoc(html);

        assert!(!adoc.contains("beforeafter"));
        assert!(adoc.contains("before\n\n++++\n<object data=\"x\"></object>\n++++\n\nafter"));
    }

    #[test]
    fn renders_custom_inline_embed_as_text_on_inline_boundaries() {
        let html = r#"<p>before<lj-embed data-id="1">x</lj-embed>after</p>"#;
        let adoc = html_to_adoc(html);

        assert_eq!(adoc, "beforexafter\n\n");
        assert!(!adoc.contains("++++"));
    }

    #[test]
    fn keeps_table_passthrough_out_of_list_item_line() {
        let html = "<ul><li>before<table><tr><td>x</td></tr></table>after</li></ul>";
        let adoc = html_to_adoc(html);

        assert!(!adoc.contains("* before++++\n"));
        assert!(!adoc.contains("* before ++++\n"));
        assert!(adoc.contains("\n++++\n<table><tr><td>x</td></tr></table>\n++++\n"));
    }

    #[test]
    fn documents_passthrough_blocks_inside_list_items_as_shallow_list_boundary() {
        let html = "<ul><li>before<table><tr><td>x</td></tr></table>after</li></ul>";
        let adoc = html_to_adoc(html);

        assert!(adoc.contains("* before"));
        assert!(adoc.contains("\n++++\n<table><tr><td>x</td></tr></table>\n++++\n"));
        assert!(adoc.contains("\n\nafter\n"));
    }

    #[test]
    fn sanitizes_link_macro_labels() {
        let html = r#"<p><a href="https://example.com">a]b</a></p>"#;
        assert_eq!(html_to_adoc(html), "https://example.com[a b]\n\n");
    }

    #[test]
    fn neutralizes_inline_macros_and_attribute_references_in_link_labels() {
        let html = format!(r#"<p><a href="https://example.test/path">{USER_FRAGMENT}</a></p>"#);

        assert_eq!(
            html_to_adoc(&html),
            "https://example.test/path[\\image:evil.png \\xref:1.adoc x \\{secret}]\n\n"
        );
    }

    #[test]
    fn sanitizes_link_macro_targets() {
        let html = r#"<p><a href="https://example.com/?tags[]=x">tagged</a></p>"#;
        assert_eq!(
            html_to_adoc(html),
            "https://example.com/?tags%5B%5D=x[tagged]\n\n"
        );
    }

    #[test]
    fn sanitizes_link_targets_that_look_like_block_macros() {
        let adoc = html_to_adoc(r#"<p><a href="include::secret">x</a></p>"#);

        assert!(
            !adoc
                .lines()
                .any(|line| line.starts_with("include::secret["))
        );
    }

    #[test]
    fn sanitizes_link_targets_with_dotted_block_macro_names() {
        let adoc = html_to_adoc(r#"<p><a href="foo.bar::x">t</a></p>"#);

        assert!(!adoc.contains("foo.bar::x[t]"));
        assert_eq!(adoc, "foo.bar%3A%3Ax[t]\n\n");
    }

    #[test]
    fn sanitizes_link_targets_that_look_like_pass_macros() {
        for raw in ["pass:q,a[x]", "pass:quotes,attributes[x]"] {
            let html = raw
                .strip_suffix("[x]")
                .map(|target| format!(r#"<p><a href="{target}">x</a></p>"#))
                .unwrap();
            let adoc = html_to_adoc(&html);

            assert!(!adoc.contains(raw));
        }
    }

    #[test]
    fn sanitizes_link_targets_with_paired_passthrough_delimiters() {
        let adoc = html_to_adoc(r#"<p><a href="+++x+++">t</a></p>"#);

        assert!(!adoc.contains("+++x+++[t]"));
        assert!(!adoc.contains("+++x%2B%2B%2B[t]"));
        assert_eq!(adoc, "%2B%2B%2Bx%2B%2B%2B[t]\n\n");
    }

    #[test]
    fn sanitizes_newlines_in_link_targets() {
        let adoc = html_to_adoc("<p><a href=\"https://ok.example/\ninclude::secret\">x</a></p>");

        assert!(
            !adoc
                .lines()
                .skip(1)
                .any(|line| line.starts_with("include::secret["))
        );
    }

    #[test]
    fn sanitizes_image_macro_labels() {
        let html = r#"<p><img src="x.jpg" alt="a]b"></p>"#;
        assert_eq!(html_to_adoc(html), "image::x.jpg[a b]\n\n");
    }

    #[test]
    fn neutralizes_attribute_references_in_image_labels_without_escaping_normalized_non_macros() {
        let html = format!(r#"<p><img src="safe.png" alt="{USER_FRAGMENT}"></p>"#);

        assert_eq!(
            html_to_adoc(&html),
            "image::safe.png[image:evil.png xref:1.adoc x \\{secret}]\n\n"
        );
    }

    #[test]
    fn sanitizes_image_macro_targets() {
        let html = r#"<p><img src="../assets/foo[1].jpg" alt="x"></p>"#;
        assert_eq!(html_to_adoc(html), "image::../assets/foo%5B1%5D.jpg[x]\n\n");
    }

    #[test]
    fn sanitizes_image_targets_that_look_like_block_macros() {
        let adoc = html_to_adoc(r#"<p><img src="image::evil" alt="x"></p>"#);

        assert!(!adoc.contains("image::image::evil[x]"));
    }

    #[test]
    fn sanitizes_image_targets_with_dotted_block_macro_names() {
        let adoc = html_to_adoc(r#"<p><img src="foo.bar::x" alt="t"></p>"#);

        assert!(!adoc.contains("image::foo.bar::x[t]"));
        assert_eq!(adoc, "image::foo.bar%3A%3Ax[t]\n\n");
    }

    #[test]
    fn sanitizes_image_targets_that_look_like_pass_macros() {
        let adoc = html_to_adoc(r#"<p><img src="pass:q,a" alt="x"></p>"#);

        assert!(!adoc.contains("image::pass:q,a[x]"));
    }

    #[test]
    fn sanitizes_image_targets_with_paired_passthrough_delimiters() {
        let adoc = html_to_adoc(r#"<p><img src="+++x+++" alt="t"></p>"#);

        assert!(!adoc.contains("image::+++x+++[t]"));
        assert!(!adoc.contains("image::+++x%2B%2B%2B[t]"));
        assert_eq!(adoc, "image::%2B%2B%2Bx%2B%2B%2B[t]\n\n");
    }

    #[test]
    fn neutralizes_plain_text_that_looks_like_include_macro() {
        let adoc = html_to_adoc("<p>include::secret[]</p>");

        assert!(!adoc.lines().any(|line| line == "include::secret[]"));
        assert!(adoc.contains(r"\include::secret[]"));
    }

    #[test]
    fn neutralizes_inline_macros_and_attribute_references_in_plain_text() {
        assert_eq!(
            html_to_adoc("<p>image:evil.png[] xref:1.adoc[x] {secret}</p>"),
            "\\image:evil.png[] \\xref:1.adoc[x] \\{secret}\n\n"
        );
    }

    #[test]
    fn neutralizes_targetless_builtin_macros_but_leaves_unknown_forms_unchanged() {
        let input = "<p>before footnote:[note] kbd:[Ctrl+C] btn:[OK] after extension:[literal]</p>";
        let expected = concat!(
            "before \\footnote:[note] \\kbd:[Ctrl+C] \\btn:[OK] ",
            "after extension:[literal]\n\n"
        );

        assert_eq!(html_to_adoc(input), expected);
    }

    #[test]
    fn does_not_double_escape_preescaped_targetless_builtin_macros() {
        let input = r"<p>\footnote:[note] \kbd:[Ctrl+C] \btn:[OK]</p>";

        assert_eq!(
            html_to_adoc(input),
            "\\footnote:[note] \\kbd:[Ctrl+C] \\btn:[OK]\n\n"
        );
    }

    #[test]
    fn does_not_double_escape_inline_macros_or_attribute_references() {
        assert_eq!(
            html_to_adoc(r"<p>\image:evil.png[] \xref:1.adoc[x] \{secret}</p>"),
            "\\image:evil.png[] \\xref:1.adoc[x] \\{secret}\n\n"
        );
    }

    #[test]
    fn does_not_double_escape_preescaped_inline_passthrough_macros() {
        assert_eq!(html_to_adoc(r"<p>\pass:[x]</p>"), "\\pass:[x]\n\n");
    }

    #[test]
    fn does_not_double_escape_preescaped_inline_passthrough_delimiters() {
        assert_eq!(html_to_adoc(r"<p>\+++x+++</p>"), "\\+++x+++\n\n");
    }

    #[test]
    fn keeps_malformed_inline_macro_and_attribute_reference_shapes_unchanged() {
        assert_eq!(
            html_to_adoc("<p>image:evil.png {secret {bad space}</p>"),
            "image:evil.png {secret {bad space}\n\n"
        );
    }

    #[test]
    fn neutralizes_arbitrary_valid_inline_macro_names_at_token_boundaries() {
        assert_eq!(
            html_to_adoc("<p>extension.name:target[]</p>"),
            "\\extension.name:target[]\n\n"
        );
    }

    #[test]
    fn keeps_inline_macro_names_embedded_after_name_bytes_unchanged() {
        assert_eq!(
            html_to_adoc("<p>9image:evil.png[]</p>"),
            "9image:evil.png[]\n\n"
        );
    }

    #[test]
    fn neutralizes_plain_text_that_looks_like_block_image_macro() {
        let adoc = html_to_adoc("<p>image::x[]</p>");

        assert!(!adoc.lines().any(|line| line == "image::x[]"));
        assert!(adoc.contains(r"\image::x[]"));
    }

    #[test]
    fn neutralizes_plain_text_that_looks_like_document_attribute() {
        let adoc = html_to_adoc("<p>:toc:</p>");

        assert!(!adoc.lines().any(|line| line == ":toc:"));
        assert!(adoc.contains(r"\:toc:"));
    }

    #[test]
    fn neutralizes_plain_text_that_looks_like_document_title() {
        let adoc = html_to_adoc("<p>= Not a document title</p>");

        assert!(!adoc.lines().any(|line| line == "= Not a document title"));
        assert!(adoc.contains(r"\= Not a document title"));
    }

    #[test]
    fn neutralizes_plain_text_that_looks_like_passthrough_delimiter() {
        let adoc = html_to_adoc("<p>++++</p>");

        assert!(!adoc.lines().any(|line| line == "++++"));
        assert!(adoc.contains(r"\++++"));
    }

    #[test]
    fn neutralizes_plain_text_that_looks_like_listing_delimiter() {
        let adoc = html_to_adoc("<p>----</p>");

        assert!(!adoc.lines().any(|line| line == "----"));
        assert!(adoc.contains(r"\----"));
    }

    #[test]
    fn neutralizes_plain_text_that_looks_like_literal_delimiter() {
        let adoc = html_to_adoc("<p>....</p>");

        assert!(!adoc.lines().any(|line| line == "...."));
        assert!(adoc.contains(r"\...."));
    }

    #[test]
    fn neutralizes_plain_text_that_looks_like_sidebar_delimiter() {
        let adoc = html_to_adoc("<p>****</p>");

        assert!(!adoc.lines().any(|line| line == "****"));
        assert!(adoc.contains(r"\****"));
    }

    #[test]
    fn neutralizes_plain_text_that_looks_like_example_delimiter() {
        let adoc = html_to_adoc("<p>====</p>");

        assert!(!adoc.lines().any(|line| line == "===="));
        assert!(adoc.contains(r"\===="));
    }

    #[test]
    fn neutralizes_plain_text_that_looks_like_quote_delimiter() {
        let adoc = html_to_adoc("<p>____</p>");

        assert!(!adoc.lines().any(|line| line == "____"));
        assert!(adoc.contains(r"\____"));
    }

    #[test]
    fn keeps_single_br_as_hard_break() {
        assert_eq!(html_to_adoc("a<br>b"), "a +\nb"); // unchanged
    }

    #[test]
    fn collapses_double_br_into_paragraph_break() {
        assert_eq!(html_to_adoc("a<br><br>b"), "a\n\nb");
    }

    #[test]
    fn collapses_br_run_of_three_into_single_paragraph_break() {
        assert_eq!(html_to_adoc("a<br><br><br>b"), "a\n\nb");
    }

    #[test]
    fn collapses_br_run_with_whitespace_between() {
        assert_eq!(html_to_adoc("a<br> <br>b"), "a\n\nb");
    }

    #[test]
    fn collapses_double_br_inside_paragraph() {
        assert_eq!(html_to_adoc("<p>a<br><br>b</p>"), "a\n\nb\n\n");
    }

    #[test]
    fn trim_trailing_hard_breaks_keeps_real_content() {
        assert_eq!(trim_trailing_hard_breaks("не рыбой. +\n+\n"), "не рыбой.");
        assert_eq!(trim_trailing_hard_breaks("word +"), "word");
        assert_eq!(trim_trailing_hard_breaks("done."), "done.");
        assert_eq!(trim_trailing_hard_breaks("C++"), "C++");
        assert_eq!(
            trim_trailing_hard_breaks("line1 +\nline2"),
            "line1 +\nline2"
        );
        assert_eq!(trim_trailing_hard_breaks(" +"), "");
    }

    #[test]
    fn keeps_lone_hard_break_followed_by_content() {
        // Load-bearing per AGENTS.md: a lone <br> *between* content still emits
        // the hard break — only trailing breaks with nothing after are trimmed.
        assert_eq!(html_to_adoc("<p>a<br>b</p>"), "a +\nb\n\n");
    }

    #[test]
    fn drops_leading_hard_break_at_paragraph_start() {
        // A <br> at the very start of a paragraph has no content before it, so
        // emitting a hard break there would leave a stray `+` line — symmetric
        // to trailing-break trimming, a leading break is dropped.
        assert_eq!(html_to_adoc("<p><br>P.S. text</p>"), "P.S. text\n\n");
    }

    #[test]
    fn drops_leading_hard_break_after_block_at_root() {
        // Reproduces post 46512: a paragraph, then a bare root-level <br>, then
        // trailing text. The <br> leads the trailing inline run (nothing before
        // it on the line) and must not emit a lone `+`.
        assert_eq!(
            html_to_adoc("<p>done.</p><br>P.S. text"),
            "done.\n\nP.S. text\n\n"
        );
    }

    #[test]
    fn drops_leading_hard_break_inside_blockquote() {
        // The blockquote block-children path emits hard breaks too; a leading
        // <br> there must likewise not leave a stray `+` line.
        assert_eq!(
            html_to_adoc("<blockquote><br>quoted</blockquote>"),
            "[quote]\n____\nquoted\n____\n\n"
        );
    }

    #[test]
    fn trims_dangling_hard_break_at_end_of_paragraph() {
        assert_eq!(
            html_to_adoc("<p>text.<br><a name='cutid1-end'></a></p>"),
            "text.\n\n"
        );
    }

    #[test]
    fn trims_dangling_hard_break_before_stripped_chrome_in_body() {
        use crate::models::Post;
        use url::Url;

        let post = Post {
            journal: "evo_lutio".to_string(),
            id: 240331,
            url: Url::parse("https://evo-lutio.livejournal.com/240331.html").unwrap(),
            title: "T".to_string(),
            author: "evo_lutio".to_string(),
            poster: None,
            published_at: None,
            tags: vec![],
            body_html:
                "Быть рыбой. <br><a name='cutid1-end'></a><br><div class=\"lj-like lj-like--v4\"></div>"
                    .to_string(),
            comments: vec![],
        };

        let rendered = render_post(&post, false, None);

        assert!(
            rendered.contains("Быть рыбой."),
            "body missing: {rendered:?}"
        );
        assert!(
            !rendered.trim_end().ends_with('+'),
            "dangling trailing +: {rendered:?}"
        );
        assert!(
            !rendered.contains("+\n+"),
            "double hard break: {rendered:?}"
        );
    }

    #[test]
    fn neutralizes_additional_delimiters_after_generated_line_breaks() {
        let adoc = html_to_adoc("<p>ok<br>....</p>");

        assert!(!adoc.lines().any(|line| line == "...."));
        assert!(adoc.contains("\n\\...."));
    }

    #[test]
    fn neutralizes_active_lines_after_generated_line_breaks() {
        for html in [
            "<p>ok<br>include::secret[]</p>",
            "<p>ok<br>:toc:</p>",
            "<p>ok<br>++++</p>",
        ] {
            let adoc = html_to_adoc(html);

            assert!(
                !adoc
                    .lines()
                    .any(|line| { matches!(line, "include::secret[]" | ":toc:" | "++++") })
            );
        }
    }

    #[test]
    fn neutralizes_plain_text_unordered_list_marker() {
        let adoc = html_to_adoc("<p>* item</p>");

        assert!(!adoc.lines().any(|line| line == "* item"));
        assert!(adoc.contains(r"\* item"));
    }

    #[test]
    fn neutralizes_plain_text_ordered_list_marker() {
        let adoc = html_to_adoc("<p>. item</p>");

        assert!(!adoc.lines().any(|line| line == ". item"));
        assert!(adoc.contains(r"\. item"));
    }

    #[test]
    fn neutralizes_plain_text_admonition_label() {
        let adoc = html_to_adoc("<p>NOTE: text</p>");

        assert!(!adoc.lines().any(|line| line == "NOTE: text"));
        assert!(adoc.contains(r"\NOTE: text"));
    }

    #[test]
    fn neutralizes_plain_text_list_marker_after_generated_line_break() {
        let adoc = html_to_adoc("<p>ok<br>* item</p>");

        assert!(!adoc.lines().any(|line| line == "* item"));
        assert!(adoc.contains("\n\\* item"));
    }

    #[test]
    fn neutralizes_split_plain_text_list_marker_at_inline_boundary() {
        let adoc = html_to_adoc("<p>*<span> item</span></p>");

        assert!(!adoc.lines().any(|line| line == "* item"));
        assert!(adoc.contains(r"\* item"));
    }

    #[test]
    fn keeps_generated_lists_unescaped() {
        assert_eq!(html_to_adoc("<ul><li>item</li></ul>"), "* item\n\n");
        assert_eq!(html_to_adoc("<ol><li>item</li></ol>"), ". item\n\n");
    }

    #[test]
    fn neutralizes_inline_passthrough_macros_in_plain_text() {
        let adoc = html_to_adoc("<p>before pass:[x] after</p>");

        assert!(!adoc.contains(" pass:[x]"));
        assert!(adoc.contains(r"before \pass:[x] after"));
    }

    #[test]
    fn neutralizes_split_inline_passthrough_macros_in_plain_text() {
        let adoc = html_to_adoc("<p>pas<span>s:[x]</span></p>");

        assert_eq!(adoc, "\\pass:[x]\n\n");
    }

    #[test]
    fn neutralizes_inline_passthrough_macro_variants_in_plain_text() {
        for (html, raw, escaped) in [
            (
                "<p>before pass:q[x] after</p>",
                " pass:q[x]",
                r"before \pass:q[x] after",
            ),
            (
                "<p>before pass:a[x] after</p>",
                " pass:a[x]",
                r"before \pass:a[x] after",
            ),
            (
                "<p>before pass:quotes[x] after</p>",
                " pass:quotes[x]",
                r"before \pass:quotes[x] after",
            ),
            (
                "<p>before pass:q,a[x] after</p>",
                " pass:q,a[x]",
                r"before \pass:q,a[x] after",
            ),
            (
                "<p>before pass:quotes,attributes[x] after</p>",
                " pass:quotes,attributes[x]",
                r"before \pass:quotes,attributes[x] after",
            ),
            (
                "<p>before pass:post_replacements[x] after</p>",
                " pass:post_replacements[x]",
                r"before \pass:post_replacements[x] after",
            ),
        ] {
            let adoc = html_to_adoc(html);

            assert!(!adoc.contains(raw));
            assert!(adoc.contains(escaped));
        }
    }

    #[test]
    fn keeps_generated_inline_code_passthrough_unescaped() {
        let adoc = html_to_adoc("<p><code>a`+]</code></p>");

        assert_eq!(adoc, "pass:q[a`+\\]]\n\n");
    }

    #[test]
    fn keeps_pass_substrings_inside_larger_words_without_a_macro_shape() {
        let adoc = html_to_adoc("<p>before compass:q after</p>");

        assert!(adoc.contains("before compass:q after"));
    }

    #[test]
    fn neutralizes_inline_passthrough_delimiters_in_plain_text() {
        let adoc = html_to_adoc("<p>before +++x+++ after</p>");

        assert!(!adoc.contains(" +++x+++"));
        assert!(adoc.contains(r"before \+++x+++ after"));
    }

    #[test]
    fn neutralizes_split_inline_passthrough_delimiters_in_plain_text() {
        let adoc = html_to_adoc("<p>++<span>+x+++</span></p>");

        assert_eq!(adoc, "\\+++x+++\n\n");
    }

    #[test]
    fn neutralizes_plain_text_inside_list_items() {
        let adoc = html_to_adoc("<ul><li>include::secret[]</li></ul>");

        assert!(!adoc.lines().any(|line| line == "* include::secret[]"));
        assert!(adoc.contains(r"* \include::secret[]"));
    }

    #[test]
    fn neutralizes_split_plain_text_block_macros_at_line_start() {
        let adoc = html_to_adoc("<p>include::<span>secret[]</span></p>");

        assert!(!adoc.lines().any(|line| line == "include::secret[]"));
        assert!(adoc.contains(r"\include::secret[]"));
    }

    #[test]
    fn neutralizes_plain_text_block_macro_prefix_before_generated_strong_text() {
        let adoc = html_to_adoc("<p>include::<strong>secret[]</strong></p>");

        assert!(
            !adoc
                .lines()
                .any(|line| line.starts_with("include::") && !line.starts_with(r"\include::"))
        );
        assert!(adoc.contains(r"\include::*secret[]*"));
    }

    #[test]
    fn neutralizes_plain_text_block_macro_prefix_before_generated_strong_brackets() {
        let adoc = html_to_adoc("<p>include::<strong>secret</strong>[]</p>");

        assert!(
            !adoc
                .lines()
                .any(|line| line.starts_with("include::") && !line.starts_with(r"\include::"))
        );
        assert!(adoc.contains(r"\include::*secret*[]"));
    }

    #[test]
    fn neutralizes_plain_text_block_macro_prefix_after_generated_line_break() {
        let adoc = html_to_adoc("<p>ok<br>include::<strong>secret[]</strong></p>");

        assert!(
            !adoc
                .lines()
                .any(|line| line.starts_with("include::") && !line.starts_with(r"\include::"))
        );
        assert!(adoc.contains("\n\\include::*secret[]*"));
    }

    #[test]
    fn neutralizes_generated_line_break_plain_text_block_macro_prefix_before_generated_strong_brackets()
     {
        let adoc = html_to_adoc("<p>ok<br>include::<strong>secret</strong>[]</p>");

        assert!(
            !adoc
                .lines()
                .any(|line| line.starts_with("include::") && !line.starts_with(r"\include::"))
        );
        assert!(adoc.contains("\n\\include::*secret*[]"));
    }

    #[test]
    fn neutralizes_plain_text_block_macro_prefix_before_generated_link_macro() {
        let adoc = html_to_adoc(r#"<p>include::<a href="x">secret</a></p>"#);

        assert!(
            !adoc
                .lines()
                .any(|line| line.starts_with("include::") && !line.starts_with(r"\include::"))
        );
        assert!(adoc.contains(r"\include::x[secret]"));
    }

    #[test]
    fn neutralizes_plain_text_pass_macro_prefix_before_generated_link_macro() {
        for (raw, escaped) in [
            (r"pass:q[x]", r"\pass:q[x]"),
            (r"pass:q,a[x]", r"\pass:q,a[x]"),
        ] {
            let html = raw
                .strip_suffix("[x]")
                .map(|prefix| format!(r#"<p>{prefix}<a href="">x</a></p>"#))
                .unwrap();
            let adoc = html_to_adoc(&html);

            assert!(!adoc.lines().any(|line| line == raw));
            assert!(adoc.contains(escaped));
        }
    }

    #[test]
    fn keeps_generated_link_macros_without_pending_plain_text_prefix() {
        let adoc = html_to_adoc(r#"<p><a href="https://example.com">x</a></p>"#);

        assert_eq!(adoc, "https://example.com[x]\n\n");
    }

    #[test]
    fn keeps_generated_image_macros_without_pending_plain_text_prefix() {
        let adoc = html_to_adoc(r#"<p><img src="x.jpg" alt="x"></p>"#);

        assert_eq!(adoc, "image::x.jpg[x]\n\n");
    }

    #[test]
    fn neutralizes_plain_text_image_macro_prefix_before_generated_emphasis() {
        let adoc = html_to_adoc("<p>image::<em>x[]</em></p>");

        assert!(
            !adoc
                .lines()
                .any(|line| line.starts_with("image::") && !line.starts_with(r"\image::"))
        );
        assert!(adoc.contains(r"\image::_x[]_"));
    }

    #[test]
    fn neutralizes_root_plain_text_block_macro_prefix_before_generated_link_macro() {
        let adoc = html_to_adoc(r#"include::<a href="x">secret</a>"#);

        assert!(
            !adoc
                .lines()
                .any(|line| line.starts_with("include::") && !line.starts_with(r"\include::"))
        );
        assert!(adoc.contains(r"\include::x[secret]"));
    }

    #[test]
    fn neutralizes_root_plain_text_image_macro_prefix_before_generated_emphasis() {
        let adoc = html_to_adoc("image::<em>x[]</em>");

        assert!(
            !adoc
                .lines()
                .any(|line| line.starts_with("image::") && !line.starts_with(r"\image::"))
        );
        assert!(adoc.contains(r"\image::_x[]_"));
    }

    #[test]
    fn neutralizes_root_plain_text_pass_macro_prefix_before_generated_link_macro() {
        let adoc = html_to_adoc(r#"pass:q<a href="">[x]</a>"#);

        assert!(!adoc.lines().any(|line| line == "pass:q[x]"));
        assert!(adoc.contains(r"\pass:q[x]"));
    }

    #[test]
    fn keeps_root_generated_link_and_image_macros_without_pending_plain_text_prefix() {
        assert_eq!(
            html_to_adoc(r#"<a href="https://example.com">x</a>"#),
            "https://example.com[x]"
        );
        assert_eq!(
            html_to_adoc(r#"<img src="x.jpg" alt="x">"#),
            "image::x.jpg[x]"
        );
    }

    #[test]
    fn neutralizes_split_plain_text_block_macros_inside_blockquotes() {
        let adoc = html_to_adoc("<blockquote><p>include::<span>secret[]</span></p></blockquote>");

        assert!(!adoc.lines().any(|line| line == "include::secret[]"));
        assert!(adoc.contains(r"\include::secret[]"));
    }

    #[test]
    fn renders_script_as_visible_source_text_instead_of_passthrough() {
        let adoc = html_to_adoc("<script>alert(1)</script>");

        assert!(!adoc.contains("++++"));
        assert!(!adoc.contains("<script>alert(1)</script>"));
        assert!(adoc.contains("----\nalert(1)\n----"));
    }

    #[test]
    fn renders_style_as_visible_source_text_instead_of_passthrough() {
        let adoc = html_to_adoc("<style>body{color:red}</style>");

        assert!(!adoc.contains("++++"));
        assert!(!adoc.contains("<style>body{color:red}</style>"));
        assert!(adoc.contains("----\nbody{color:red}\n----"));
    }

    #[test]
    fn renders_nested_script_as_visible_source_text_instead_of_passthrough() {
        let adoc = html_to_adoc("<p>before<script>alert(1)</script>after</p>");

        assert!(!adoc.contains("++++"));
        assert!(!adoc.contains("++++\n<script>alert(1)</script>\n++++"));
        assert!(adoc.contains("before\n\n----\nalert(1)\n----\n\nafter"));
    }

    #[test]
    fn renders_nested_style_as_visible_source_text_instead_of_passthrough() {
        let adoc = html_to_adoc("<p>before<style>body{color:red}</style>after</p>");

        assert!(!adoc.contains("++++"));
        assert!(!adoc.contains("++++\n<style>body{color:red}</style>\n++++"));
        assert!(adoc.contains("before\n\n----\nbody{color:red}\n----\n\nafter"));
    }

    #[test]
    fn renders_script_inside_passthrough_container_as_safe_listing() {
        let adoc = html_to_adoc("<div><script>alert(1)</script></div>");

        assert!(!adoc.contains("++++"));
        assert!(!adoc.contains("++++\n<div><script>alert(1)</script></div>\n++++"));
        assert!(adoc.contains("----\n<div><script>alert(1)</script></div>\n----"));
    }

    #[test]
    fn renders_script_inside_table_as_safe_listing() {
        let adoc = html_to_adoc("<table><tr><td><script>alert(1)</script></td></tr></table>");

        assert!(!adoc.contains("++++"));
        assert!(
            !adoc
                .contains("++++\n<table><tr><td><script>alert(1)</script></td></tr></table>\n++++")
        );
        assert!(
            adoc.contains("----\n<table><tr><td><script>alert(1)</script></td></tr></table>\n----")
        );
    }

    #[test]
    fn deeply_nested_html_degrades_at_depth_cap() {
        // Nest past MAX_RENDER_DEPTH (256). Without the recursion cap, render_element
        // would recurse once per level (and, deep enough, overflow the stack). With
        // the cap, rendering stops at the limit and the remaining subtree is emitted
        // as raw passthrough — so a literal "<blockquote" tag appears in the output,
        // which normal blockquote rendering ("[quote]" blocks) never produces.
        let depth = 300;
        let html = format!(
            "{}text{}",
            "<blockquote>".repeat(depth),
            "</blockquote>".repeat(depth)
        );
        let out = html_to_adoc(&html);
        assert!(out.contains("text"), "content must be preserved");
        assert!(
            out.contains("<blockquote"),
            "expected raw passthrough degradation once the depth cap is hit"
        );
    }

    #[test]
    fn many_sibling_divs_render_quickly() {
        use std::time::Instant;
        let html = "<div>x</div>".repeat(2000);
        let start = Instant::now();
        let _ = html_to_adoc(&html);
        assert!(
            start.elapsed().as_secs() < 3,
            "render took too long: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn many_nested_divs_render_quickly() {
        // Deeply *nested* passthrough (as opposed to the sibling case above) used to
        // recover each element's original source by rescanning the matching close
        // and re-parsing/serializing each candidate — both O(subtree) per element,
        // so O(n^2) overall (~94s at 2000 levels). Source recovery is now a single
        // linear scan backed by fixed-size byte ranges, so this must complete
        // quickly without allocating overlapping source substrings.
        use std::time::Instant;
        let depth = 2000;
        let html = format!("{}x{}", "<div>".repeat(depth), "</div>".repeat(depth));
        let start = Instant::now();
        let _ = html_to_adoc(&html);
        assert!(
            start.elapsed().as_secs() < 3,
            "render took too long: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn deeply_nested_spans_do_not_overflow() {
        // Nested inline plain-text wrappers (`<span>`) are consumed by
        // `plain_text_inline_content`, which used to recurse uncapped and could
        // overflow the stack (uncatchable SIGABRT). Past MAX_RENDER_DEPTH it now
        // returns `None`, so the caller falls through to the already-capped
        // `render_element` path and this RETURNS with content preserved instead of
        // aborting. Depth is kept just past the 256 cap because this regression
        // exercises render recursion rather than source-index storage.
        let depth = 300;
        let html = format!("{}text{}", "<span>".repeat(depth), "</span>".repeat(depth));
        let out = html_to_adoc(&html);
        assert!(out.contains("text"), "content must be preserved: {out:?}");
    }

    #[test]
    fn deeply_nested_pre_preserves_deepest_text() {
        let depth = 300;
        let html = format!(
            "<pre>{}deep-marker{}</pre>",
            "<span>".repeat(depth),
            "</span>".repeat(depth)
        );

        assert_eq!(html_to_adoc(&html), "----\ndeep-marker\n----\n\n");
    }

    #[test]
    fn deeply_nested_code_preserves_deepest_text() {
        let depth = 300;
        let html = format!(
            "<code>{}deep-marker{}</code>",
            "<span>".repeat(depth),
            "</span>".repeat(depth)
        );

        assert_eq!(html_to_adoc(&html), "`deep-marker`");
    }

    #[test]
    fn renders_post_document_with_comments() {
        use crate::models::{Comment, Post};
        use chrono::{DateTime, Utc};
        use url::Url;

        let post = Post {
            journal: "news".to_string(),
            id: 166511,
            url: Url::parse("https://news.livejournal.com/166511.html").unwrap(),
            title: "Post Title".to_string(),
            author: "news".to_string(),
            poster: None,
            published_at: Some(
                DateTime::parse_from_rfc3339("2024-12-17T16:01:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            tags: vec!["updates".to_string(), "release-notes".to_string()],
            body_html: "<p>Hello <strong>world</strong>.</p>".to_string(),
            comments: vec![Comment {
                id: Some("t1".to_string()),
                parent_id: None,
                depth: 0,
                author: "alice".to_string(),
                subject: Some("First".to_string()),
                posted_at: Some(
                    DateTime::parse_from_rfc3339("2024-12-17T17:02:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                body_html: "<p>Comment body.</p>".to_string(),
                deleted: false,
            }],
        };

        let rendered = render_post(&post, true, None);
        assert!(rendered.contains("= Post Title"));
        assert!(rendered.contains(":lj-id: 166511"));
        assert!(rendered.contains(":lj-tags: updates, release-notes"));
        assert!(!rendered.contains("== Post"));
        assert!(rendered.contains("Hello *world*."));
        assert!(!rendered.contains("== Comments"));
        assert!(rendered.contains("=== alice, 2024-12-17T17:02:00Z - First"));
    }

    #[test]
    fn omits_comments_when_excluded() {
        let post = test_post_with_comments(vec![test_comment(0, false)]);

        let rendered = render_post(&post, false, None);

        assert!(!rendered.contains("== Comments"));
        assert!(!rendered.contains("commenter"));
    }

    #[test]
    fn reproduces_post_46512_shape_without_stray_hard_break() {
        // Regression for post 46512: the body is a paragraph, then a bare
        // root-level <br>, then a "P.S." run whose self-link is rewritten to an
        // xref. The leading <br> must not leave a stray `+` line, and the whole
        // document must end with a single newline.
        use crate::models::Post;
        use url::Url;

        let rewriter = LinkRewriter::new(
            "evo_lutio",
            HashSet::from([46512u64]),
            Url::parse("https://evo-lutio.livejournal.com/").unwrap(),
        );
        let post = Post {
            journal: "evo_lutio".to_string(),
            id: 46512,
            url: Url::parse("https://evo-lutio.livejournal.com/46512.html").unwrap(),
            title: "Еще раз о границах".to_string(),
            author: "evo_lutio".to_string(),
            poster: None,
            published_at: None,
            tags: vec![],
            body_html: concat!(
                "<p>И даже нотариально заверенное.</p>",
                "<br>P.S. ",
                r#"<a href="https://evo-lutio.livejournal.com/46512.html">такой вывод.</a>"#,
            )
            .to_string(),
            comments: vec![],
        };

        let rendered = render_post(&post, false, Some(&rewriter));

        // No stray `+` line anywhere.
        assert!(
            !rendered.lines().any(|line| line == "+"),
            "stray lone `+` line: {rendered:?}"
        );
        // The P.S. paragraph and its rewritten self-xref are intact.
        assert!(rendered.contains("P.S. xref:46512.adoc[такой вывод.]"));
        // Exactly one trailing newline.
        assert!(rendered.ends_with("вывод.]\n"));
        assert!(
            !rendered.ends_with("\n\n"),
            "trailing blank line: {rendered:?}"
        );
    }

    #[test]
    fn post_document_ends_with_single_trailing_newline() {
        let post = test_post_with_comments(vec![]);

        let rendered = render_post(&post, false, None);

        assert!(rendered.ends_with('\n'), "got: {rendered:?}");
        assert!(
            !rendered.ends_with("\n\n"),
            "trailing blank line: {rendered:?}"
        );
    }

    #[test]
    fn post_document_with_comments_ends_with_single_trailing_newline() {
        let post = test_post_with_comments(vec![test_comment(0, false), test_comment(1, false)]);

        let rendered = render_post(&post, true, None);

        assert!(rendered.ends_with('\n'), "got: {rendered:?}");
        assert!(
            !rendered.ends_with("\n\n"),
            "trailing blank line: {rendered:?}"
        );
        // Blocks stay separated by a blank line even though each now ends with a
        // single newline.
        assert!(rendered.contains("Comment body.\n\n"));
    }

    #[test]
    fn post_document_with_deleted_last_comment_ends_with_single_trailing_newline() {
        let post = test_post_with_comments(vec![test_comment(0, true)]);

        let rendered = render_post(&post, true, None);

        assert!(rendered.contains("_Deleted comment._"));
        assert!(
            !rendered.ends_with("\n\n"),
            "trailing blank line: {rendered:?}"
        );
    }

    #[test]
    fn renders_deleted_comment_body_text() {
        let post = test_post_with_comments(vec![test_comment(0, true)]);

        let rendered = render_post(&post, true, None);

        assert!(rendered.contains("_Deleted comment._"));
        assert!(!rendered.contains("Comment body."));
    }

    #[test]
    fn renders_deep_replies_as_block_titles() {
        let post = test_post_with_comments(vec![test_comment(4, false)]);

        let rendered = render_post(&post, true, None);

        assert!(rendered.contains(".commenter, 2024-12-17T17:02:00Z - Subject"));
        assert!(!rendered.contains("======= commenter"));
    }

    #[test]
    fn neutralizes_passthrough_macros_in_post_and_comment_titles() {
        let mut post = test_post_with_comments(vec![test_comment(0, false)]);
        post.title = "  pass:[post]\nTitle  ".to_string();
        post.comments[0].author = "pass:[author]".to_string();
        post.comments[0].subject = Some("Subject pass:[subject]".to_string());

        let rendered = render_post(&post, true, None);

        assert!(rendered.starts_with("= \\pass:[post] Title\n\n"));
        assert!(!rendered.contains("= pass:[post]"));
        assert!(
            rendered
                .contains("=== \\pass:[author], 2024-12-17T17:02:00Z - Subject \\pass:[subject]")
        );
        assert!(!rendered.contains("=== pass:[author]"));
        assert!(!rendered.contains("Subject pass:[subject]"));
    }

    #[test]
    fn neutralizes_inline_macros_and_attribute_references_in_post_titles() {
        let mut post = test_post_with_comments(vec![]);
        post.title = USER_FRAGMENT.to_string();

        let rendered = render_post(&post, false, None);

        assert!(rendered.starts_with("= \\image:evil.png[] \\xref:1.adoc[x] \\{secret}\n\n"));
    }

    #[test]
    fn neutralizes_inline_macros_and_attribute_references_in_post_attributes() {
        let mut post = test_post_with_comments(vec![]);
        post.author = USER_FRAGMENT.to_string();

        let rendered = render_post(&post, false, None);

        assert!(rendered.contains(":lj-author: \\image:evil.png[] \\xref:1.adoc[x] \\{secret}\n"));
    }

    #[test]
    fn neutralizes_inline_macros_and_attribute_references_in_comment_headings() {
        let mut comment = test_comment(0, false);
        comment.author = USER_FRAGMENT.to_string();
        comment.subject = Some(USER_FRAGMENT.to_string());
        let post = test_post_with_comments(vec![comment]);

        let rendered = render_post(&post, true, None);

        assert!(rendered.contains(
            "=== \\image:evil.png[] \\xref:1.adoc[x] \\{secret}, 2024-12-17T17:02:00Z - \\image:evil.png[] \\xref:1.adoc[x] \\{secret}\n"
        ));
    }

    #[test]
    fn renders_max_depth_comment_as_deep_reply_without_overflow() {
        let post = test_post_with_comments(vec![test_comment(usize::MAX, false)]);

        let rendered = std::panic::catch_unwind(|| render_post(&post, true, None))
            .expect("max-depth comment should render without panicking");

        assert!(rendered.contains(".commenter, 2024-12-17T17:02:00Z - Subject"));
        assert!(!rendered.contains("=== commenter"));
    }

    #[test]
    fn renders_poster_attribute_after_author_when_present() {
        let mut post = test_post_with_comments(vec![]);
        post.poster = Some("livejournal".to_string());

        let rendered = render_post(&post, false, None);

        assert!(
            rendered.contains(":lj-author: news\n:lj-poster: livejournal\n"),
            "got: {rendered:?}"
        );
    }

    #[test]
    fn omits_poster_attribute_when_absent() {
        let post = test_post_with_comments(vec![]); // poster is None

        let rendered = render_post(&post, false, None);

        assert!(!rendered.contains(":lj-poster:"), "got: {rendered:?}");
    }

    #[test]
    fn escapes_newlines_in_poster_attribute_value() {
        let mut post = test_post_with_comments(vec![]);
        post.poster = Some("weird\nposter".to_string());

        let rendered = render_post(&post, false, None);

        assert!(
            rendered.contains(":lj-poster: weird poster\n"),
            "got: {rendered:?}"
        );
        assert!(!rendered.contains("weird\nposter"));
    }

    fn test_post_with_comments(comments: Vec<crate::models::Comment>) -> crate::models::Post {
        use chrono::{DateTime, Utc};
        use url::Url;

        crate::models::Post {
            journal: "news".to_string(),
            id: 166511,
            url: Url::parse("https://news.livejournal.com/166511.html").unwrap(),
            title: "Post Title".to_string(),
            author: "news".to_string(),
            poster: None,
            published_at: Some(
                DateTime::parse_from_rfc3339("2024-12-17T16:01:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            tags: vec![],
            body_html: "<p>Hello.</p>".to_string(),
            comments,
        }
    }

    fn test_comment(depth: usize, deleted: bool) -> crate::models::Comment {
        use chrono::{DateTime, Utc};

        crate::models::Comment {
            id: Some("t1".to_string()),
            parent_id: None,
            depth,
            author: "commenter".to_string(),
            subject: Some("Subject".to_string()),
            posted_at: Some(
                DateTime::parse_from_rfc3339("2024-12-17T17:02:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            body_html: "<p>Comment body.</p>".to_string(),
            deleted,
        }
    }

    fn news_rewriter() -> LinkRewriter {
        LinkRewriter::new(
            "news",
            HashSet::from([166512u64]),
            Url::parse("https://news.livejournal.com/").unwrap(),
        )
    }

    #[test]
    fn rewrites_in_set_link_to_xref() {
        let r = news_rewriter();
        let adoc = html_to_adoc_with_links(
            r#"<p>See <a href="https://news.livejournal.com/166512.html">that post</a>.</p>"#,
            Some(&r),
        );
        assert!(
            adoc.contains("xref:166512.adoc[that post]"),
            "got: {adoc:?}"
        );
        assert!(!adoc.contains("livejournal.com"), "got: {adoc:?}");
    }

    #[test]
    fn leaves_external_and_out_of_set_links_unchanged() {
        let r = news_rewriter();
        let adoc = html_to_adoc_with_links(
            r#"<p><a href="https://example.com/x">ext</a> <a href="https://news.livejournal.com/999.html">gone</a></p>"#,
            Some(&r),
        );
        assert!(adoc.contains("https://example.com/x[ext]"), "got: {adoc:?}");
        assert!(
            adoc.contains("https://news.livejournal.com/999.html[gone]"),
            "got: {adoc:?}"
        );
    }

    #[test]
    fn no_rewriter_leaves_links_unchanged() {
        let adoc =
            html_to_adoc(r#"<p><a href="https://news.livejournal.com/166512.html">x</a></p>"#);
        assert!(
            adoc.contains("https://news.livejournal.com/166512.html[x]"),
            "got: {adoc:?}"
        );
    }

    #[test]
    fn resolves_safe_links_against_delivered_page_and_renders_unsafe_links_inertly() {
        let r = LinkRewriter::new(
            "news",
            HashSet::from([166512u64]),
            Url::parse("https://news.livejournal.com/archive/2026/166511.html").unwrap(),
        );
        let adoc = html_to_adoc_with_links(
            concat!(
                "<p>",
                r#"<a href="/profile">profile</a> "#,
                r#"<a href="next.html">next</a> "#,
                r#"<a href="//cdn.example.test/x">cdn</a> "#,
                r#"<a href="http://outside.example.test/y">outside</a> "#,
                r#"<a href="https://news.livejournal.com/166512.html?thread=5#c1">local</a> "#,
                r#"<a href="javascript:alert(1)">script</a> "#,
                r#"<a href="data:text/plain,boom">data</a> "#,
                r#"<a href="mailto:reader@example.test">mail</a>"#,
                "</p>",
            ),
            Some(&r),
        );

        assert!(
            adoc.contains("https://news.livejournal.com/profile[profile]"),
            "got: {adoc:?}"
        );
        assert!(
            adoc.contains("https://news.livejournal.com/archive/2026/next.html[next]"),
            "got: {adoc:?}"
        );
        assert!(
            adoc.contains("https://cdn.example.test/x[cdn]"),
            "got: {adoc:?}"
        );
        assert!(
            adoc.contains("http://outside.example.test/y[outside]"),
            "got: {adoc:?}"
        );
        assert!(adoc.contains("xref:166512.adoc[local]"), "got: {adoc:?}");
        assert!(adoc.contains("script data mail"), "got: {adoc:?}");
        for unsafe_target in ["javascript:", "data:", "mailto:"] {
            assert!(!adoc.contains(unsafe_target), "got: {adoc:?}");
        }
    }

    #[test]
    fn rewrites_comment_links_with_the_post_link_context() {
        let mut comment = test_comment(0, false);
        comment.body_html = concat!(
            r#"<p><a href="next.html">next</a> "#,
            r#"<a href="https://news.livejournal.com/166512.html">local</a> "#,
            r#"<a href="javascript:alert(1)">unsafe</a></p>"#,
        )
        .to_string();
        let mut post = test_post_with_comments(vec![comment]);
        post.url = Url::parse("https://news.livejournal.com/archive/2026/166511.html").unwrap();
        let r = LinkRewriter::new("news", HashSet::from([166512u64]), post.url.clone());

        let rendered = render_post(&post, true, Some(&r));

        assert!(
            rendered.contains("https://news.livejournal.com/archive/2026/next.html[next]"),
            "got: {rendered:?}"
        );
        assert!(
            rendered.contains("xref:166512.adoc[local] unsafe"),
            "got: {rendered:?}"
        );
        assert!(!rendered.contains("javascript:"), "got: {rendered:?}");
        assert!(rendered.ends_with("\n"), "got: {rendered:?}");
        assert!(!rendered.ends_with("\n\n"), "got: {rendered:?}");
    }

    #[test]
    fn resolves_linked_image_href_and_keeps_safe_image_when_href_is_unsafe() {
        let r = LinkRewriter::new(
            "news",
            HashSet::new(),
            Url::parse("https://news.livejournal.com/archive/2026/166511.html").unwrap(),
        );
        let adoc = html_to_adoc_with_links(
            concat!(
                "<p>",
                r#"<a href="../gallery"><img src="https://cdn.example.test/photo.jpg" alt="photo"></a> "#,
                r#"<a href="javascript:alert(1)"><img src="https://cdn.example.test/safe.jpg" alt="safe"></a> "#,
                r#"<a href="https://example.test/"><img src="data:image/png,boom" alt="unsafe image"></a>"#,
                "</p>",
            ),
            Some(&r),
        );

        assert!(
            adoc.contains(concat!(
                "image:https://cdn.example.test/photo.jpg[photo,",
                "link=https://news.livejournal.com/archive/gallery]"
            )),
            "got: {adoc:?}"
        );
        assert!(
            adoc.contains("image:https://cdn.example.test/safe.jpg[safe]"),
            "got: {adoc:?}"
        );
        assert!(!adoc.contains("link=javascript:"), "got: {adoc:?}");
        assert!(adoc.contains("safe] unsafe image"), "got: {adoc:?}");
        assert!(!adoc.contains("image:data:"), "got: {adoc:?}");
    }
}
