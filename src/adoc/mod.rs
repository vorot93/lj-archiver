use crate::{
    links::{LinkRewriter, POST_LINK_PREFIX_ATTRIBUTE},
    models::{Comment, Post},
};
use chrono::SecondsFormat;
use scraper::{ElementRef, Html, node::Node};
use url::Url;

mod classify;
mod escape;
mod inline;
mod source_html;

use classify::*;
use escape::*;
use inline::*;
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

    if link_rewriter.is_some() {
        output.push_str("ifndef::");
        output.push_str(POST_LINK_PREFIX_ATTRIBUTE);
        output.push_str("[:");
        output.push_str(POST_LINK_PREFIX_ATTRIBUTE);
        output.push_str(":]\n");
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

    neutralize_plain_adoc_fragment_at_line_start(&encode_authored_backslashes(title.trim()), false)
}

fn normalize_xref_label(value: &str) -> String {
    value
        .split(|ch: char| matches!(ch, '|' | '[' | ']' | '\n' | '\r') || ch.is_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn sanitize_authored_xref_label(value: &str) -> String {
    sanitize_macro_label(&normalize_xref_label(value))
}

pub(crate) fn sanitize_rendered_xref_label(value: &str) -> String {
    let value = strip_obsolete_rendered_macro_protection(value);
    sanitize_rendered_macro_label(&normalize_xref_label(&value))
}

fn escape_attribute_value(value: &str) -> String {
    let normalized = value.replace(['\r', '\n'], " ");
    neutralize_plain_adoc_fragment_at_line_start(&encode_authored_backslashes(&normalized), false)
}

pub fn html_to_adoc(html: &str) -> String {
    html_to_adoc_with_links(html, None)
}

pub fn html_to_adoc_with_links(html: &str, link_rewriter: Option<&LinkRewriter>) -> String {
    let scanned = match crate::html_scan::scan(html) {
        Ok(scanned) => scanned,
        Err(_) => return render_source_listing(html),
    };
    // Parse the instrumented copy so every semantic node carries the source
    // identity that owns its original bytes. `ctx.source_html` stays the
    // *original* input: every emitted byte is sliced from it, never from the
    // instrumented copy and never re-serialized from the DOM.
    let instrumented = scanned.instrumented_html(html);
    let fragment = Html::parse_fragment(&instrumented);
    let source_index = build_source_index(&fragment, &scanned);
    let mut output = String::new();
    let mut inline_output = InlineBuffer::new();
    let mut plain_text = String::new();
    let ctx = RenderCtx {
        source_html: html,
        scanned: &scanned,
        source_index: &source_index,
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
                    if skip_suppressed(element, ctx) {
                        continue;
                    }

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
                    if passthrough_element_name_is_safe(element.value().name())
                        && is_top_level_inline(element)
                    {
                        if let Some(text) = plain_text_inline_content(element) {
                            append_inline_fragment(&mut plain_text, &text);
                        } else {
                            flush_plain_text_run(&mut inline_output, &mut plain_text);
                            if should_preserve_unwrapped_inline_output(&rendered) {
                                flush_root_inline(&mut output, &mut inline_output, true);
                                output.push_str(&rendered);
                            } else {
                                append_generated_inline(&mut inline_output, &rendered);
                            }
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
    scanned: &'a crate::html_scan::ScannedHtml,
    source_index: &'a SourceIndex,
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

fn render_element(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> Spanned {
    if ctx.depth >= MAX_RENDER_DEPTH {
        return Spanned::generated(render_passthrough(element, ctx));
    }

    if is_stripped_chrome(element) {
        return Spanned::default();
    }

    // The single passthrough decision, shared with the source-coverage pass so
    // the two can never disagree about who owns an element's source bytes.
    if renders_as_source_preserving_passthrough(element) {
        return Spanned::generated(render_passthrough(element, ctx));
    }

    let name = element.value().name();

    match name {
        "p" => render_paragraph(element, ctx),
        "br" => Spanned::generated(" +\n"),
        "a" => render_link(element, ctx),
        "img" => render_image(element, ctx),
        "strong" | "b" => render_inline_wrapper(&render_inline_children(element, ctx), "*", "*"),
        "em" | "i" => render_inline_wrapper(&render_inline_children(element, ctx), "_", "_"),
        "code" => Spanned::generated(render_inline_code(element)),
        "pre" => Spanned::generated(render_listing(element)),
        "script" | "style" => Spanned::generated(render_safe_listing(element)),
        "blockquote" => render_blockquote(element, ctx),
        "ul" => render_list(element, "*", ctx),
        "ol" => render_list(element, ".", ctx),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => render_heading(element, name, ctx),
        _ => render_inline_children(element, ctx),
    }
}

fn has_unreviewed_element_descendant(element: ElementRef<'_>) -> bool {
    element.descendants().skip(1).any(|descendant| {
        ElementRef::wrap(descendant)
            .is_some_and(|descendant| !passthrough_element_name_is_safe(descendant.value().name()))
    })
}

/// A paragraph is a finished block: its bytes sit behind the blank line that
/// ends it, so nothing inside it is ever rewritten and it needs no interior
/// origins. Every block renderer answers `Spanned::generated` for that reason.
fn render_paragraph(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> Spanned {
    let content = render_inline_children(element, ctx);
    let trimmed = trim_trailing_hard_breaks(content.trim_start());

    if trimmed.is_empty() {
        Spanned::default()
    } else {
        Spanned::generated(format!("{trimmed}\n\n"))
    }
}

fn render_inline_children(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> Spanned {
    render_inline_children_with_options(element, ctx, false)
}

fn render_inline_children_with_options(
    element: ElementRef<'_>,
    ctx: RenderCtx<'_>,
    skip_direct_lists: bool,
) -> Spanned {
    let mut output = InlineBuffer::new();
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
                    if skip_suppressed(child_element, ctx) {
                        continue;
                    }

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
                    } else if !passthrough_element_name_is_safe(child_element.value().name())
                        || is_passthrough_block(child_element)
                    {
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
                        let rendered = render_element(child_element, ctx.deeper());
                        if should_preserve_unwrapped_inline_output(&rendered) {
                            append_inline_block(&mut output, &rendered);
                        } else {
                            append_generated_inline(&mut output, &rendered);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    flush_plain_text_run(&mut output, &mut plain_text);

    output.into_spanned()
}

fn render_block_children(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> Spanned {
    let mut output = InlineBuffer::new();

    for child in element.children() {
        match child.value() {
            Node::Text(text) => {
                if !text.trim().is_empty() {
                    append_plain_text_fragment(&mut output, text);
                }
            }
            Node::Element(_) => {
                if let Some(child_element) = ElementRef::wrap(child) {
                    if skip_suppressed(child_element, ctx) {
                        continue;
                    }

                    output.push_verbatim(&render_element(child_element, ctx.deeper()));
                }
            }
            _ => {}
        }
    }

    output.into_spanned()
}

fn render_link(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> Spanned {
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

            let Some(macro_target) = link_macro_target(href, LinkTargetKind::Anchor, ctx) else {
                return Spanned::authored(sanitize_rendered_macro_label(&text));
            };

            // The target is the renderer's: it went through the URL policy and
            // `sanitize_macro_target`, and an escape inside it would corrupt a
            // working link — or, for a same-journal post, a pending-link token.
            // The label is the post's, so it stays escapable.
            render_inline_wrapper_with_label(
                &text,
                &Spanned::authored(sanitize_rendered_macro_label(&text)),
                &format!("{macro_target}["),
                "]",
            )
        }
        None => text,
    }
}

fn render_image(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> Spanned {
    match element.attr("src") {
        Some(src) => {
            let alt = sanitize_macro_label(element.attr("alt").unwrap_or_default());
            match image_macro_target(src, ctx) {
                Some(target) => macro_with_label(&format!("image::{target}["), &alt, "]"),
                None if !alt.is_empty() => Spanned::authored(alt),
                None => Spanned::authored(sanitize_macro_label(src)),
            }
        }
        None => Spanned::default(),
    }
}

/// A macro whose delimiters and target are the renderer's and whose label is
/// the post's.
fn macro_with_label(prefix: &str, label: &str, suffix: &str) -> Spanned {
    let mut output = Spanned::default();
    output.push_generated(prefix);
    output.push_authored(label);
    output.push_generated(suffix);
    output
}

/// Which click target an anchor or a linked image resolves to. Anchors resolve
/// same-journal posts through `xref:`; linked images resolve them through the
/// `link=` attribute prefix.
#[derive(Clone, Copy)]
enum LinkTargetKind {
    Anchor,
    ImageLink,
}

fn image_macro_target(src: &str, ctx: RenderCtx<'_>) -> Option<String> {
    match ctx.link_rewriter {
        Some(_) if src.starts_with("../assets/") => Some(sanitize_macro_target(src)),
        Some(rewriter) => rewriter.absolute_target(src),
        None => direct_target(src),
    }
}

/// The click target of an anchor or linked image, decided beside
/// [`image_macro_target`] so both the image source and its click target answer
/// to one policy. Archive runs with a `LinkRewriter` keep their stricter
/// resolution (same-journal posts become local targets; everything else must
/// resolve to hosted HTTP(S) against the delivered page).
fn link_macro_target(href: &str, kind: LinkTargetKind, ctx: RenderCtx<'_>) -> Option<String> {
    match ctx.link_rewriter {
        Some(rewriter) => {
            let local = match kind {
                LinkTargetKind::Anchor => rewriter.xref_target(href),
                LinkTargetKind::ImageLink => rewriter.image_link_target(href),
            };
            local.or_else(|| rewriter.absolute_target(href))
        }
        None => direct_target(href),
    }
}

/// A direct (no-rewriter) macro target. Active only for a hosted absolute
/// HTTP(S) URL or a syntactically valid scheme-less reference, which stays
/// source-relative. Explicit `javascript:`, `data:`, `file:`, `mailto:`,
/// `ftp:`, malformed absolute, and hostless HTTP(S) references lose the link
/// and keep only their sanitized visible label or image.
fn direct_target(reference: &str) -> Option<String> {
    match Url::parse(reference) {
        Ok(url) => (matches!(url.scheme(), "http" | "https") && url.has_host())
            .then(|| sanitize_macro_target(reference)),
        Err(url::ParseError::RelativeUrlWithoutBase) => Some(sanitize_macro_target(reference)),
        Err(_) => None,
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
fn render_linked_image(image: ElementRef<'_>, href: &str, ctx: RenderCtx<'_>) -> Spanned {
    let src = image.value().attr("src").unwrap_or_default();
    let alt = sanitize_macro_label(image.value().attr("alt").unwrap_or_default());
    let Some(target) = image_macro_target(src, ctx) else {
        return Spanned::authored(if alt.is_empty() {
            sanitize_macro_label(src)
        } else {
            alt
        });
    };

    let Some(link) = link_macro_target(href, LinkTargetKind::ImageLink, ctx) else {
        return macro_with_label(&format!("image:{target}["), &alt, "]");
    };
    let link = link.replace(',', "%2C");

    macro_with_label(&format!("image:{target}["), &alt, &format!(",link={link}]"))
}

fn render_blockquote(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> Spanned {
    let children = render_block_children(element, ctx);
    let content = trim_trailing_hard_breaks(&trim_leading_hard_breaks(&children));
    let delimiter = safe_block_delimiter(&content, '_');

    Spanned::generated(format!("[quote]\n{delimiter}\n{content}\n{delimiter}\n\n"))
}

/// The renderer's universal fail-closed response, and the rendering of every
/// raw-text element. `specialcharacters` keeps the content inert against
/// *substitutions*, but Asciidoctor's preprocessor still reads the block's own
/// lines, so directive lines are escaped before the block is composed — see
/// [`neutralize_preprocessor_directive_lines`].
pub(super) fn render_specialcharacters_listing(content: &str) -> String {
    let content = neutralize_preprocessor_directive_lines(content);
    let delimiter = safe_block_delimiter(&content, '-');
    let mut output = format!("[subs=specialcharacters]\n{delimiter}\n{content}");
    if !content.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&delimiter);
    output.push_str("\n\n");
    output
}

fn render_listing(element: ElementRef<'_>) -> String {
    let content = raw_text(element);

    render_specialcharacters_listing(content.trim_matches('\n'))
}

fn render_safe_listing(element: ElementRef<'_>) -> String {
    let content = raw_text(element);

    render_specialcharacters_listing(content.trim_matches('\n'))
}

fn push_specialcharacters_segment(output: &mut String, text: &str) {
    output.push_str("pass:c[");
    for character in text.chars() {
        if character == ']' {
            output.push('\\');
        }
        output.push(character);
    }
    output.push(']');
}

fn encode_inline_code(content: &str) -> String {
    let mut encoded = String::from("`");
    let mut run_start = 0;
    for (index, character) in content.char_indices() {
        let fragment = match character {
            '\\' => Some("pass:[&#92;]"),
            '`' => Some("pass:[&#96;]"),
            _ => None,
        };
        let Some(fragment) = fragment else {
            continue;
        };
        if run_start < index {
            push_specialcharacters_segment(&mut encoded, &content[run_start..index]);
        }
        encoded.push_str(fragment);
        run_start = index + character.len_utf8();
    }
    if run_start < content.len() {
        push_specialcharacters_segment(&mut encoded, &content[run_start..]);
    } else if content.is_empty() {
        push_specialcharacters_segment(&mut encoded, "");
    }
    encoded.push('`');
    encoded
}

fn render_inline_code(element: ElementRef<'_>) -> String {
    let content = raw_text(element);

    encode_inline_code(content.trim_matches('\n'))
}

fn render_list(element: ElementRef<'_>, marker: &str, ctx: RenderCtx<'_>) -> Spanned {
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
            if skip_suppressed(child_element, ctx) {
                continue;
            }

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
                if skip_suppressed(list_element, ctx) {
                    continue;
                }

                match list_element.value().name() {
                    "ul" | "ol" => {
                        output.push_str(&render_element(list_element, ctx.in_list().deeper()));
                    }
                    _ => {}
                }
            }
        }
    }

    if !output.is_empty() && !output.ends_with("\n\n") {
        output.push('\n');
    }

    Spanned::generated(output)
}

fn render_heading(element: ElementRef<'_>, name: &str, ctx: RenderCtx<'_>) -> Spanned {
    let level = name
        .strip_prefix('h')
        .and_then(|level| level.parse::<usize>().ok())
        .unwrap_or(1)
        + 1;

    Spanned::generated(format!(
        "{} {}\n\n",
        "=".repeat(level),
        trim_trailing_hard_breaks(render_inline_children(element, ctx).trim_start())
    ))
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
        || !passthrough_element_name_is_safe(element.value().name())
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
    passthrough_element_name_is_safe(name)
        && (is_known_inline(name)
            || (!is_supported_inline(name) && !is_block_or_passthrough_boundary(name)))
}

fn flush_root_inline(output: &mut String, inline_output: &mut InlineBuffer, before_block: bool) {
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

/// True when this node's original bytes are already emitted by a distinct
/// source-preserving passthrough owner — html5ever moved it out of the element
/// whose source range contains it, so it is that element's DOM sibling rather
/// than its descendant. Every loop that renders child elements consults this
/// first, so covered bytes are emitted exactly once, by their owner.
fn skip_suppressed(element: ElementRef<'_>, ctx: RenderCtx<'_>) -> bool {
    ctx.source_index.is_suppressed(element.id())
}

/// The one path for appending renderer-owned inline output (strong, emphasis,
/// code, anchor, linked image). It neutralizes a pending authored macro prefix,
/// appends, and then guards the composed logical line when the generated
/// delimiters newly completed active block syntax. Every generated-inline
/// append must go through here — appending directly reopens finding 1.
fn append_generated_inline(output: &mut InlineBuffer, fragment: &Spanned) {
    if let Some(pending) =
        neutralize_pending_plain_text_before_generated_inline(output.line(), fragment)
    {
        let line_start = output.line_start();
        output.replace_from(line_start, &pending);
    }

    let line_start = output.line_start();
    let before = output.line().to_string();
    output.push(fragment);
    if generated_inline_completes_active_line(&before, output.line(), fragment) {
        output.insert_at(line_start, GENERATED_INLINE_LINE_GUARD);
    }
}

fn append_plain_text_fragment(output: &mut InlineBuffer, text: &str) {
    let mut plain_text = String::new();
    append_normalized_text_fragment(&mut plain_text, text);
    flush_plain_text_run(output, &mut plain_text);
}

fn append_normalized_text_fragment(output: &mut String, text: &str) {
    let normalized = normalize_inline_text(text);

    append_inline_fragment(output, &normalized);
}

fn flush_plain_text_run(output: &mut InlineBuffer, plain_text: &mut String) {
    if plain_text.is_empty() {
        return;
    }

    let is_line_start = output.is_empty() || output.ends_with('\n');
    let fragment = neutralize_plain_adoc_fragment_at_line_start(plain_text, is_line_start);
    plain_text.clear();

    if is_line_start {
        // The run *is* the logical line, and the neutralizer just handled it as
        // one.
        output.push_authored(&fragment);
        return;
    }

    // Mid-line the run is inert on its own, but the line it completes may not
    // be: renderer-owned output between two authored runs (`<p>[pass<i></i>]</p>`)
    // means neither run starts the line, so the line-start neutralizer never
    // inspects it. Classify the composed line instead. Appending directly here
    // reopens that hole.
    let line_start = output.line_start();
    let before = output.line().to_string();
    output.push_authored(&fragment);

    if let Some(neutralized) = neutralize_line_completed_by_authored_text(&before, output.line()) {
        output.replace_from(line_start, &neutralized);
    }
}

/// Flush a run of `<br>` elements buffered by the child-iteration loops in
/// `html_to_adoc` and `render_inline_children`. LiveJournal uses a lone `<br>`
/// for an in-paragraph line break and a run of two or more (possibly with
/// whitespace-only text nodes between them) for a paragraph gap, so a lone
/// pending break reproduces today's hard-break emission (` +\n`) exactly,
/// while a run of two or more collapses into a single paragraph break (a
/// blank line, no trailing `+`).
fn flush_pending_line_breaks(
    output: &mut InlineBuffer,
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
            output.push_generated(" +\n");
        }
    } else {
        output.trim_end_in_place();
        output.push_generated("\n\n");
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

fn append_inline_block(output: &mut InlineBuffer, block: &str) {
    let block = block.trim_end();
    if block.is_empty() {
        return;
    }

    output.trim_end_in_place();

    let mut fragment = String::new();
    if !output.is_empty() {
        fragment.push_str("\n\n");
    }
    fragment.push_str(block);
    fragment.push_str("\n\n");

    output.push_verbatim(&Spanned::generated(fragment));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::links::{LinkRewriter, resolve_pending_links};
    use scraper::{Html, Selector};
    use std::{collections::HashSet, fs, process::Command};
    use tempfile::tempdir;
    use url::Url;

    const USER_FRAGMENT: &str = "image:evil.png[] xref:1.adoc[x] {secret}";
    const SPECIAL_ATTRIBUTE_FRAGMENT: &str = concat!(
        "{set:lj-archiver-post-link-prefix:javascript:alert(1)//} ",
        "{counter:owned} {counter2:owned}",
    );

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
    fn authored_additional_block_constructs_are_escaped_without_altering_generated_blocks() {
        for marker in [
            "<<<",
            "'''",
            ".Authored title",
            "--",
            "|===",
            ",===",
            ":===",
            "!===",
        ] {
            let adoc = html_to_adoc(&format!("<p>before<br>{marker}<br>after</p>"));

            assert!(
                adoc.lines().any(|line| line == format!(r"\{marker} +")),
                "missing protected marker in {adoc:?}"
            );
            assert!(
                adoc.lines().any(|line| line == "after"),
                "missing following prose in {adoc:?}"
            );
        }

        assert_eq!(
            html_to_adoc("<table><tr><td>cell</td></tr></table>"),
            "++++\n<table><tr><td>cell</td></tr></table>\n++++\n\n"
        );
        assert_eq!(
            html_to_adoc("<pre>code</pre>"),
            "[subs=specialcharacters]\n----\ncode\n----\n\n"
        );
    }

    #[test]
    fn space_prefixed_marker_hides_protection_and_active_control_stays_protected() {
        assert_eq!(
            html_to_adoc(concat!(
                "<p>before<br> <<< <br>after</p>",
                "<p>control-before<br><br>include::secret[]<br><br>control-after</p>",
            )),
            concat!(
                "before +\n&#60;<< +\nafter\n\n",
                "control-before\n\n\\include::secret[]\n\ncontrol-after\n\n",
            )
        );
    }

    #[test]
    fn no_rewriter_rejects_explicit_non_http_image_schemes() {
        for src in [
            "data:image/svg+xml;base64,PHN2Zy8+",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "mailto:user@example.test",
        ] {
            let rendered = html_to_adoc(&format!(r#"<img src="{src}" alt="safe">"#));
            assert_eq!(rendered, "safe");
            assert!(!rendered.contains("image:"));
        }
        assert!(
            html_to_adoc(r#"<img src="relative/pic.png" alt="relative">"#)
                .contains("image::relative/pic.png[relative]")
        );
        assert!(
            html_to_adoc(r#"<img src="https://cdn.example/pic.png" alt="remote">"#)
                .contains("image::https://cdn.example/pic.png[remote]")
        );
    }

    #[test]
    fn direct_anchor_and_linked_image_reject_unsafe_href_targets() {
        for href in [
            "javascript:alert(1)",
            "data:text/html,%3Cscript%3Ealert(1)%3C/script%3E",
            "file:///etc/passwd",
            "mailto:user@example.test",
            "ftp://example.test/f",
            "http://",
            "https://",
        ] {
            assert_eq!(
                html_to_adoc(&format!(r#"<p><a href="{href}">label</a></p>"#)),
                "label\n\n",
                "anchor {href}"
            );
            assert_eq!(
                html_to_adoc(&format!(
                    concat!(
                        r#"<p><a href="{}">"#,
                        r#"<img src="https://cdn.example.test/p.png" alt="a"></a></p>"#,
                    ),
                    href
                )),
                "image:https://cdn.example.test/p.png[a]\n\n",
                "linked image {href}"
            );
        }
    }

    #[test]
    fn direct_anchor_and_linked_image_keep_safe_relative_and_hosted_targets() {
        assert_eq!(
            html_to_adoc(r#"<p><a href="/local/page.html">label</a></p>"#),
            "/local/page.html[label]\n\n"
        );
        assert_eq!(
            html_to_adoc(r#"<p><a href="deeper/page.html">label</a></p>"#),
            "deeper/page.html[label]\n\n"
        );
        assert_eq!(
            html_to_adoc(r#"<p><a href="https://example.test/p">label</a></p>"#),
            "https://example.test/p[label]\n\n"
        );
        assert_eq!(
            html_to_adoc(r#"<p><a href="http://example.test/p">label</a></p>"#),
            "http://example.test/p[label]\n\n"
        );
        assert_eq!(
            html_to_adoc(concat!(
                r#"<p><a href="/local/page.html">"#,
                r#"<img src="https://cdn.example.test/p.png" alt="a"></a></p>"#,
            )),
            "image:https://cdn.example.test/p.png[a,link=/local/page.html]\n\n"
        );
    }

    #[test]
    fn every_listing_declares_specialcharacters_only_substitutions() {
        for html in [
            "<pre>pass:[&lt;img src=x onerror=alert(1)&gt;]</pre>",
            "<script>pass:[<img src=x onerror=alert(1)>]</script>",
            "<iframe srcdoc='<img src=x onerror=alert(1)>'></iframe>",
        ] {
            let adoc = html_to_adoc(html);
            assert!(
                adoc.contains("[subs=specialcharacters]\n----"),
                "missing explicit substitutions in {adoc:?}"
            );
        }
    }

    #[test]
    fn specialcharacters_listing_preserves_source_line_endings_exactly() {
        for source in ["", "body", "body\n", "body\n\n"] {
            let rendered = render_specialcharacters_listing(source);
            let delimiter = rendered.lines().nth(1).unwrap();
            let expected = if source.ends_with('\n') {
                format!("[subs=specialcharacters]\n{delimiter}\n{source}{delimiter}\n\n")
            } else {
                format!("[subs=specialcharacters]\n{delimiter}\n{source}\n{delimiter}\n\n")
            };
            assert_eq!(rendered, expected);
        }
    }

    /// Asciidoctor's preprocessor reads column-zero directive lines *inside*
    /// `[subs=specialcharacters]` listings, so every listing path must escape
    /// them. The backslash is consumed on conversion, so the rendered text is
    /// unchanged.
    #[test]
    fn preprocessor_directive_lines_are_escaped_in_every_listing_path() {
        for directive in [
            "include::/etc/hostname[]",
            "ifdef::never-defined[]",
            "ifndef::never-defined[]",
            "ifeval::[1==1]",
            "endif::[]",
        ] {
            assert_eq!(
                html_to_adoc(&format!("<pre>a\n{directive}\nb</pre>")),
                format!("[subs=specialcharacters]\n----\na\n\\{directive}\nb\n----\n\n"),
                "<pre> did not escape {directive}"
            );
            assert_eq!(
                html_to_adoc(&format!("<script>a\n{directive}\nb</script>")),
                format!("[subs=specialcharacters]\n----\na\n\\{directive}\nb\n----\n\n"),
                "<script> did not escape {directive}"
            );
            assert_eq!(
                html_to_adoc(&format!("<style>a\n{directive}\nb</style>")),
                format!("[subs=specialcharacters]\n----\na\n\\{directive}\nb\n----\n\n"),
                "<style> did not escape {directive}"
            );

            // A fail-closed listing: the `style` attribute is never reviewed, so
            // the whole element degrades to its original source bytes.
            let source = format!("<section style=\"x\">\n{directive}\n</section>");
            assert_eq!(
                html_to_adoc(&source),
                format!(
                    "[subs=specialcharacters]\n----\n<section style=\"x\">\n\\{directive}\n</section>\n----\n\n"
                ),
                "the fail-closed listing did not escape {directive}"
            );
        }
    }

    /// The same for the reviewed-passthrough path: Asciidoctor evaluates
    /// directive lines inside `++++` too, and consumes the escaping backslash
    /// there as well.
    #[test]
    fn include_directive_lines_are_escaped_in_raw_passthrough() {
        let html = "<table><tr><td>\ninclude::/etc/hostname[]\n</td></tr></table>";

        assert_eq!(
            html_to_adoc(html),
            concat!(
                "++++\n",
                "<table><tr><td>\n\\include::/etc/hostname[]\n</td></tr></table>",
                "\n++++\n\n",
            )
        );
    }

    /// Only a real directive is escaped. A `#ifdef` in a C listing, a mention of
    /// `include::` inside a sentence, a directive-shaped line that leading
    /// whitespace has already deactivated, and one already carrying its own
    /// backslash all stay byte-identical — a second backslash would become
    /// visible in the converted output.
    #[test]
    fn listings_leave_non_directive_and_already_escaped_lines_alone() {
        for content in [
            "#ifdef _WIN32\n#include \"windows.h\"\n#endif",
            "the include::secret[] macro reads a file",
            "include::secret[] and more",
            " include::secret[]",
            "\tifdef::never[]",
            "include::secret",
            "\\include::secret[]",
            "\\endif::[]",
        ] {
            assert_eq!(
                html_to_adoc(&format!("<pre>{content}</pre>")),
                format!("[subs=specialcharacters]\n----\n{content}\n----\n\n"),
                "{content:?} was rewritten"
            );
        }
    }

    #[test]
    fn authored_block_metadata_lines_are_visibly_escaped() {
        for line in ["[subs=normal]", "[source,ruby]", "[#owned]", "[[owned]]"] {
            let adoc = html_to_adoc(&format!("{line}<iframe></iframe>"));
            assert!(adoc.contains(&format!("\\{line}")), "{adoc:?}");
            assert!(adoc.contains("[subs=specialcharacters]\n----"));
        }
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by CI"]
    fn authored_block_metadata_before_listings_converts_visible_and_inert() {
        for line in ["[subs=normal]", "[source,ruby]", "[#owned]", "[[owned]]"] {
            let source =
                format!(r#"{line}<iframe srcdoc="pass:[<img onerror=alert(1)>]"></iframe>"#);
            let adoc = html_to_adoc(&source);
            let converted = convert_with_asciidoctor(&adoc);
            let document = Html::parse_document(&converted);
            let visible = document.root_element().text().collect::<String>();
            assert!(visible.contains(line), "{line:?}: {converted}");
            assert!(visible.contains("pass:[<img onerror=alert(1)>]"));
            assert!(
                document
                    .select(&Selector::parse("img").unwrap())
                    .next()
                    .is_none(),
                "{line:?}: {converted}"
            );
        }
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
        assert!(adoc.contains("[subs=specialcharacters]\n-----\nbefore\n----\nafter\n-----"));
        assert!(!adoc.starts_with("----\nbefore\n----\nafter\n----"));
    }

    #[test]
    fn uses_safe_quote_delimiter_when_blockquote_contains_default_delimiter() {
        let adoc = html_to_adoc("<blockquote><pre>____\ninclude::secret[]</pre></blockquote>");

        assert!(adoc.starts_with(
            "[quote]\n_____\n[subs=specialcharacters]\n----\n____\n\\include::secret[]\n----"
        ));
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
    fn active_passthrough_capabilities_fail_closed_to_source_listings() {
        for source in [
            r#"<table onclick="alert(1)"><tr><td>x</td></tr></table>"#,
            r#"<iframe srcdoc="<script>alert(1)</script>"></iframe>"#,
            r#"<section><meta http-equiv="refresh" content="0;url=https://example.test"></section>"#,
            r#"<foo><p>custom</p></foo>"#,
            r#"<section><svg><a href="https://example.test">x</a></svg></section>"#,
        ] {
            assert_eq!(
                html_to_adoc(source),
                format!("[subs=specialcharacters]\n----\n{source}\n----\n\n")
            );
        }
    }

    #[test]
    fn reviewed_inert_table_passthrough_remains_raw() {
        let source = concat!(
            r#"<table class="record"><thead><tr><th scope="col">Name</th></tr></thead>"#,
            "<tbody><tr><td>value</td></tr></tbody></table>",
        );
        assert_eq!(html_to_adoc(source), format!("++++\n{source}\n++++\n\n"));
    }

    #[test]
    fn encoded_executable_url_fails_closed() {
        let source = r#"<section><a href="java&#x73;cript:alert(1)">x</a></section>"#;
        assert_eq!(
            html_to_adoc(source),
            format!("[subs=specialcharacters]\n----\n{source}\n----\n\n")
        );
    }

    #[test]
    fn original_scan_error_lists_complete_unchanged_source_once() {
        let source = r#"<div CLASS='record' TITLE='original &amp; bytes'>x</div>"#;
        crate::html_scan::force_next_scan_error();

        let adoc = html_to_adoc(source);

        assert_eq!(
            adoc,
            format!("[subs=specialcharacters]\n----\n{source}\n----\n\n")
        );
        assert_eq!(adoc.matches(source).count(), 1);
        assert!(!adoc.contains("++++"));
        assert!(!adoc.contains(r#"class="record""#));
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
    fn bogus_declaration_tags_do_not_shift_passthrough_source_spans() {
        let html = "<!foo <div><div>real</div>";
        assert_eq!(html_to_adoc(html), "++++\n<div>real</div>\n++++\n\n");
    }

    #[test]
    fn processing_instruction_tags_do_not_shift_passthrough_source_spans() {
        let html = "<?foo <div><div>real</div>";
        assert_eq!(html_to_adoc(html), "++++\n<div>real</div>\n++++\n\n");
    }

    #[test]
    fn malformed_space_after_tag_open_does_not_shift_real_div_source() {
        assert_eq!(
            html_to_adoc("< div>fake</div><div data-real='yes'>real</div>"),
            concat!(
                "< div>fake\n\n",
                "[subs=specialcharacters]\n",
                "----\n<div data-real='yes'>real</div>\n----\n\n",
            )
        );
    }

    #[test]
    fn tokenizer_valid_punctuation_in_tag_name_does_not_shift_real_div_source() {
        assert_eq!(
            html_to_adoc("<div!>fake</div!><div data-real='yes'>real</div>"),
            concat!(
                "[subs=specialcharacters]\n----\n<div!>fake</div!>\n----\n\n",
                "[subs=specialcharacters]\n----\n<div data-real='yes'>real</div>\n----\n\n",
            )
        );
    }

    #[test]
    fn quote_in_attribute_name_does_not_hide_tag_end_or_shift_real_div_source() {
        let input = "<div \">fake</div><div data-real='yes'>real</div>";
        let expected = concat!(
            "[subs=specialcharacters]\n----\n<div \">fake</div>\n----\n\n",
            "[subs=specialcharacters]\n----\n<div data-real='yes'>real</div>\n----\n\n"
        );
        let adoc = html_to_adoc(input);

        assert_eq!(adoc, expected);
        assert_eq!(adoc.matches("fake").count(), 1);
        assert_eq!(adoc.matches(">real</div>").count(), 1);
    }

    #[test]
    fn nul_in_tag_name_does_not_shift_following_real_div_source() {
        let input = "<div\0x>fake</div\0x><div data-real='yes'>real</div>";
        let expected = concat!(
            "[subs=specialcharacters]\n----\n<div\0x>fake</div\0x>\n----\n\n",
            "[subs=specialcharacters]\n----\n<div data-real='yes'>real</div>\n----\n\n",
        );
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
            concat!(
                "[subs=specialcharacters]\n",
                "----\nconst x = \"<table>fake</table>\";\n----\n\n",
                "++++\n<table><tr><td>real</td></tr></table>\n++++\n\n",
            )
        );
    }

    #[test]
    fn raw_text_end_tag_prefix_requires_appropriate_boundary() {
        let html = concat!(
            "<script>const x = \"</scripture><table>fake</table>\";</script>",
            "<table title='real'><tr><td>real</td></tr></table>"
        );
        assert_eq!(
            html_to_adoc(html),
            concat!(
                "[subs=specialcharacters]\n",
                "----\n",
                "const x = \"</scripture><table>fake</table>\";\n",
                "----\n\n",
                "++++\n",
                "<table title='real'><tr><td>real</td></tr></table>\n",
                "++++\n\n"
            )
        );
    }

    #[test]
    fn abrupt_comment_end_preserves_following_source_span() {
        let html = "<!--><div title='abrupt'>real</div>";
        assert_eq!(
            html_to_adoc(html),
            "++++\n<div title='abrupt'>real</div>\n++++\n\n"
        );
    }

    #[test]
    fn bang_comment_end_preserves_following_source_span() {
        let html = "<!-- comment --!><div title='bang'>real</div>";
        assert_eq!(
            html_to_adoc(html),
            "++++\n<div title='bang'>real</div>\n++++\n\n"
        );
    }

    /// html5ever foster-parents `<div>FOSTER</div>` out of the table to *before*
    /// it, so the DOM's first child is the fostered div while the source's first
    /// div is the cell one. Each must resolve to its own bytes; a per-name
    /// ordinal would exchange them. The rendered output can no longer show this
    /// — the table covers the fostered div and emits those bytes itself — so the
    /// mapping is asserted where it lives, and
    /// `foster_parented_content_is_emitted_exactly_once_by_its_passthrough_owner`
    /// fences the rendering.
    #[test]
    fn foster_parented_same_name_elements_keep_their_own_source_bytes() {
        let html = r#"<table><tr><td><div>CELL</div></td></tr><div>FOSTER</div></table>"#;
        let scanned = crate::html_scan::scan(html).unwrap();
        let fragment = Html::parse_fragment(&scanned.instrumented_html(html));
        let index = build_source_index(&fragment, &scanned);

        let divs: Vec<&str> = fragment
            .tree
            .root()
            .descendants()
            .filter_map(ElementRef::wrap)
            .filter(|element| element.value().name() == "div")
            .map(|element| &html[index.range(element.id()).expect("div has a source range")])
            .collect();

        assert_eq!(divs, ["<div>FOSTER</div>", "<div>CELL</div>"]);
    }

    #[test]
    fn foster_parented_content_is_emitted_exactly_once_by_its_passthrough_owner() {
        let html = r#"<table><tr><td><div>CELL</div></td></tr><div>FOSTER</div></table>"#;

        assert_eq!(
            html_to_adoc(html),
            format!("++++\n{html}\n++++\n\n"),
            "the table owns every byte in its source range"
        );
    }

    #[test]
    fn foster_parented_non_passthrough_content_is_also_emitted_once() {
        let html = r#"<table><tr><td><div>CELL</div></td></tr><p>FOSTER</p></table>"#;
        let rendered = html_to_adoc(html);

        assert_eq!(rendered, format!("++++\n{html}\n++++\n\n"));
        assert_eq!(
            rendered.matches("FOSTER").count(),
            1,
            "foster-parented paragraph must not also render as prose: {rendered}"
        );
    }

    #[test]
    fn content_outside_a_passthrough_owner_is_still_rendered_normally() {
        let html = concat!(
            r#"<p>before</p>"#,
            r#"<table><tr><td>c</td></tr></table>"#,
            r#"<p>after</p>"#,
        );

        assert_eq!(
            html_to_adoc(html),
            concat!(
                "before\n\n",
                "++++\n<table><tr><td>c</td></tr></table>\n++++\n\n",
                "after\n\n",
            )
        );
    }

    /// A foreign-content breakout is the same hazard as foster parenting: the
    /// tree constructor promotes `<font>` out of `<svg>` (and `<table>` out of
    /// `<math>`'s HTML integration point) to a *sibling* of the element whose
    /// source range still contains it. The coverage pass suppresses it for the
    /// same reason and by the same rule.
    #[test]
    fn foreign_content_breakout_is_emitted_once_by_its_owner() {
        for html in [
            r#"<svg><font color="red"><table><tr><td>c</td></tr></table></font></svg>"#,
            concat!(
                r#"<math><annotation-xml encoding="text/html">"#,
                r#"<table><tr><td>c</td></tr></table></annotation-xml></math>"#,
            ),
        ] {
            assert_eq!(
                html_to_adoc(html),
                format!("[subs=specialcharacters]\n----\n{html}\n----\n\n"),
                "for {html}"
            );
        }
    }

    /// Suppression may only credit an owner that actually emits the bytes.
    /// Stripped chrome emits nothing at all, so a table that is chrome owns
    /// nothing and the content html5ever moved out of it must still render.
    #[test]
    fn stripped_chrome_owns_nothing_so_content_moved_out_of_it_survives() {
        let html = r#"<table class="lj-like"><tr><td>c</td></tr><div>FOSTER</div></table>"#;

        assert_eq!(html_to_adoc(html), "++++\n<div>FOSTER</div>\n++++\n\n");
    }

    /// Every loop that renders child elements must skip suppressed ones, not
    /// just the root loop: `render_inline_children_with_options` (the `<span>`),
    /// `render_block_children` (the `<blockquote>`) and `render_list`'s nested
    /// list pass (the inner `<ul>`).
    #[test]
    fn foster_parented_content_is_suppressed_inside_every_child_loop() {
        for (html, expected) in [
            (
                r#"<span><table><tr><td>c</td></tr><b>F</b></table></span>"#,
                "++++\n<table><tr><td>c</td></tr><b>F</b></table>\n++++\n\n",
            ),
            (
                r#"<blockquote><table><tr><td>c</td></tr><div>F</div></table></blockquote>"#,
                concat!(
                    "[quote]\n____\n",
                    "++++\n<table><tr><td>c</td></tr><div>F</div></table>\n++++\n",
                    "____\n\n",
                ),
            ),
            (
                r#"<ul><li><table><tr><td>c</td></tr><ul><li>F</li></ul></table></li></ul>"#,
                concat!(
                    "* ++++\n",
                    "<table><tr><td>c</td></tr><ul><li>F</li></ul></table>\n",
                    "++++\n\n",
                ),
            ),
        ] {
            let rendered = html_to_adoc(html);

            assert_eq!(rendered, expected, "for {html}");
            assert_eq!(rendered.matches('F').count(), 1, "for {html}: {rendered}");
        }
    }

    /// Suppression may only credit an owner that actually emits the bytes.
    /// `render_list` renders a list item through its content and drops every
    /// other child of a list, so a table directly inside a list emits nothing
    /// and the item html5ever fostered out of it must still render.
    #[test]
    fn a_list_owns_nothing_so_an_item_fostered_out_of_it_survives() {
        assert_eq!(
            html_to_adoc(r#"<ul><li>a</li><table><tr><td>c</td></tr><li>F</li></table></ul>"#),
            "* a\n* F\n\n"
        );
    }

    /// A start tag says where a node *begins*, never what it contains, so an
    /// element the source never closed has an unknown extent and must never be
    /// suppressed. html5ever reconstructs an unclosed formatting element after
    /// the `</table>` that popped it, and the reconstruction carries the same
    /// synthetic identity — so suppressing on a start-tag range suppresses the
    /// reconstruction too, and its subtree holds everything parsed after the
    /// table. An unclosed `<font>` or `<b>` inside a table is ordinary
    /// LiveJournal markup, so this would silently swallow whole post bodies.
    #[test]
    fn an_unclosed_element_inside_a_table_is_never_suppressed() {
        for (html, survivors) in [
            (
                r#"<table><tr><td>c</td></tr><b>x</table>more"#,
                ["x", "more"],
            ),
            (
                concat!(
                    r#"<table><tr><td>c</td></tr><font color="red">x</table>"#,
                    r#"<p>para1</p><p>para2</p>"#,
                ),
                ["para1", "para2"],
            ),
            (
                r#"<table><tr><td>c</td></tr><a href="/z">link</table>after"#,
                ["link", "after"],
            ),
            (
                r#"<blockquote><table><tr><td>c</td></tr><b>x</table>more</blockquote>"#,
                ["x", "more"],
            ),
        ] {
            let rendered = html_to_adoc(html);

            for survivor in survivors {
                assert!(
                    rendered.contains(survivor),
                    "{survivor:?} was dropped from {html}: {rendered}"
                );
            }
        }
    }

    /// Known limitation: suppression is keyed on the scanner's start-tag
    /// identities, which only elements carry. Foster-parented bare *text* has no
    /// identity and no source offsets, so it still renders beside the table that
    /// also emits it. That duplicates inert prose; it never loses content, which
    /// is the direction that matters.
    #[test]
    fn foster_parented_bare_text_is_still_emitted_twice() {
        let html = r#"<table><tr><td>c</td></tr>TEXT</table>"#;

        assert_eq!(
            html_to_adoc(html),
            format!("TEXT\n\n++++\n{html}\n++++\n\n")
        );
    }

    /// The `<div>` opens inside the table and closes after it, so the two source
    /// ranges interleave and neither element can be emitted as live markup: each
    /// range holds a tag the other one closes. Both fail closed to one inert
    /// listing. The overlap is still emitted twice — which of the two blocks
    /// should own it is exactly what is undecidable — but nothing is lost, and
    /// no unbalanced markup reaches the converted document.
    #[test]
    fn crossing_source_ranges_degrade_to_inert_listings() {
        let html = r#"<table><tr><td>c</td></tr><div>cross</table>tail</div>"#;

        assert_eq!(
            html_to_adoc(html),
            concat!(
                "[subs=specialcharacters]\n----\n<div>cross</table>tail</div>\n----\n\n",
                "[subs=specialcharacters]\n----\n",
                "<table><tr><td>c</td></tr><div>cross</table>\n----\n\n",
                "tail\n\n",
            )
        );
    }

    #[test]
    fn deeply_nested_foster_parenting_stays_within_the_render_depth_cap() {
        let depth = MAX_RENDER_DEPTH + 32;
        let html = format!(
            "{}<table><tr><td>c</td></tr><div>FOSTER</div></table>{}",
            "<div>".repeat(depth),
            "</div>".repeat(depth),
        );

        // Must not overflow the stack and must emit the fostered content once.
        let rendered = html_to_adoc(&html);
        assert_eq!(rendered.matches("FOSTER").count(), 1, "{rendered}");
    }

    #[test]
    fn synthetic_source_identity_never_reaches_output() {
        for (html, expected) in [
            (
                r#"<table><tr><td>c</td></tr></table>"#,
                "++++\n<table><tr><td>c</td></tr></table>\n++++\n\n",
            ),
            (
                r#"<table><tr><td><div>CELL</div></td></tr><div>FOSTER</div></table>"#,
                concat!(
                    "++++\n<table><tr><td><div>CELL</div></td></tr>",
                    "<div>FOSTER</div></table>\n++++\n\n",
                ),
            ),
            (
                r#"<div><span>text</span><table><tr><td>x</td></tr></table></div>"#,
                concat!(
                    "++++\n<div><span>text</span>",
                    "<table><tr><td>x</td></tr></table></div>\n++++\n\n",
                ),
            ),
            (
                r#"<div/>trailing<table><tr><td>c</td></tr></table>"#,
                concat!(
                    "[subs=specialcharacters]\n----\n",
                    "<div/>trailing<table><tr><td>c</td></tr></table>\n----\n\n",
                ),
            ),
            (
                r#"<pre><table><tr><td>c</td></tr></table></pre>"#,
                "[subs=specialcharacters]\n----\nc\n----\n\n",
            ),
        ] {
            let rendered = html_to_adoc(html);

            assert_eq!(rendered, expected, "for {html}");
            assert!(
                !rendered.contains("lj-archiver-source-id"),
                "synthetic identity leaked for {html}: {rendered}"
            );
        }
    }

    /// The source may already carry the synthetic attribute's base name. The
    /// scanner then instruments under a suffixed name, and neither name may
    /// reach the output: the authored one survives byte-for-byte because output
    /// is sliced from the original input, and the suffixed one never appears at
    /// all because the instrumented copy is only ever parsed.
    #[test]
    fn existing_source_identity_attribute_is_preserved_byte_for_byte() {
        let html =
            r#"<div data-lj-archiver-source-id="7"><table><tr><td>c</td></tr></table></div>"#;
        let rendered = html_to_adoc(html);

        assert_eq!(
            rendered,
            format!("[subs=specialcharacters]\n----\n{html}\n----\n\n")
        );
        assert!(
            !rendered.contains("data-lj-archiver-source-id-x"),
            "collision-avoiding synthetic identity leaked: {rendered}"
        );
    }

    /// An authored `<html>` start tag merges its attributes onto the fragment
    /// parser's own root element, so that root can absorb a source identity it
    /// does not own. Indexing it would give every unmapped node an "enclosing"
    /// range that ends before them — here, dropping everything after
    /// `</html>`.
    #[test]
    fn an_authored_html_element_never_becomes_an_enclosing_source_range() {
        assert_eq!(
            html_to_adoc(r#"<html>a</html><div/>b"#),
            "a\n\n[subs=specialcharacters]\n----\n<div/>b\n----\n\n"
        );
    }

    /// A node the source never closed has no range of its own, so it is emitted
    /// as an inert listing bounded by what the scanner does know. That listing
    /// must start at the element's *own* start tag: an enclosing range also
    /// covers the element's preceding siblings, which the renderer has already
    /// emitted, and can cover LiveJournal chrome that `is_stripped_chrome`
    /// deliberately removed.
    #[test]
    fn unclosed_element_listing_starts_at_its_own_start_tag() {
        for (html, expected) in [
            (
                r#"<div class="lj-like something">x</div><div/>y"#,
                "[subs=specialcharacters]\n----\n<div/>y\n----\n\n",
            ),
            (
                r#"<p>intro</p><div class="cut">rest of the post"#,
                concat!(
                    "intro\n\n[subs=specialcharacters]\n----\n",
                    "<div class=\"cut\">rest of the post\n----\n\n",
                ),
            ),
            (
                r#"<p>x</p><div/>a"#,
                "x\n\n[subs=specialcharacters]\n----\n<div/>a\n----\n\n",
            ),
            // With a known enclosing range the listing still starts at the
            // element's own start tag, and ends where that range ends — which
            // trails the enclosing element's own closing tag into the inert
            // listing. Over-emitting one closing tag is the conservative side:
            // stopping earlier would need a content-end offset the scanner does
            // not record, and guessing one risks dropping content.
            (
                r#"<blockquote>q<div/>inner</blockquote>after"#,
                concat!(
                    "[quote]\n____\nq[subs=specialcharacters]\n----\n",
                    "<div/>inner</blockquote>\n----\n____\n\nafter\n\n",
                ),
            ),
        ] {
            assert_eq!(html_to_adoc(html), expected, "for {html}");
        }
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
    fn safe_delimiter_selection_scans_each_line_once() {
        let mut content = String::new();
        for length in 4..=20_000 {
            content.push_str(&"-".repeat(length));
            content.push('\n');
        }

        let (delimiter, inspected) = safe_block_delimiter_with_steps(&content, '-');
        assert_eq!(delimiter.len(), 20_001);
        assert!(inspected <= content.len() + 20_000);
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
    fn renders_unreviewed_font_html_as_exact_listing() {
        let source = r#"<font color="red">text</font>"#;

        assert_eq!(
            html_to_adoc(source),
            format!("[subs=specialcharacters]\n----\n{source}\n----\n\n")
        );
    }

    #[test]
    fn preserves_backticks_inside_inline_code() {
        let adoc = html_to_adoc("<p><code>a`b</code></p>");

        assert_eq!(adoc, "`pass:c[a]pass:[&#96;]pass:c[b]`\n\n");
    }

    #[test]
    fn inline_code_never_exposes_authored_passthrough_syntax() {
        let adoc =
            html_to_adoc(r"<code>pass:[&lt;img src=x onerror=alert(1)&gt;] ` ] slash\</code>");
        assert!(adoc.starts_with("`pass:c["));
        assert!(!adoc.contains("pass:q["));
    }

    #[test]
    fn inline_code_encodes_each_delimiter_adversary() {
        for (source, expected) in [
            ("<code>]</code>", "`pass:c[\\]]`"),
            ("<code>`</code>", "`pass:[&#96;]`"),
            (r"<code>slash\</code>", "`pass:c[slash]pass:[&#92;]`"),
            (r"<code>\\]</code>", "`pass:[&#92;]pass:[&#92;]pass:c[\\]]`"),
        ] {
            assert_eq!(html_to_adoc(source), expected, "{source:?}");
        }
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by CI"]
    fn inline_code_round_trips_delimiter_adversaries_inertly() {
        for (source, expected) in [
            ("<code>]</code>", "]"),
            ("<code>`</code>", "`"),
            (r"<code>slash\</code>", "slash\\"),
            (r"<code>\\]</code>", r"\\]"),
            (
                r"<code>pass:[&lt;img src=x onerror=alert(1)&gt;] ` ] slash\</code>",
                "pass:[<img src=x onerror=alert(1)>] ` ] slash\\",
            ),
        ] {
            let adoc = html_to_adoc(source);
            let converted = convert_with_asciidoctor(&adoc);
            let document = Html::parse_document(&converted);
            let selector = Selector::parse("code").unwrap();
            let codes = document
                .select(&selector)
                .map(|node| node.text().collect::<String>())
                .collect::<Vec<_>>();
            assert_eq!(codes, [expected], "{source:?}: {adoc:?}");
            assert!(!converted.contains("<img src=x"));
        }
    }

    #[test]
    fn code_with_custom_descendant_preserves_exact_containing_source_listing() {
        let source = "<code>before<foo onclick='x'>inside</foo>after</code>";

        assert_eq!(
            html_to_adoc(source),
            format!("[subs=specialcharacters]\n----\n{source}\n----\n\n")
        );
    }

    #[test]
    fn pre_with_custom_descendant_preserves_exact_containing_source_listing() {
        let source = "<pre>before<foo onclick='x'>inside</foo>after</pre>";

        assert_eq!(
            html_to_adoc(source),
            format!("[subs=specialcharacters]\n----\n{source}\n----\n\n")
        );
    }

    #[test]
    fn preserves_iframe_source_attribute_quotes_in_listing() {
        let html = "<iframe src='x'></iframe>";

        assert_eq!(
            html_to_adoc(html),
            "[subs=specialcharacters]\n----\n<iframe src='x'></iframe>\n----\n\n"
        );
    }

    #[test]
    fn preserves_embed_source_attribute_quotes_in_listing() {
        let html = "<embed src='x'>";

        assert_eq!(
            html_to_adoc(html),
            "[subs=specialcharacters]\n----\n<embed src='x'>\n----\n\n"
        );
    }

    #[test]
    fn preserves_repeated_embeds_as_distinct_listings() {
        let html = "<embed src='one'><embed src='two'>";
        let adoc = html_to_adoc(html);

        assert_eq!(
            adoc,
            concat!(
                "[subs=specialcharacters]\n----\n<embed src='one'>\n----\n\n",
                "[subs=specialcharacters]\n----\n<embed src='two'>\n----\n\n",
            )
        );
    }

    #[test]
    fn preserves_mixed_quote_repeated_iframes_as_distinct_listings() {
        let html = "<iframe src='x'></iframe><iframe src=\"x\"></iframe>";
        let adoc = html_to_adoc(html);

        assert_eq!(
            adoc,
            concat!(
                "[subs=specialcharacters]\n----\n<iframe src='x'></iframe>\n----\n\n",
                "[subs=specialcharacters]\n----\n<iframe src=\"x\"></iframe>\n----\n\n",
            )
        );
    }

    #[test]
    fn preserves_repeated_single_quoted_iframes_as_distinct_listings() {
        let html = "<iframe src='first'></iframe><iframe src='second'></iframe>";
        let adoc = html_to_adoc(html);

        assert_eq!(
            adoc,
            concat!(
                "[subs=specialcharacters]\n----\n<iframe src='first'></iframe>\n----\n\n",
                "[subs=specialcharacters]\n----\n<iframe src='second'></iframe>\n----\n\n",
            )
        );
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
    fn renders_top_level_plain_unknown_inline_html_as_listing() {
        let source = "<foo>x</foo>";

        assert_eq!(
            html_to_adoc(source),
            format!("[subs=specialcharacters]\n----\n{source}\n----\n\n")
        );
    }

    #[test]
    fn renders_top_level_custom_embed_like_unknown_inline_as_listing() {
        let source = "<lj-embed data-id='1'>x</lj-embed>";

        assert_eq!(
            html_to_adoc(source),
            format!("[subs=specialcharacters]\n----\n{source}\n----\n\n")
        );
    }

    #[test]
    fn renders_attributed_unknown_inline_html_as_listing() {
        let source = r#"<foo data-x="1">x</foo>"#;

        assert_eq!(
            html_to_adoc(source),
            format!("[subs=specialcharacters]\n----\n{source}\n----\n\n")
        );
    }

    #[test]
    fn custom_inline_elements_are_exact_visible_listings_with_boundaries() {
        for (source, expected) in [
            (
                "<foo>x</foo>",
                "[subs=specialcharacters]\n----\n<foo>x</foo>\n----\n\n",
            ),
            (
                r#"before<foo data-x="1">inside</foo>after"#,
                concat!(
                    "before\n\n",
                    "[subs=specialcharacters]\n----\n<foo data-x=\"1\">inside</foo>\n----\n\n",
                    "after\n\n",
                ),
            ),
            (
                r#"<p>before<lj-embed data-id="1">inside</lj-embed>after</p>"#,
                concat!(
                    "before\n\n",
                    "[subs=specialcharacters]\n----\n<lj-embed data-id=\"1\">inside</lj-embed>\n----\n\n",
                    "after\n\n",
                ),
            ),
            (
                r#"before<span><foo data-x="1">inside</foo></span>after"#,
                concat!(
                    "before\n\n",
                    "[subs=specialcharacters]\n----\n<foo data-x=\"1\">inside</foo>\n----\n\n",
                    "after\n\n",
                ),
            ),
        ] {
            assert_eq!(html_to_adoc(source), expected);
        }
    }

    #[test]
    fn keeps_top_level_plain_unknown_inline_listing_block_separated() {
        assert_eq!(
            html_to_adoc("a<foo>b</foo>c"),
            "a\n\n[subs=specialcharacters]\n----\n<foo>b</foo>\n----\n\nc\n\n"
        );
    }

    #[test]
    fn keeps_top_level_attributed_unknown_inline_listing_block_separated() {
        assert_eq!(
            html_to_adoc("a<foo data-x='1'>b</foo>c"),
            "a\n\n[subs=specialcharacters]\n----\n<foo data-x='1'>b</foo>\n----\n\nc\n\n"
        );
    }

    #[test]
    fn keeps_top_level_custom_unknown_inline_listing_block_separated() {
        assert_eq!(
            html_to_adoc("a<lj-embed data-id='1'>b</lj-embed>c"),
            concat!(
                "a\n\n[subs=specialcharacters]\n",
                "----\n<lj-embed data-id='1'>b</lj-embed>\n----\n\nc\n\n",
            )
        );
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
    fn active_standard_block_elements_fail_closed_to_listings() {
        for html in [
            "<menu><li>x</li></menu>",
            "<noscript><p>x</p></noscript>",
            "<hgroup><h1>x</h1></hgroup>",
            r#"<canvas data-id="1">x</canvas>"#,
        ] {
            let adoc = html_to_adoc(html);

            assert_eq!(
                adoc,
                format!("[subs=specialcharacters]\n----\n{html}\n----\n\n")
            );
        }
    }

    #[test]
    fn keeps_unknown_inline_elements_as_exact_listings() {
        for html in [
            r#"<lj-embed data-id="1">x</lj-embed>"#,
            r#"<foo data-id="1">x</foo>"#,
        ] {
            let adoc = html_to_adoc(html);

            assert_eq!(
                adoc,
                format!("[subs=specialcharacters]\n----\n{html}\n----\n\n")
            );
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
    fn preserves_iframe_listing_inside_inline_wrapper() {
        let html = r#"<p><span><iframe src="x"></iframe></span></p>"#;
        let adoc = html_to_adoc(html);

        assert_eq!(
            adoc,
            "[subs=specialcharacters]\n----\n<iframe src=\"x\"></iframe>\n----\n\n"
        );
        assert_ne!(adoc, "\n\n");
    }

    #[test]
    fn preserves_iframe_listing_inside_em_wrapper() {
        let html = r#"<p><em><iframe src="x"></iframe></em></p>"#;
        let adoc = html_to_adoc(html);

        assert!(adoc.lines().any(|line| line == "----"));
        assert!(!adoc.lines().any(|line| line == "_----" || line == "----_"));
        assert_eq!(
            adoc,
            "[subs=specialcharacters]\n----\n<iframe src=\"x\"></iframe>\n----\n\n"
        );
    }

    #[test]
    fn preserves_script_listing_inside_strong_wrapper() {
        let html = "<p><strong><script>include::x[]</script></strong></p>";
        let adoc = html_to_adoc(html);

        assert!(adoc.lines().any(|line| line == "----"));
        assert!(!adoc.lines().any(|line| line == "*----" || line == "----*"));
        assert!(adoc.contains("[subs=specialcharacters]\n----\n\\include::x[]\n----"));
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
    fn preserves_iframe_listing_inside_link_wrapper() {
        let html = r#"<a href="x"><iframe src="y"></iframe></a>"#;
        let adoc = html_to_adoc(html);

        assert_eq!(
            adoc,
            "[subs=specialcharacters]\n----\n<iframe src=\"y\"></iframe>\n----\n\n"
        );
        assert!(!adoc.contains("x[----"));
        assert!(!adoc.contains("----]"));
    }

    #[test]
    fn preserves_script_listing_inside_link_wrapper() {
        let html = r#"<a href="x"><script>include::x[]</script></a>"#;
        let adoc = html_to_adoc(html);

        assert_eq!(
            adoc,
            "[subs=specialcharacters]\n----\n\\include::x[]\n----\n\n"
        );
        assert!(!adoc.contains("x[----"));
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
    fn preserves_unknown_element_with_block_child_as_listing() {
        let html = "<foo><p>x</p></foo>";
        let adoc = html_to_adoc(html);

        assert_eq!(
            adoc,
            "[subs=specialcharacters]\n----\n<foo><p>x</p></foo>\n----\n\n"
        );
        assert_ne!(adoc, "x");
    }

    #[test]
    fn keeps_iframe_listing_on_block_boundaries() {
        let html = r#"<p>before <iframe src="x"></iframe> after</p>"#;
        let adoc = html_to_adoc(html);

        assert!(!adoc.contains("before ----\n"));
        assert!(!adoc.contains("----\n after"));
        assert!(adoc.contains("\n----\n<iframe src=\"x\"></iframe>\n----\n"));
    }

    #[test]
    fn keeps_center_listing_on_block_boundaries() {
        let html = "<p>before<center>x</center>after</p>";
        let adoc = html_to_adoc(html);

        assert!(!adoc.contains("beforexafter"));
        assert!(adoc.contains(
            "before\n\n[subs=specialcharacters]\n----\n<center>x</center>\n----\n\nafter"
        ));
    }

    #[test]
    fn keeps_object_listing_on_block_boundaries() {
        let html = r#"<p>before<object data="x"></object>after</p>"#;
        let adoc = html_to_adoc(html);

        assert!(!adoc.contains("beforeafter"));
        assert!(adoc.contains(concat!(
            "before\n\n[subs=specialcharacters]\n",
            "----\n<object data=\"x\"></object>\n----\n\nafter",
        )));
    }

    #[test]
    fn renders_custom_inline_embed_listing_on_block_boundaries() {
        let html = r#"<p>before<lj-embed data-id="1">x</lj-embed>after</p>"#;
        let adoc = html_to_adoc(html);

        assert_eq!(
            adoc,
            concat!(
                "before\n\n",
                "[subs=specialcharacters]\n----\n<lj-embed data-id=\"1\">x</lj-embed>\n----\n\n",
                "after\n\n",
            )
        );
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
    fn rejects_explicit_non_http_link_schemes_with_dotted_names() {
        let adoc = html_to_adoc(r#"<p><a href="foo.bar::x">t</a></p>"#);

        assert!(!adoc.contains("foo.bar::x[t]"));
        assert_eq!(adoc, "t\n\n");
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
    fn rejects_explicit_non_http_image_schemes_with_dotted_names() {
        let adoc = html_to_adoc(r#"<p><img src="foo.bar::x" alt="t"></p>"#);

        assert!(!adoc.contains("image::foo.bar::x[t]"));
        assert_eq!(adoc, "t\n\n");
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
    fn sanitizes_leading_colon_in_image_macro_targets() {
        // A leading `:` in the target composes a spurious colon run once
        // spliced into the macro invocation: `image:` + `:evil.jpg` reads as
        // the block-macro prefix `image::` (turning an inline linked image
        // into a block, violating the "never a block macro inside link
        // brackets" invariant), and `image::` + `:evil.jpg` reads as the
        // malformed `image:::`. Predates this task; surfaced by the Task 1
        // review (round 2).
        assert_eq!(
            html_to_adoc(
                r#"<p><a href="https://example.test/"><img src=":evil.jpg" alt="x"></a></p>"#
            ),
            "image:%3Aevil.jpg[x,link=https://example.test/]\n\n"
        );
        assert_eq!(
            html_to_adoc(r#"<p><img src=":evil.jpg" alt="x"></p>"#),
            "image::%3Aevil.jpg[x]\n\n"
        );
    }

    /// An authored `{`/`}` in a macro target would otherwise reach Asciidoctor as
    /// a live attribute reference — `{set:…}` could redefine the post-link prefix
    /// and hijack a later localized image link. The `url` crate leaves both bytes
    /// untouched in a URL fragment/query/path, so `sanitize_macro_target`
    /// percent-encodes them. (H1.)
    #[test]
    fn percent_encodes_attribute_reference_braces_in_macro_targets() {
        assert_eq!(
            sanitize_macro_target("https://x.example/#{set:lj-archiver-post-link-prefix:x}"),
            "https://x.example/#%7Bset:lj-archiver-post-link-prefix:x%7D"
        );
        assert_eq!(
            html_to_adoc(r##"<p><a href="https://x.example/#{set:a:b}">t</a></p>"##),
            "https://x.example/#%7Bset:a:b%7D[t]\n\n"
        );
        assert_eq!(
            html_to_adoc(r#"<p><img src="../assets/{counter:c}.jpg" alt="x"></p>"#),
            "image::../assets/%7Bcounter:c%7D.jpg[x]\n\n"
        );
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
        // A builtin whose name abuts a preceding name-continuation byte still
        // fires (no left word boundary), so the escape must sit before the name.
        assert_eq!(
            html_to_adoc("<p>x-image:evil.png[] 9xref:1.adoc[y] a-link:javascript:alert(1)[c]</p>"),
            "x-\\image:evil.png[] 9\\xref:1.adoc[y] a-\\link:\\javascript:alert(1)[c]\n\n"
        );
    }

    #[test]
    fn neutralizes_targetless_builtin_macros_but_leaves_unknown_forms_unchanged() {
        // The math and index families fire at the renderer's settings (no
        // `-a experimental`); `indexterm:[…]` even deletes its text. All must be
        // neutralized. `kbd`/`btn` need experimental and are inert, but are kept
        // in the vocabulary anyway. A genuinely unknown `extension:[…]` is inert
        // and stays untouched.
        let input = concat!(
            "<p>before footnote:[note] kbd:[Ctrl+C] btn:[OK] stem:[x^2] latexmath:[e^x] ",
            "asciimath:[sqrt(4)] indexterm:[secret] indexterm2:[shown] after extension:[literal]</p>"
        );
        let expected = concat!(
            "before \\footnote:[note] \\kbd:[Ctrl+C] \\btn:[OK] \\stem:[x^2] \\latexmath:[e^x] ",
            "\\asciimath:[sqrt(4)] \\indexterm:[secret] \\indexterm2:[shown] after extension:[literal]\n\n"
        );

        assert_eq!(html_to_adoc(input), expected);

        // The same builtins fire when the name abuts a preceding name byte, too.
        assert_eq!(
            html_to_adoc("<p>x-indexterm:[secret] 9stem:[x^2]</p>"),
            "x-\\indexterm:[secret] 9\\stem:[x^2]\n\n"
        );
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

    /// Asciidoctor's inline-macro regexes carry no left word boundary, so a
    /// builtin name that abuts a preceding digit, `.`, `-` or `_` still fires
    /// (`9image:evil.png[]` is a live image). The escape must therefore sit
    /// immediately before the name, not at the surrounding token boundary.
    #[test]
    fn neutralizes_inline_macro_names_embedded_after_name_bytes() {
        assert_eq!(
            html_to_adoc("<p>9image:evil.png[]</p>"),
            "9\\image:evil.png[]\n\n"
        );
        assert_eq!(
            html_to_adoc("<p>word.xref:1.adoc[t]</p>"),
            "word.\\xref:1.adoc[t]\n\n"
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
    fn neutralizes_missing_plain_text_line_start_constructs() {
        for (html, active, escaped) in [
            (
                "<p>// visible comment</p>",
                "// visible comment",
                r"\// visible comment",
            ),
            (
                "<p>- visible item</p>",
                "- visible item",
                r"\- visible item",
            ),
            (
                "<p>term:: visible definition</p>",
                "term:: visible definition",
                "term:pass:[:] visible definition",
            ),
            ("<p>term::</p>", "term::", "term:pass:[:]"),
        ] {
            let adoc = html_to_adoc(html);
            assert!(!adoc.lines().any(|line| line == active), "{adoc:?}");
            assert!(adoc.contains(escaped), "{adoc:?}");
        }
    }

    #[test]
    fn neutralizes_missing_constructs_after_breaks_and_inline_boundaries() {
        let adoc =
            html_to_adoc("<p>ok<br>-<span> item</span><br>term<span>:: definition</span></p>");

        assert!(adoc.contains("\n\\- item"), "{adoc:?}");
        assert!(adoc.contains("\nterm:pass:[:] definition"), "{adoc:?}");
    }

    #[test]
    fn composes_line_start_neutralization_with_description_delimiter_encoding() {
        for (html, expected) in [
            (
                "<p>// term:: definition</p>",
                "\\// term:pass:[:] definition\n\n",
            ),
            (
                "<p>- term:: definition</p>",
                "\\- term:pass:[:] definition\n\n",
            ),
            (
                "<p>include::[x] term:: definition</p>",
                "\\include::[x] term:pass:[:] definition\n\n",
            ),
            (
                r"<p>\term:: definition</p>",
                "\\term:pass:[:] definition\n\n",
            ),
            (
                "<p>term:: first :: definition</p>",
                "term:pass:[:] first :pass:[:] definition\n\n",
            ),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html:?}");
        }

        assert_eq!(
            neutralize_plain_adoc_fragment_at_line_start("ordinary term:: definition", false),
            "ordinary term:: definition"
        );
    }

    #[test]
    fn keeps_pending_plain_text_description_delimiters_encoded_before_hard_breaks() {
        assert_eq!(
            html_to_adoc("<p>include::<br>later</p>"),
            "include:pass:[:] +\nlater\n\n"
        );
    }

    #[test]
    fn description_delimiter_neutralization_is_idempotent() {
        for input in [
            "// term:: definition",
            "- term:: definition",
            r"\term:: definition",
            "term:: first :: definition",
        ] {
            let escaped = neutralize_active_adoc_lines(input);
            assert_eq!(neutralize_active_adoc_lines(&escaped), escaped, "{input:?}");
        }
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
    fn keeps_generated_inline_code_encoding_unescaped() {
        let adoc = html_to_adoc("<p><code>a`+]</code></p>");

        assert_eq!(adoc, "`pass:c[a]pass:[&#96;]pass:c[+\\]]`\n\n");
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
        let adoc = html_to_adoc(r#"pass:q<a href="">x</a>"#);

        assert!(!adoc.lines().any(|line| line == "pass:q[x]"));
        assert!(adoc.contains(r"\pass:q[x]"), "{adoc:?}");
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
    fn generated_inline_wrappers_do_not_complete_active_block_lines() {
        for (html, expected) in [
            ("<p>.<strong>Title</strong></p>", "pass:[].*Title*\n\n"),
            ("<p>.<em>Title</em></p>", "pass:[]._Title_\n\n"),
            ("<p>.<code>x</code></p>", "pass:[].`pass:c[x]`\n\n"),
            (
                r#"<p>.<a href="https://example.test/">x</a></p>"#,
                "pass:[].https://example.test/[x]\n\n",
            ),
            (
                concat!(
                    r#"<p>.<a href="https://example.test/">"#,
                    r#"<img src="https://cdn.example.test/p.png" alt="a"></a></p>"#,
                ),
                "pass:[].image:https://cdn.example.test/p.png[a,link=https://example.test/]\n\n",
            ),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html}");
        }
    }

    #[test]
    fn safe_prose_controls_do_not_gain_a_generated_inline_guard() {
        for (html, expected) in [
            ("<p>Hello <strong>world</strong></p>", "Hello *world*\n\n"),
            ("<p><strong>Lead</strong> text</p>", "*Lead* text\n\n"),
            // `". "` is unconditionally an ordered-list marker, so the authored run
            // is already escaped by the plain-text neutralizer before the append.
            ("<p>. <strong>spaced</strong></p>", "\\. *spaced*\n\n"),
            ("<p>C++ <em>and</em> more</p>", "C++ _and_ more\n\n"),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html}");
        }
    }

    #[test]
    fn guards_a_dot_leading_generated_target_but_not_a_lone_block_image() {
        // A renderer-owned target that starts the line with `.` completes
        // block-title syntax. Unguarded, Asciidoctor turns the line into a title
        // and absorbs the *following* block into it.
        for (html, expected) in [
            (
                r#"<p><a href="../other/page.html">x</a></p>"#,
                "pass:[]../other/page.html[x]\n\n",
            ),
            (
                r#"<p><a href="./page.html">x</a></p>"#,
                "pass:[]./page.html[x]\n\n",
            ),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html}");
        }

        // The renderer's own block image macro alone on its line is deliberate
        // output; guarding it would render the image as literal text.
        assert_eq!(
            html_to_adoc(r#"<p><img src="x.jpg" alt="x"></p>"#),
            "image::x.jpg[x]\n\n"
        );
        assert_eq!(
            html_to_adoc(r#"<img src="x.jpg" alt="x">"#),
            "image::x.jpg[x]"
        );
    }

    #[test]
    fn authored_text_after_a_generated_inline_fragment_cannot_complete_active_lines() {
        // A generated inline fragment splits an authored run in two, and neither
        // half starts a line, so the line-start neutralizer never sees the line
        // these halves compose. Every case below is inert as authored text alone.
        for (html, expected) in [
            // `[pass]` is block metadata that sets the *next* block's style to a
            // raw passthrough — the paragraph after it is emitted unescaped.
            ("<p>[pass<i></i>]</p>", "pass:[][pass]\n\n"),
            (
                r#"<p>[subs="macros"<i></i>]</p>"#,
                "pass:[][subs=\"macros\"]\n\n",
            ),
            ("<p>[pass<b>x</b>]</p>", "pass:[][pass*x*]\n\n"),
            (
                "<p>[pass<code>x</code>]</p>",
                "pass:[][pass`pass:c[x]`]\n\n",
            ),
            // Heading and table-of-contents injection.
            (
                "<p>=<i></i> Injected Heading</p>",
                "pass:[]= Injected Heading\n\n",
            ),
            // The rest of the active-line vocabulary, all anchored at column zero.
            ("<p>-<i></i> item</p>", "pass:[]- item\n\n"),
            ("<p>*<i></i> item</p>", "pass:[]* item\n\n"),
            ("<p>NOTE<i></i>: hi</p>", "pass:[]NOTE: hi\n\n"),
            ("<p>:<i></i>attr: v</p>", "pass:[]:attr: v\n\n"),
            // A block macro line, which for `include::` is also a live
            // preprocessor directive that reads a local file at conversion time.
            (
                "<p>include<i></i>::/etc/hostname[]</p>",
                "pass:[]include::/etc/hostname[]\n\n",
            ),
            // A description-list marker is positional rather than anchored at the
            // line start, so the guard cannot deactivate it; the marker itself is
            // neutralized instead, byte-for-byte as the line-start pass does.
            ("<p>term<i></i>:: def</p>", "term:pass:[:] def\n\n"),
            // The same marker split *across* the generated fragment.
            ("<p>term:<i></i>: def</p>", "term:pass:[:] def\n\n"),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html}");
        }
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn a_neutralized_split_line_converts_to_the_authored_text() {
        // Neutralizing must stay invisible: the reader sees the bytes the post
        // author wrote, with no guard and no stray escape.
        for (html, visible) in [
            ("<p>[pass<i></i>]</p>", "[pass]"),
            (r#"<p>[subs="macros"<i></i>]</p>"#, r#"[subs="macros"]"#),
            ("<p>=<i></i> Injected Heading</p>", "= Injected Heading"),
            ("<p>NOTE<i></i>: hi</p>", "NOTE: hi"),
            ("<p>term<i></i>:: def</p>", "term:: def"),
            ("<p>term:<i></i>: def</p>", "term:: def"),
        ] {
            let converted = convert_with_asciidoctor(&html_to_adoc(html));
            let text = Html::parse_document(&converted)
                .root_element()
                .text()
                .collect::<String>();

            assert_eq!(text.trim(), visible, "{html} converted to {converted}");
        }
    }

    #[test]
    fn mid_sentence_prose_does_not_gain_a_line_guard() {
        for (html, expected) in [
            (
                "<p>Hello <b>there</b> [not a block] ok.</p>",
                "Hello *there* [not a block] ok.\n\n",
            ),
            ("<p>a = b <b>c</b> = d</p>", "a = b *c* = d\n\n"),
            ("<p>C++ <em>and</em> more</p>", "C++ _and_ more\n\n"),
            ("<p>x<i></i>y</p>", "xy\n\n"),
            ("<p>x<i></i>. y</p>", "x. y\n\n"),
            (
                "<p>1<code>st</code> place [ok]</p>",
                "1`pass:c[st]` place [ok]\n\n",
            ),
            // An already-escaped line keeps its single escape: the composed line
            // is inert before *and* after the append, so no guard is added.
            ("<p>//<i></i> comment</p>", "\\// comment\n\n"),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html}");
        }
    }

    #[test]
    fn plain_text_inline_wrappers_keep_their_single_authored_run() {
        // `<span>` contributes its text to the *same* authored run rather than
        // splitting it, so the run is still neutralized as one line start. This
        // path must stay untouched by the mid-line guard.
        for (html, expected) in [
            ("<p>[pass<span></span>]</p>", "\\[pass]\n\n"),
            ("<p>[pass<span>x</span>]</p>", "\\[passx]\n\n"),
            (
                "<p>=<span></span> Injected Heading</p>",
                "\\= Injected Heading\n\n",
            ),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html}");
        }
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn split_authored_block_metadata_cannot_activate_a_following_block() {
        // `[pass]` on its own line turns the *next* block into a raw passthrough,
        // so an authored `<script>` in the following paragraph reaches the reader
        // executable. `[subs="macros"]` turns on macro substitution inside the
        // reviewed raw-HTML block that follows, so authored AsciiDoc macro text
        // inside an archived table becomes a live link the URL policy never saw.
        for html in [
            concat!(
                "<p>[pass<i></i>]</p>",
                "<p>&lt;script&gt;alert(document.domain)&lt;/script&gt;</p>",
            ),
            concat!(
                r#"<p>[subs="macros"<i></i>]</p>"#,
                "<table><tr><td>link:javascript:alert(1)[CLICK]</td></tr></table>",
            ),
            concat!(
                "<p>=<i></i> Injected Heading</p>",
                "<p>ordinary post text</p>",
            ),
        ] {
            let converted = convert_with_asciidoctor(&html_to_adoc(html));
            let document = Html::parse_document(&converted);

            assert!(
                !converted.contains("<script"),
                "raw script reached the converted output for {html}: {converted}"
            );
            assert!(
                document
                    .select(&Selector::parse("a").unwrap())
                    .filter_map(|anchor| anchor.attr("href"))
                    .all(|href| !href.to_ascii_lowercase().starts_with("javascript:")),
                "a javascript: target reached the converted output for {html}: {converted}"
            );
            assert!(
                document
                    .select(&Selector::parse("h1, h2, h3, h4, h5, h6").unwrap())
                    .next()
                    .is_none(),
                "an authored heading was injected for {html}: {converted}"
            );
        }
    }

    #[test]
    fn a_closed_passthrough_macro_is_not_a_dangling_prefix() {
        // Only a `pass:` prefix that runs to the *end* of the line is completed
        // by the `[` the next generated fragment supplies. A macro that already
        // closed earlier on the line is not, and escaping the line for it puts a
        // visible backslash in front of ordinary prose.
        for (html, expected) in [
            (
                "<p>a <code>x</code> <em>y</em></p>",
                "a `pass:c[x]` _y_\n\n",
            ),
            ("<p>a<code>x</code><em>y</em></p>", "a`pass:c[x]`_y_\n\n"),
            (
                "<p>.<strong>Title</strong><em>x</em></p>",
                "pass:[].*Title*_x_\n\n",
            ),
            ("<p>[pass<i></i>]<i></i></p>", "pass:[][pass]\n\n"),
            (
                "<p>foo<i></i>:: bar <em>y</em></p>",
                "foo:pass:[:] bar _y_\n\n",
            ),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html}");
        }

        // A genuinely dangling authored prefix still earns its escape.
        for (html, expected) in [
            (r#"<p>pass:q<a href="">x</a></p>"#, "\\pass:q[x]\n\n"),
            (
                "<p>include::<strong>secret[]</strong></p>",
                "\\include::*secret[]*\n\n",
            ),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html}");
        }
    }

    const SPLIT_SCRIPT: &str = "&lt;script&gt;alert(document.domain)&lt;/script&gt;";
    const SPLIT_SCRIPT_TEXT: &str = "<script>alert(document.domain)</script>";

    /// The five verified reproductions of the split-run inline injection class,
    /// each with the single-run control that was always inert. A generated
    /// inline fragment — very often the *empty* one `<i></i>` renders to —
    /// split the authored run, so neither half held a whole construct and no
    /// per-run scanner fired on either.
    #[test]
    fn split_authored_inline_constructs_are_escaped_like_their_single_run_controls() {
        for (split, control, split_expected, control_expected) in [
            // R1 — raw executable `<script>` through a split inline passthrough.
            (
                format!("<p>+++<i></i>{SPLIT_SCRIPT}+++</p>"),
                format!("<p>+++{SPLIT_SCRIPT}+++</p>"),
                format!("\\+++{SPLIT_SCRIPT_TEXT}+++\n\n"),
                format!("\\+++{SPLIT_SCRIPT_TEXT}+++\n\n"),
            ),
            // R2 — `javascript:` anchor through a split inline macro.
            (
                "<p>link:javascript<i></i>:alert(1)[click]</p>".to_string(),
                "<p>link:javascript:alert(1)[click]</p>".to_string(),
                "\\link:\\javascript:alert(1)[click]\n\n".to_string(),
                "\\link:\\javascript:alert(1)[click]\n\n".to_string(),
            ),
            // R3 — a remote fetch on every read, which `--no-media` never sees.
            (
                "<p>image:https://evil.example/track.png<i></i>[x]</p>".to_string(),
                "<p>image:https://evil.example/track.png[x]</p>".to_string(),
                "\\image:\\https://evil.example/track.png[x]\n\n".to_string(),
                "\\image:\\https://evil.example/track.png[x]\n\n".to_string(),
            ),
            // R4 — hijacking the archive's own post-link prefix attribute.
            (
                "<p>{set:lj-archiver-post-link-prefix<i></i>:javascript:alert(1)//}</p>"
                    .to_string(),
                "<p>{set:lj-archiver-post-link-prefix:javascript:alert(1)//}</p>".to_string(),
                "\\{set:lj-archiver-post-link-prefix:javascript:alert(1)//}\n\n".to_string(),
                "\\{set:lj-archiver-post-link-prefix:javascript:alert(1)//}\n\n".to_string(),
            ),
            // R5 — the escape lands immediately before the dangling prefix, not
            // at the logical line start, which left both a live macro and a
            // visible stray backslash.
            (
                format!(r#"<p>hi pass:<a href="">{SPLIT_SCRIPT}</a></p>"#),
                format!(r#"<p>pass:<a href="">{SPLIT_SCRIPT}</a></p>"#),
                format!("hi \\pass:[{SPLIT_SCRIPT_TEXT}]\n\n"),
                format!("\\pass:[{SPLIT_SCRIPT_TEXT}]\n\n"),
            ),
        ] {
            assert_eq!(html_to_adoc(&split), split_expected, "split {split}");
            assert_eq!(
                html_to_adoc(&control),
                control_expected,
                "control {control}"
            );
        }
    }

    /// R5's other half. The same wrong insertion point served the block-macro
    /// prefix family, and its correct mid-line answer is *not* the same one.
    ///
    /// An inline macro's escape belongs immediately before its name, which at a
    /// line start happens to be the line start — that coincidence is why the old
    /// insertion point looked right for as long as only line starts were tested.
    /// A *block* macro has no mid-line reading at all (`BlockMacroRx` is
    /// anchored at `^`, and every builtin inline macro excludes a leading `:`
    /// from its target), so the answer there is that nothing needs deactivating:
    /// the line-start case stays with the block-line guard that owns it, and the
    /// second colon the description-list neutralizer already replaced stays
    /// replaced.
    #[test]
    fn a_dangling_macro_prefix_is_escaped_before_the_name_not_the_line() {
        for (html, expected) in [
            // A macro name that starts the line: both answers coincide.
            (r#"<p>pass:q<a href="">x</a></p>"#, "\\pass:q[x]\n\n"),
            (
                r#"<p>image:t.png<a href="">x</a></p>"#,
                "\\image:t.png[x]\n\n",
            ),
            // Mid-line the escape moves to the name, not to column zero.
            (r#"<p>hi pass:q<a href="">x</a></p>"#, "hi \\pass:q[x]\n\n"),
            (
                r#"<p>hi image:t.png<a href="">x</a></p>"#,
                "hi \\image:t.png[x]\n\n",
            ),
            // A block-macro prefix keeps the line-start guard that owns it.
            (
                r#"<p>include::<a href="x">y</a></p>"#,
                "\\include::x[y]\n\n",
            ),
            (
                r#"<p>hi include::<a href="x">y</a></p>"#,
                "hi include:pass:[:]x[y]\n\n",
            ),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html}");
        }
    }

    /// Coverage by vocabulary rather than by example: every scanner in
    /// `neutralize_inline_adoc_syntax`, in all three split directions.
    #[test]
    fn every_inline_scanner_is_covered_in_every_split_direction() {
        for (scanner, direction, html, expected) in [
            // Attribute references.
            (
                "attribute reference",
                "authored then generated",
                "<p>{set:a:javascript:alert(1)//<b>}</b></p>",
                "\\{set:a:javascript:alert(1)//*}*\n\n",
            ),
            (
                "attribute reference",
                "generated then authored",
                "<p><b>{set:a:javascript:alert(1)//</b>}</p>",
                "*\\{set:a:javascript:alert(1)//*}\n\n",
            ),
            (
                "attribute reference",
                "authored, generated, authored",
                "<p>{set:a:javascript:alert(1)//<i></i>}</p>",
                "\\{set:a:javascript:alert(1)//}\n\n",
            ),
            // Inline passthrough macros.
            (
                "passthrough macro",
                "authored then generated",
                r#"<p>hi pass:q<a href="">x</a></p>"#,
                "hi \\pass:q[x]\n\n",
            ),
            (
                "passthrough macro",
                "generated then authored",
                "<p><b>pass:</b>[&lt;script&gt;alert(1)&lt;/script&gt;]</p>",
                "*\\pass:*[<script>alert(1)</script>]\n\n",
            ),
            (
                "passthrough macro",
                "authored, generated, authored",
                "<p>pass<i></i>:[&lt;script&gt;alert(1)&lt;/script&gt;]</p>",
                "\\pass:[<script>alert(1)</script>]\n\n",
            ),
            // Inline macros.
            (
                "inline macro",
                "authored then generated",
                r#"<p>image:https://evil.example/t.png<a href="">x</a></p>"#,
                "\\image:\\https://evil.example/t.png[x]\n\n",
            ),
            (
                "inline macro",
                "generated then authored",
                "<p><b>image:https://evil.example/t.png</b>[x]</p>",
                "*\\image:\\https://evil.example/t.png*[x]\n\n",
            ),
            (
                "inline macro",
                "authored, generated, authored",
                "<p>image:https://evil.example/t.png<i></i>[x]</p>",
                "\\image:\\https://evil.example/t.png[x]\n\n",
            ),
            // Inline passthrough delimiters.
            (
                "passthrough delimiters",
                "authored then generated",
                "<p>+++&lt;script&gt;alert(1)&lt;/script&gt;<b>+++</b></p>",
                "\\+++<script>alert(1)</script>*+++*\n\n",
            ),
            (
                "passthrough delimiters",
                "generated then authored",
                "<p><b>+++</b>&lt;script&gt;alert(1)&lt;/script&gt;+++</p>",
                "*\\+++*<script>alert(1)</script>+++\n\n",
            ),
            (
                "passthrough delimiters",
                "authored, generated, authored",
                "<p>+++<i></i>&lt;script&gt;alert(1)&lt;/script&gt;+++</p>",
                "\\+++<script>alert(1)</script>+++\n\n",
            ),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{scanner}, {direction}");
        }
    }

    /// The macro half of the class is closed for the whole `name:target[...]`
    /// vocabulary, not for the handful of names the reproductions happened to
    /// use: the scanner keys on the shape, so every name behaves alike.
    #[test]
    fn the_whole_inline_macro_vocabulary_is_closed_against_splitting() {
        for name in [
            "link",
            "mailto",
            "image",
            "icon",
            "xref",
            "kbd",
            "btn",
            "menu",
            "footnote",
            "footnoteref",
            "stem",
            "indexterm",
            "pass",
            "unregistered-extension",
        ] {
            let split = html_to_adoc(&format!("<p>{name}:evil.example/t<i></i>[x]</p>"));
            let control = html_to_adoc(&format!("<p>{name}:evil.example/t[x]</p>"));

            assert_eq!(split, control, "{name}");
            assert!(split.starts_with('\\'), "{name} was left live in {split:?}");
        }
    }

    /// A generated wrapper's delimiters are the renderer's, but its *interior*
    /// is authored, so authored text inside one composes with authored text
    /// outside it across the wrapper's own boundary. The fragment rule covers
    /// this with no extra case — the escape lands at the construct's name, which
    /// is inside the generated fragment, because that is where it has to go.
    #[test]
    fn authored_text_inside_a_generated_wrapper_cannot_compose_with_text_outside() {
        for (html, expected) in [
            // A raw `<script>` through a split passthrough pair.
            (
                "<p><b>+++</b>&lt;script&gt;alert(1)&lt;/script&gt;+++</p>",
                "*\\+++*<script>alert(1)</script>+++\n\n",
            ),
            // The archive's own post-link prefix, hijacked from inside an <em>.
            (
                "<p><em>{set:lj-archiver-post-link-prefix:javascript:alert(1)//</em>}</p>",
                "_\\{set:lj-archiver-post-link-prefix:javascript:alert(1)//_}\n\n",
            ),
            // ... and from outside, closed by authored text after the wrapper.
            (
                "<p>{set:lj-archiver-post-link-prefix:javascript:alert(1)//<b>x</b>}</p>",
                "\\{set:lj-archiver-post-link-prefix:javascript:alert(1)//*x*}\n\n",
            ),
            // A remote fetch on every read.
            (
                "<p><b>image:https://evil.example/track.png</b>[x]</p>",
                "*\\image:\\https://evil.example/track.png*[x]\n\n",
            ),
            // An anchor with no href emits its authored label raw, so the whole
            // "generated" fragment is authored text.
            (
                "<p><a>link:javascript</a>:alert(1)[click]</p>",
                "\\link:\\javascript:alert(1)[click]\n\n",
            ),
            // A code span is a passthrough: Asciidoctor extracts it before it
            // substitutes anything, so nothing inside one needs escaping and
            // nothing inside one may be escaped.
            (
                "<p><code>link:javascript</code>:alert(1)[click]</p>",
                "`pass:c[link:javascript]`:alert(1)[click]\n\n",
            ),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html}");
        }
    }

    /// A description-list marker is positional, so a leading backslash does not
    /// deactivate it and the `pass:[:]` rewrite is the only remedy. Restoring
    /// the `::` before a generated fragment is therefore right only when that
    /// fragment makes the marker ineligible — which needs a visible character
    /// immediately after the delimiter.
    #[test]
    fn a_description_marker_is_only_restored_when_the_fragment_disqualifies_it() {
        for (html, expected) in [
            // Empty and space-leading fragments leave the marker eligible.
            ("<p>term::<i></i></p>", "term:pass:[:]\n\n"),
            ("<p>term::<b> x </b></p>", "term:pass:[:] *x*\n\n"),
            // A fragment that starts with a visible character disqualifies it,
            // and the block macro underneath takes its own escape.
            (
                r#"<p>include::<a href="x">y</a></p>"#,
                "\\include::x[y]\n\n",
            ),
            (
                "<p>include::<strong>secret[]</strong></p>",
                "\\include::*secret[]*\n\n",
            ),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html}");
        }
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn a_restored_description_delimiter_does_not_convert_to_a_definition_list() {
        for html in [
            "<p>term::<i></i></p>",
            "<p>term::<b> x </b></p>",
            r#"<p>include::<a href="x">y</a></p>"#,
        ] {
            let converted = convert_with_asciidoctor(&html_to_adoc(html));

            assert!(
                !converted.contains("<dl"),
                "authored text became a definition list for {html}: {converted}"
            );
        }
    }

    /// A generated fragment splits an authored run across a hard break just as
    /// well as within a line: an inline `+++` pair spans line breaks inside one
    /// paragraph, so the composed-line scan has to as well.
    #[test]
    fn a_generated_hard_break_cannot_split_an_inline_passthrough_pair() {
        assert_eq!(
            html_to_adoc("<p>+++<br>&lt;script&gt;alert(1)&lt;/script&gt;<br>+++</p>"),
            "\\+++ +\n<script>alert(1)</script> +\n+++\n\n"
        );
    }

    /// The block-children path a blockquote renders through appends generated
    /// inline output too, so it composes authored runs the same way.
    #[test]
    fn blockquote_block_children_cannot_split_an_authored_construct() {
        for (html, expected) in [
            (
                "<blockquote>+++<b>x</b>&lt;script&gt;alert(1)&lt;/script&gt;+++</blockquote>",
                "[quote]\n____\n\\+++*x*<script>alert(1)</script>+++\n____\n\n",
            ),
            (
                "<blockquote>link:javascript<b>x</b>:alert(1)[c]</blockquote>",
                "[quote]\n____\n\\link:javascript*x*:alert(1)[c]\n____\n\n",
            ),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html}");
        }
    }

    /// The scan is bounded to the current logical line, plus the resumable
    /// `+++` pair window. Pin that as a byte budget, because the suite's own
    /// fixtures are far too small for wall-clock timing to bound anything.
    #[test]
    fn composed_scanning_stays_linear_in_body_size() {
        // A paragraph of `<br>`-separated sentences is the dominant LiveJournal
        // body shape, and it is the shape a block-wide rescan on every append
        // makes quadratic. The scan window is the current *line* — plus the
        // resumable `+++` pair window — so total scanned bytes must stay a
        // constant multiple of the body, not a multiple of body × fragments.
        let sentence = "Some ordinary sentence of archived prose. ";
        let mut ratios = Vec::new();

        for sentences in [200usize, 400, 800] {
            let body = format!("<p>{}</p>", vec![sentence; sentences].join("<br>"));

            take_scanned_bytes();
            let rendered = html_to_adoc(&body);
            let scanned = take_scanned_bytes();

            assert!(rendered.contains("ordinary sentence"));
            ratios.push(scanned as f64 / body.len() as f64);
        }

        // Quadratic growth would double the ratio each time the body doubles.
        let worst = ratios.iter().cloned().fold(f64::MIN, f64::max);
        assert!(
            worst <= 8.0,
            "composed scanning read {worst:.1}x the body; ratios were {ratios:?}"
        );
    }

    /// The other clause of the rule: a construct one emitted span holds
    /// outright keeps the meaning its producer gave it, and no byte the
    /// renderer composed is ever rewritten. Every row here is
    /// renderer-generated output that must convert exactly as before —
    /// escaping any of it breaks working links, images and code spans.
    #[test]
    fn generated_inline_constructs_are_never_escaped_by_composition() {
        for (html, expected) in [
            ("<p>a <code>x</code> y</p>", "a `pass:c[x]` y\n\n"),
            (
                r#"<p><a href="https://e.example/x">t</a></p>"#,
                "https://e.example/x[t]\n\n",
            ),
            (
                r#"<p>see <a href="https://e.example/x">t</a> now</p>"#,
                "see https://e.example/x[t] now\n\n",
            ),
            (
                r#"<p><img src="https://e.example/i.png" alt="a"></p>"#,
                "image::https://e.example/i.png[a]\n\n",
            ),
            (
                concat!(
                    r#"<p>x <a href="https://e.example/p">"#,
                    r#"<img src="https://e.example/i.png" alt="a"></a> y</p>"#,
                ),
                "x image:https://e.example/i.png[a,link=https://e.example/p] y\n\n",
            ),
            (
                "<p>a <b>bold</b> b <em>it</em> c</p>",
                "a *bold* b _it_ c\n\n",
            ),
            ("<p>{not-an-attr} plain</p>", "\\{not-an-attr} plain\n\n"),
            ("<p>C++ and +++ alone</p>", "C++ and +++ alone\n\n"),
            ("<p>a<i></i>b</p>", "ab\n\n"),
            ("<p>1 + 2 <b>=</b> 3</p>", "1 + 2 *=* 3\n\n"),
            // The rows above all separate the macro from the authored text with
            // a space, which stops a macro target before it can reach past the
            // macro's own bracket. These do not: authored `[…]` abutting a
            // generated macro is what reaches into the renderer's attribute
            // list, and `link=` is the value that must survive it intact.
            (
                concat!(
                    r#"<p><a href="https://e.example/p">"#,
                    r#"<img src="https://e.example/i.png" alt="a"></a>[1]</p>"#,
                ),
                "image:https://e.example/i.png[a,link=https://e.example/p][1]\n\n",
            ),
            (
                concat!(
                    r#"<p>x<a href="https://e.example/p">"#,
                    r#"<img src="https://e.example/i.png" alt="a"></a>[1]y</p>"#,
                ),
                "ximage:https://e.example/i.png[a,link=https://e.example/p][1]y\n\n",
            ),
            (
                r#"<p><a href="https://e.example/x">t</a>[1]</p>"#,
                "https://e.example/x[t][1]\n\n",
            ),
            (
                r#"<p><img src="https://e.example/i.png" alt="a">[1]</p>"#,
                "image::https://e.example/i.png[a][1]\n\n",
            ),
        ] {
            assert_eq!(html_to_adoc(html), expected, "{html}");
        }
    }

    /// The cost of the rule, pinned rather than discovered later: authored text
    /// that composes a construct across a boundary gains the escape its
    /// single-run spelling has always had, even where the text is benign and
    /// the construct was never live. Inertness wins over fidelity; what must
    /// never happen is the renderer's own bytes gaining one, which the rows in
    /// `generated_inline_constructs_are_never_escaped_by_composition` pin.
    #[test]
    fn authored_text_gains_the_escape_its_single_run_spelling_has() {
        for (split, control, expected) in [
            (
                "<p>See <b>foo:bar</b>[1]</p>",
                "<p>See foo:bar[1]</p>",
                ("See *\\foo:bar*[1]\n\n", "See \\foo:bar[1]\n\n"),
            ),
            (
                "<p>See <i></i>foo:bar[1]</p>",
                "<p>See foo:bar[1]</p>",
                ("See \\foo:bar[1]\n\n", "See \\foo:bar[1]\n\n"),
            ),
        ] {
            assert_eq!(html_to_adoc(split), expected.0, "{split}");
            assert_eq!(html_to_adoc(control), expected.1, "{control}");
        }
    }

    /// Link localization emits generated attribute references and `xref:`
    /// macros. They must stay live, whatever authored text sits beside them.
    ///
    /// The abutting text has to contain a `[`: a macro target runs only as far
    /// as the next whitespace, so plain words beside a macro can never reach
    /// past its own bracket and the scan is never even tempted.
    #[test]
    fn localized_cross_post_links_survive_beside_authored_text() {
        let rewriter = news_rewriter();
        let adoc = html_to_adoc_with_links(
            concat!(
                r#"<p>[b]<a href="https://news.livejournal.com/166512.html">other</a>[1]</p>"#,
                r#"<p>x<a href="https://news.livejournal.com/166512.html">"#,
                r#"<img src="https://cdn.example.test/p.jpg" alt="a"></a>[1]</p>"#,
            ),
            Some(&rewriter),
        );

        assert!(adoc.contains("xref:166512.adoc[other]"), "{adoc:?}");
        assert!(
            adoc.contains("link={lj-archiver-post-link-prefix}166512.adoc]"),
            "{adoc:?}"
        );
        assert!(!adoc.contains(r"\https"), "{adoc:?}");
        assert!(!adoc.contains(r"\xref"), "{adoc:?}");
        assert!(
            !adoc.contains(r"{lj-archiver-post-link-prefix}\"),
            "{adoc:?}"
        );
    }

    /// A linked image's `link=` value is a *pending-link token* when the target
    /// is a same-journal post. An escape spliced into it stops the token
    /// matching what the renderer recorded, `validate_pending_links` fails on
    /// every namespace, and the post is dropped from the archive — content loss
    /// on ordinary gallery markup.
    #[test]
    fn a_pending_link_token_survives_authored_text_that_abuts_it() {
        for tail in ["[1]", "tail[1]", "<b>x[y]</b>", "<code>x</code>"] {
            let rewriter = LinkRewriter::pending(
                "news",
                HashSet::from([166512u64]),
                Url::parse("https://news.livejournal.com/1.html").unwrap(),
                "00112233445566778899aabbccddeeff",
            );
            let adoc = html_to_adoc_with_links(
                &format!(
                    concat!(
                        r#"<p><a href="https://news.livejournal.com/166512.html">"#,
                        r#"<img src="https://cdn.example.test/p.jpg" alt="a"></a>{}</p>"#,
                    ),
                    tail
                ),
                Some(&rewriter),
            );

            crate::links::validate_pending_links(&adoc, &rewriter.pending_links())
                .unwrap_or_else(|error| panic!("{tail}: {error:?} in {adoc:?}"));
        }
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn split_authored_inline_constructs_convert_inert() {
        for html in [
            format!("<p>+++<i></i>{SPLIT_SCRIPT}+++</p>"),
            format!("<p><b>+++</b>{SPLIT_SCRIPT}+++</p>"),
            format!("<p>+++<br>{SPLIT_SCRIPT}<br>+++</p>"),
            format!("<blockquote>+++<b>x</b>{SPLIT_SCRIPT}+++</blockquote>"),
            "<p>link:javascript<i></i>:alert(1)[click]</p>".to_string(),
            "<p>link:javascript:alert(1)<i></i>[click]</p>".to_string(),
            "<p><b>link:javascript</b>:alert(1)[click]</p>".to_string(),
            "<p><a>link:javascript</a>:alert(1)[click]</p>".to_string(),
            "<p>image:https://evil.example/track.png<i></i>[x]</p>".to_string(),
            "<p><b>image:https://evil.example/track.png</b>[x]</p>".to_string(),
            format!(r#"<p>hi pass:<a href="">{SPLIT_SCRIPT}</a></p>"#),
            format!("<p><b>pass:</b>[{SPLIT_SCRIPT}]</p>"),
        ] {
            let converted = convert_with_asciidoctor(&html_to_adoc(&html));
            let document = Html::parse_document(&converted);

            assert!(
                !converted.contains("<script"),
                "raw script reached the converted output for {html}: {converted}"
            );
            assert!(
                document
                    .select(&Selector::parse("a").unwrap())
                    .filter_map(|anchor| anchor.attr("href"))
                    .all(|href| !href.to_ascii_lowercase().starts_with("javascript:")),
                "a javascript: target reached the converted output for {html}: {converted}"
            );
            assert!(
                document
                    .select(&Selector::parse("img").unwrap())
                    .filter_map(|image| image.attr("src"))
                    .all(|src| !src.contains("evil.example")),
                "a remote image fetch reached the converted output for {html}: {converted}"
            );
        }
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn a_split_attribute_assignment_cannot_redirect_a_following_xref() {
        for html in [
            "<p>{set:lj-archiver-post-link-prefix<i></i>:javascript:alert(1)//}</p>",
            "<p>{set:lj-archiver-post-link-prefix:javascript:alert(1)//<b>x</b>}</p>",
            "<p><b>{set:lj-archiver-post-link-prefix:javascript:alert(1)//</b>}</p>",
        ] {
            let adoc = format!(
                concat!(
                    "ifndef::lj-archiver-post-link-prefix",
                    "[:lj-archiver-post-link-prefix:]\n\n",
                    "{}\n",
                    "xref:{{lj-archiver-post-link-prefix}}166511.adoc[other post]\n",
                ),
                html_to_adoc(html),
            );
            let converted = convert_with_asciidoctor(&adoc);
            let document = Html::parse_document(&converted);

            assert!(
                document
                    .select(&Selector::parse("a").unwrap())
                    .filter_map(|anchor| anchor.attr("href"))
                    .all(|href| href == "166511.html"),
                "the post-link prefix was hijacked for {html}: {converted}"
            );
        }
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn localized_cross_post_links_still_resolve_beside_authored_text() {
        let rewriter = news_rewriter();
        // The abutting text carries a `[`: plain words beside a macro stop the
        // target at the next space and never reach past the macro's own
        // bracket, so they cannot exercise the bound at all.
        let adoc = html_to_adoc_with_links(
            concat!(
                r#"<p>[b]<a href="https://news.livejournal.com/166512.html">other</a>[1]</p>"#,
                r#"<p>x<a href="https://news.livejournal.com/166512.html">"#,
                r#"<img src="https://cdn.example.test/p.jpg" alt="a"></a>[1]y</p>"#,
            ),
            Some(&rewriter),
        );
        let converted = convert_with_asciidoctor(&format!(
            "ifndef::lj-archiver-post-link-prefix[:lj-archiver-post-link-prefix:]\n\n{adoc}"
        ));
        let document = Html::parse_document(&converted);
        let hrefs: Vec<_> = document
            .select(&Selector::parse("a").unwrap())
            .filter_map(|anchor| anchor.attr("href"))
            .collect();

        // The anchor resolves through `xref:`, the linked image through the
        // `link=` attribute prefix — one localized `.html`, one localized
        // `.adoc` the book resolves — and both survive the authored `[…]` that
        // abuts them with no separating space.
        assert_eq!(hrefs, ["166512.html", "166512.adoc"], "got: {converted}");
        assert!(
            document
                .select(&Selector::parse("img").unwrap())
                .any(|image| image.attr("src") == Some("https://cdn.example.test/p.jpg")),
            "the localized linked image lost its source: {converted}"
        );
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
        assert!(adoc.contains("before\n\n[subs=specialcharacters]\n----\nalert(1)\n----\n\nafter"));
    }

    #[test]
    fn renders_nested_style_as_visible_source_text_instead_of_passthrough() {
        let adoc = html_to_adoc("<p>before<style>body{color:red}</style>after</p>");

        assert!(!adoc.contains("++++"));
        assert!(!adoc.contains("++++\n<style>body{color:red}</style>\n++++"));
        assert!(
            adoc.contains(
                "before\n\n[subs=specialcharacters]\n----\nbody{color:red}\n----\n\nafter"
            )
        );
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

        assert_eq!(
            html_to_adoc(&html),
            "[subs=specialcharacters]\n----\ndeep-marker\n----\n\n"
        );
    }

    #[test]
    fn deeply_nested_code_preserves_deepest_text() {
        let depth = 300;
        let html = format!(
            "<code>{}deep-marker{}</code>",
            "<span>".repeat(depth),
            "</span>".repeat(depth)
        );

        assert_eq!(html_to_adoc(&html), "`pass:c[deep-marker]`");
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
    fn ordinary_deleted_comment_words_render_the_complete_body() {
        use crate::parser::parse_post_page;
        use url::Url;

        let html = r#"
            <article data-journal="news" data-itemid="1">
              <div class="entry-content">post</div>
            </article>
            <section id="comments">
              <div class="comment" id="t1">
                <span class="comment-author">alice</span>
                <div class="comment-content"><p>The Deleted comment label is confusing.</p></div>
              </div>
            </section>
        "#;
        let post = parse_post_page(
            html,
            &Url::parse("https://news.livejournal.com/1.html").unwrap(),
        )
        .unwrap();

        let rendered = render_post(&post, true, None);

        assert!(rendered.contains("The Deleted comment label is confusing."));
        assert!(!rendered.contains("_Deleted comment._"));
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
    fn neutralizes_special_attribute_references_in_every_authored_text_context() {
        let mut comment = test_comment(0, false);
        comment.author = SPECIAL_ATTRIBUTE_FRAGMENT.to_string();
        comment.subject = Some(SPECIAL_ATTRIBUTE_FRAGMENT.to_string());
        comment.body_html = format!("<p>{SPECIAL_ATTRIBUTE_FRAGMENT}</p>");
        let mut post = test_post_with_comments(vec![comment]);
        post.title = SPECIAL_ATTRIBUTE_FRAGMENT.to_string();
        post.poster = Some(SPECIAL_ATTRIBUTE_FRAGMENT.to_string());
        post.tags = vec![SPECIAL_ATTRIBUTE_FRAGMENT.to_string()];
        post.body_html = format!(
            concat!(
                "<p>{0} <a href=\"https://example.test/\">{0}</a></p>",
                "<p><a href=\"https://news.livejournal.com/166512.html\">",
                "<img src=\"https://cdn.example.test/photo.jpg\" alt=\"{0}\"></a></p>",
            ),
            SPECIAL_ATTRIBUTE_FRAGMENT,
        );
        let rewriter = news_rewriter();

        let rendered = render_post(&post, true, Some(&rewriter));

        for special in [
            "{set:lj-archiver-post-link-prefix:javascript:alert(1)//}",
            "{counter:owned}",
            "{counter2:owned}",
        ] {
            assert!(
                rendered.matches(&format!(r"\{special}")).count() >= 7,
                "special attribute reference was not neutralized in every context: {rendered:?}"
            );
            assert!(
                !rendered.contains(&format!(" {special}")),
                "active special attribute reference remained: {rendered:?}"
            );
        }
        assert!(rendered.contains("link={lj-archiver-post-link-prefix}166512.adoc]"));
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
    fn generated_post_defaults_linked_image_prefix_for_standalone_rendering() {
        let mut post = test_post_with_comments(vec![]);
        post.body_html = concat!(
            r#"<a href="https://news.livejournal.com/166512.html">"#,
            r#"<img src="https://cdn.example.test/photo.jpg" alt="photo"></a>"#,
        )
        .to_string();
        let r = news_rewriter();

        let rendered = render_post(&post, false, Some(&r));

        assert_eq!(
            rendered,
            concat!(
                "= Post Title\n\n",
                ":lj-id: 166511\n",
                ":lj-url: https://news.livejournal.com/166511.html\n",
                ":lj-author: news\n",
                ":lj-date: 2024-12-17T16:01:00Z\n",
                "ifndef::lj-archiver-post-link-prefix",
                "[:lj-archiver-post-link-prefix:]\n\n",
                "image:https://cdn.example.test/photo.jpg[photo,",
                "link={lj-archiver-post-link-prefix}166512.adoc]\n",
            )
        );
        assert!(!rendered.ends_with("\n\n"), "got: {rendered:?}");
    }

    #[test]
    fn encodes_commas_and_authored_backslashes_before_macro_label_delimiters() {
        let adoc = html_to_adoc(concat!(
            "<p>",
            r#"<a href="https://one.example/">first\</a> "#,
            r#"<a href="https://two.example/">second</a> "#,
            r#"<img src="https://cdn.example/one.jpg" alt="x,link=javascript:alert(1)\"> "#,
            r#"<a href="https://safe.example/"><img src="https://cdn.example/two.jpg" alt="linked,alt\"></a>"#,
            "</p>",
        ));

        assert_eq!(
            adoc,
            concat!(
                "https://one.example/[first&#92;] ",
                "https://two.example/[second] ",
                "image::https://cdn.example/one.jpg[x&#44;link=javascript:alert(1)&#92;] ",
                "image:https://cdn.example/two.jpg[linked&#44;alt&#92;,",
                "link=https://safe.example/]\n\n",
            )
        );
    }

    #[test]
    fn encodes_authored_backslashes_in_xref_labels_before_the_closing_delimiter() {
        assert_eq!(sanitize_authored_xref_label(r"First\"), "First&#92;");
    }

    #[test]
    fn rendered_xref_labels_strip_only_guards_the_renderer_could_generate() {
        assert_eq!(
            sanitize_rendered_xref_label(r"\image:evil.png[] \pass:[safe]"),
            "image:evil.png pass: safe"
        );
        assert_eq!(
            sanitize_rendered_xref_label(r"\evil:[literal]"),
            r"\evil: literal"
        );
        assert_eq!(
            sanitize_rendered_xref_label(r"x\image:foo[]"),
            r"x\image:foo"
        );
        assert_eq!(
            sanitize_rendered_xref_label(r"x.\pass:[safe]"),
            "x.pass: safe"
        );
        assert_eq!(
            sanitize_rendered_xref_label(r"x-\pass:[safe]"),
            r"x-\pass: safe"
        );
    }

    #[test]
    fn encodes_terminal_backslashes_in_authored_heading_and_attribute_lines() {
        let mut post = test_post_with_comments(vec![]);
        post.title = "Post \\".to_string();
        post.poster = Some("poster \\".to_string());
        post.tags = vec!["tag \\".to_string()];
        post.body_html = concat!(
            r#"<a href="https://news.livejournal.com/166512.html">"#,
            r#"<img src="https://cdn.example.test/photo.jpg" alt="photo"></a>"#,
        )
        .to_string();
        let rewriter = news_rewriter();

        let rendered = render_post(&post, false, Some(&rewriter));

        assert!(
            rendered.starts_with("= Post &#92;\n\n"),
            "got: {rendered:?}"
        );
        assert!(
            rendered.contains(":lj-poster: poster &#92;\n:lj-date:"),
            "got: {rendered:?}"
        );
        assert!(
            rendered.contains(":lj-tags: tag &#92;\nifndef::lj-archiver-post-link-prefix"),
            "got: {rendered:?}"
        );
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

    #[test]
    fn direct_local_linked_image_uses_contextual_uri_not_xref_macro() {
        let r = LinkRewriter::new(
            "news",
            HashSet::from([2u64]),
            Url::parse("https://news.livejournal.com/1.html").unwrap(),
        );

        let adoc = html_to_adoc_with_links(
            r#"<a href="https://news.livejournal.com/2.html"><img src="https://cdn.example.test/photo.jpg" alt="photo"></a>"#,
            Some(&r),
        );

        assert_eq!(
            adoc,
            concat!(
                "image:https://cdn.example.test/photo.jpg[photo,",
                "link={lj-archiver-post-link-prefix}2.adoc]"
            )
        );
    }

    #[test]
    fn pending_local_linked_image_resolves_to_contextual_uri_not_xref_macro() {
        let r = LinkRewriter::pending(
            "news",
            HashSet::from([2u64]),
            Url::parse("https://news.livejournal.com/1.html").unwrap(),
            "00112233445566778899aabbccddeeff",
        );
        let pending = html_to_adoc_with_links(
            r#"<a href="https://news.livejournal.com/2.html"><img src="https://cdn.example.test/photo.jpg" alt="photo"></a>"#,
            Some(&r),
        );

        let resolved = resolve_pending_links(&pending, &r.pending_links(), &HashSet::from([2u64]))
            .unwrap()
            .contents;

        assert_eq!(
            resolved,
            concat!(
                "image:https://cdn.example.test/photo.jpg[photo,",
                "link={lj-archiver-post-link-prefix}2.adoc]"
            )
        );
    }

    #[test]
    fn failed_pending_linked_image_keeps_external_href_comma_hardened() {
        let r = LinkRewriter::pending(
            "news",
            HashSet::from([2u64]),
            Url::parse("https://news.livejournal.com/1.html").unwrap(),
            "00112233445566778899aabbccddeeff",
        );
        let pending = html_to_adoc_with_links(
            concat!(
                r#"<a href="https://news.livejournal.com/2.html?view=,role=red">"#,
                r#"<img src="https://cdn.example.test/photo.jpg" alt="photo"></a>"#,
            ),
            Some(&r),
        );

        let resolved = resolve_pending_links(&pending, &r.pending_links(), &HashSet::new())
            .unwrap()
            .contents;

        assert_eq!(
            resolved,
            concat!(
                "image:https://cdn.example.test/photo.jpg[photo,",
                "link=https://news.livejournal.com/2.html?view=%2Crole=red]"
            )
        );
        assert!(!resolved.contains(",role=red"), "got: {resolved:?}");
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn neutralizes_missing_constructs_after_asciidoctor_conversion() {
        let adoc = [
            html_to_adoc("<p>// visible comment</p>"),
            html_to_adoc("<p>- visible item</p>"),
            html_to_adoc("<p>term:: visible definition</p>"),
            html_to_adoc("<p>term::</p>"),
        ]
        .concat();

        let html = convert_with_asciidoctor(&adoc);
        let document = Html::parse_document(&html);
        let paragraphs = document
            .select(&Selector::parse("div.paragraph > p").unwrap())
            .map(|paragraph| paragraph.text().collect::<String>())
            .collect::<Vec<_>>();

        assert_eq!(
            paragraphs,
            [
                r"\// visible comment",
                r"\- visible item",
                "term:: visible definition",
                "term::",
            ],
            "got: {html}"
        );
        assert!(
            document
                .select(&Selector::parse(".ulist, .dlist").unwrap())
                .next()
                .is_none(),
            "unexpected list in: {html}"
        );
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn composed_line_start_neutralization_converts_to_distinct_literal_paragraphs() {
        let adoc = [
            html_to_adoc("<p>// term:: definition</p>"),
            html_to_adoc("<p>- term:: definition</p>"),
            html_to_adoc(r"<p>\term:: definition</p>"),
            html_to_adoc("<p>include::<br>later</p>"),
        ]
        .concat();

        let html = convert_with_asciidoctor(&adoc);
        let document = Html::parse_document(&html);
        let paragraphs = document
            .select(&Selector::parse("div.paragraph > p").unwrap())
            .map(|paragraph| paragraph.text().collect::<String>())
            .collect::<Vec<_>>();

        assert_eq!(
            paragraphs,
            [
                r"\// term:: definition",
                r"\- term:: definition",
                r"\term:: definition",
                "include::\nlater",
            ],
            "got: {html}"
        );
        assert!(
            document
                .select(&Selector::parse(".ulist, .dlist").unwrap())
                .next()
                .is_none(),
            "unexpected list in: {html}"
        );
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn colon_leading_linked_image_target_converts_inline_not_as_a_block() {
        let converted = convert_with_asciidoctor(&html_to_adoc(
            r#"<p><a href="https://example.test/"><img src=":evil.jpg" alt="x"></a></p>"#,
        ));
        let document = Html::parse_document(&converted);

        assert!(
            document
                .select(&Selector::parse("div.imageblock").unwrap())
                .next()
                .is_none(),
            "leading colon turned an inline linked image into a block macro: {converted}"
        );
        let paragraph = document
            .select(&Selector::parse("p").unwrap())
            .next()
            .expect("linked image stays inside a paragraph");
        let anchor = paragraph
            .select(&Selector::parse("a").unwrap())
            .next()
            .expect("linked image keeps its anchor");
        assert_eq!(anchor.attr("href"), Some("https://example.test/"));
        assert!(
            anchor
                .select(&Selector::parse("img").unwrap())
                .next()
                .is_some(),
            "anchor should wrap the image: {converted}"
        );
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn direct_local_linked_image_converts_to_relative_href() {
        let r = LinkRewriter::new(
            "news",
            HashSet::from([2u64]),
            Url::parse("https://news.livejournal.com/1.html").unwrap(),
        );
        let adoc = html_to_adoc_with_links(
            r#"<a href="https://news.livejournal.com/2.html"><img src="https://cdn.example.test/photo.jpg" alt="photo"></a>"#,
            Some(&r),
        );

        let html = convert_with_asciidoctor(&format!(
            "ifndef::lj-archiver-post-link-prefix[:lj-archiver-post-link-prefix:]\n\n{adoc}"
        ));

        assert!(html.contains("href=\"2.adoc\""), "got: {html}");
        assert!(!html.contains("href=\"xref:2.adoc\""), "got: {html}");
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn pending_linked_image_targets_convert_without_xref_or_attribute_split() {
        let local = LinkRewriter::pending(
            "news",
            HashSet::from([2u64]),
            Url::parse("https://news.livejournal.com/1.html").unwrap(),
            "00112233445566778899aabbccddeeff",
        );
        let local_pending = html_to_adoc_with_links(
            r#"<a href="https://news.livejournal.com/2.html"><img src="https://cdn.example.test/local.jpg" alt="local"></a>"#,
            Some(&local),
        );
        let local_adoc = resolve_pending_links(
            &local_pending,
            &local.pending_links(),
            &HashSet::from([2u64]),
        )
        .unwrap()
        .contents;

        let failed = LinkRewriter::pending(
            "news",
            HashSet::from([2u64]),
            Url::parse("https://news.livejournal.com/1.html").unwrap(),
            "00112233445566778899aabbccddeeff",
        );
        let failed_pending = html_to_adoc_with_links(
            concat!(
                r#"<a href="https://news.livejournal.com/2.html?view=,role=red">"#,
                r#"<img src="https://cdn.example.test/failed.jpg" alt="failed"></a>"#,
            ),
            Some(&failed),
        );
        let failed_adoc =
            resolve_pending_links(&failed_pending, &failed.pending_links(), &HashSet::new())
                .unwrap()
                .contents;

        let html = convert_with_asciidoctor(&format!(
            concat!(
                "ifndef::lj-archiver-post-link-prefix",
                "[:lj-archiver-post-link-prefix:]\n\n",
                "{}\n\n{}",
            ),
            local_adoc, failed_adoc
        ));

        assert!(html.contains("href=\"2.adoc\""), "got: {html}");
        assert!(!html.contains("href=\"xref:2.adoc\""), "got: {html}");
        assert!(
            html.contains(concat!(
                "href=\"https://news.livejournal.com/2.html?",
                "view=%2Crole=red\""
            )),
            "got: {html}"
        );
        assert!(!html.contains("class=\"image red\""), "got: {html}");
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn special_attribute_references_convert_visible_inert_before_generated_post_links() {
        let mut comment = test_comment(0, false);
        comment.author = SPECIAL_ATTRIBUTE_FRAGMENT.to_string();
        comment.subject = Some(SPECIAL_ATTRIBUTE_FRAGMENT.to_string());
        comment.body_html = format!("<p>{SPECIAL_ATTRIBUTE_FRAGMENT}</p>");
        let mut post = test_post_with_comments(vec![comment]);
        post.title = SPECIAL_ATTRIBUTE_FRAGMENT.to_string();
        post.poster = Some(SPECIAL_ATTRIBUTE_FRAGMENT.to_string());
        post.tags = vec![SPECIAL_ATTRIBUTE_FRAGMENT.to_string()];
        post.body_html = format!(
            concat!(
                "<p>{0} <a href=\"https://example.test/\">{0}</a></p>",
                "<p><a href=\"https://news.livejournal.com/166512.html\">",
                "<img src=\"https://cdn.example.test/photo.jpg\" alt=\"{0}\"></a></p>",
            ),
            SPECIAL_ATTRIBUTE_FRAGMENT,
        );

        let html = convert_with_asciidoctor(&render_post(&post, true, Some(&news_rewriter())));
        let document = Html::parse_document(&html);
        let active_hrefs = document
            .select(&Selector::parse("[href]").unwrap())
            .filter_map(|element| element.attr("href"))
            .collect::<Vec<_>>();

        assert!(active_hrefs.contains(&"166512.adoc"), "got: {html}");
        assert!(
            active_hrefs
                .iter()
                .all(|href| !href.to_ascii_lowercase().starts_with("javascript:")),
            "special attribute reference created an active URL: {html}"
        );
        let visible = document.root_element().text().collect::<String>();
        for literal in [
            "{set:lj-archiver-post-link-prefix:javascript:alert(1)//}",
            "{counter:owned}",
            "{counter2:owned}",
        ] {
            assert!(visible.contains(literal), "missing {literal:?} in: {html}");
        }
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn macro_label_delimiters_convert_without_injection_or_adjacent_macro_absorption() {
        let adoc = html_to_adoc(concat!(
            "<p>",
            r#"<a href="https://one.example/">first\</a> and "#,
            r#"<a href="https://two.example/">second</a></p>"#,
            "<p>",
            r#"<img src="https://cdn.example/one.jpg" alt="x,link=javascript:alert(1)"></p>"#,
            "<p>",
            r#"<a href="https://safe-one.example/"><img src="https://cdn.example/two.jpg" alt="linked,alt\"></a> and "#,
            r#"<a href="https://safe-two.example/"><img src="https://cdn.example/three.jpg" alt="third"></a>"#,
            "</p>",
        ));
        let html = convert_with_asciidoctor(&adoc);
        let document = Html::parse_document(&html);
        let hrefs = document
            .select(&Selector::parse("a[href]").unwrap())
            .filter_map(|anchor| anchor.attr("href"))
            .collect::<Vec<_>>();
        let images = document
            .select(&Selector::parse("img").unwrap())
            .collect::<Vec<_>>();

        assert!(hrefs.contains(&"https://one.example/"), "got: {html}");
        assert!(hrefs.contains(&"https://two.example/"), "got: {html}");
        assert!(hrefs.contains(&"https://safe-one.example/"), "got: {html}");
        assert!(hrefs.contains(&"https://safe-two.example/"), "got: {html}");
        assert!(
            hrefs
                .iter()
                .all(|href| !href.to_ascii_lowercase().starts_with("javascript:")),
            "authored alt injected an active URL: {html}"
        );
        assert_eq!(images.len(), 3, "adjacent image macro was absorbed: {html}");
        assert_eq!(images[0].attr("alt"), Some("x,link=javascript:alert(1)"));
        assert_eq!(images[1].attr("alt"), Some(r"linked,alt\"));
        assert_eq!(images[2].attr("alt"), Some("third"));

        let dir = tempdir().unwrap();
        let archive = crate::archive::Archive::new(dir.path(), "news");
        let authored = [
            (1, "image:evil.png[]", "= \\image:evil.png[]\n\nBody\n"),
            (
                2,
                "A | bracketed [title]",
                "= A | bracketed [title]\n\nBody\n",
            ),
            (3, "One, two", "= One, two\n\nBody\n"),
            (4, "{secret}", "= \\{secret}\n\nBody\n"),
            (5, r"First\", "= First&#92;\n\nBody\n"),
            (6, "Adjacent", "= Adjacent\n\nBody\n"),
        ]
        .map(|(id, title, contents)| crate::models::ArchivedPost {
            id,
            title: crate::models::ArchivedTitle::authored(title),
            path: archive.write_post(id, contents).unwrap(),
        });
        archive.write_index(&authored).unwrap();
        let first_index = fs::read_to_string(dir.path().join("index.adoc")).unwrap();

        let inspected = archive.inspect_existing().unwrap().unwrap();
        archive.write_index(&inspected.posts).unwrap();
        let second_index = fs::read_to_string(dir.path().join("index.adoc")).unwrap();

        for index in [&first_index, &second_index] {
            let index_html = convert_with_asciidoctor(index);
            let index_document = Html::parse_document(&index_html);
            let index_links = index_document
                .select(&Selector::parse("a[href]").unwrap())
                .map(|anchor| {
                    format!(
                        "{}|{}",
                        anchor.attr("href").unwrap(),
                        anchor.text().collect::<String>()
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                index_links,
                [
                    "posts/1.html|image:evil.png",
                    "posts/2.html|A bracketed title",
                    "posts/3.html|One, two",
                    "posts/4.html|{secret}",
                    r"posts/5.html|First\",
                    "posts/6.html|Adjacent",
                ],
                "one index label absorbed the next row: {index_html}"
            );
        }
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn terminal_backslashes_do_not_continue_into_renderer_owned_header_lines() {
        let mut post = test_post_with_comments(vec![]);
        post.title = "Post \\".to_string();
        post.poster = Some("poster \\".to_string());
        post.tags = vec!["tag \\".to_string()];
        post.body_html = concat!(
            r#"<a href="https://news.livejournal.com/166512.html">"#,
            r#"<img src="https://cdn.example.test/photo.jpg" alt="photo"></a>"#,
        )
        .to_string();
        let rendered = render_post(&post, false, Some(&news_rewriter()));
        let rendered = rendered.replacen("= Post &#92;", "== Post &#92;", 1);
        let html = convert_with_asciidoctor(&format!(
            "{rendered}\nMetadata: {{lj-poster}} | {{lj-date}} | {{lj-tags}}\n"
        ));
        let document = Html::parse_document(&html);
        let visible = document.root_element().text().collect::<String>();

        assert!(
            visible.contains(r"Post \"),
            "title line was corrupted: {html}"
        );
        assert!(
            visible.contains(r"Metadata: poster \ | 2024-12-17T16:01:00Z | tag \"),
            "attribute line continuation corrupted trusted header lines: {html}"
        );
        assert!(html.contains("href=\"166512.adoc\""), "got: {html}");
        assert!(
            !html.contains("{lj-archiver-post-link-prefix}"),
            "got: {html}"
        );
    }

    #[test]
    fn unsafe_url_attributes_degrade_passthrough_to_exact_source_listing() {
        let source = concat!(
            r#"<section data-kind='opaque'><a href="javascript:alert(1)">script link</a>"#,
            r#"<a href='mailto:reader@example.test'>mail link</a>"#,
            r#"<img src="data:image/png,boom" alt="data image">"#,
            r#"<img src='file:///secret.jpg' alt='file image'></section>"#,
        );
        let r = LinkRewriter::new(
            "news",
            HashSet::new(),
            Url::parse("https://news.livejournal.com/1.html").unwrap(),
        );

        let adoc = html_to_adoc_with_links(source, Some(&r));

        assert_eq!(
            adoc,
            format!("[subs=specialcharacters]\n----\n{source}\n----\n\n")
        );
        assert!(!adoc.contains("++++"), "got: {adoc:?}");
    }

    #[test]
    fn safe_url_attributes_keep_raw_passthrough_control() {
        let source = concat!(
            r#"<section title='safe'><a href="/profile">safe link</a>"#,
            r#"<img src='photo.jpg' alt='safe image'></section>"#,
        );
        let r = LinkRewriter::new(
            "news",
            HashSet::new(),
            Url::parse("https://news.livejournal.com/archive/1.html").unwrap(),
        );

        let adoc = html_to_adoc_with_links(source, Some(&r));

        assert_eq!(
            adoc,
            concat!(
                "++++\n",
                r#"<section title='safe'><a href="https://news.livejournal.com/profile">safe link</a>"#,
                r#"<img src='https://news.livejournal.com/archive/photo.jpg' alt='safe image'></section>"#,
                "\n++++\n\n",
            ),
            "only the recovered value ranges change: quoting, spacing and every \
             other attribute stay byte-identical"
        );
    }

    /// A rewriter as an archived post gets one: the delivered page is a post of
    /// the journal, and one sibling post is in the export.
    fn archived_post_rewriter() -> LinkRewriter {
        LinkRewriter::new(
            "news",
            HashSet::from([166512u64]),
            Url::parse("https://news.livejournal.com/166511.html").unwrap(),
        )
    }

    #[test]
    fn raw_passthrough_localizes_same_journal_links_per_context() {
        let rewriter = archived_post_rewriter();
        let html = r#"<table><tr><td><a href="/166512.html">post</a></td></tr></table>"#;

        assert_eq!(
            html_to_adoc_with_links(html, Some(&rewriter)),
            concat!(
                "ifndef::lj-archiver-book[]\n",
                "++++\n",
                r#"<table><tr><td><a href="166512.html">post</a></td></tr></table>"#,
                "\n++++\nendif::[]\n",
                "ifdef::lj-archiver-book[]\n",
                "++++\n",
                r#"<table><tr><td><a href="posts/166512.html">post</a></td></tr></table>"#,
                "\n++++\nendif::[]\n\n",
            )
        );
    }

    #[test]
    fn raw_passthrough_localizes_downloaded_media_per_context() {
        let rewriter = archived_post_rewriter();
        let html = r#"<table><tr><td><img src="../assets/a.png" alt="a"></td></tr></table>"#;

        assert_eq!(
            html_to_adoc_with_links(html, Some(&rewriter)),
            concat!(
                "ifndef::lj-archiver-book[]\n",
                "++++\n",
                r#"<table><tr><td><img src="../assets/a.png" alt="a"></td></tr></table>"#,
                "\n++++\nendif::[]\n",
                "ifdef::lj-archiver-book[]\n",
                "++++\n",
                r#"<table><tr><td><img src="assets/a.png" alt="a"></td></tr></table>"#,
                "\n++++\nendif::[]\n\n",
            )
        );
    }

    #[test]
    fn raw_passthrough_absolutizes_context_independent_remote_references() {
        let rewriter = archived_post_rewriter();
        let html = concat!(
            r#"<table><tr><td>"#,
            r#"<a href="//cdn.example.test/p">protocol</a>"#,
            r#"<a href="/tag/x">root</a>"#,
            r#"<a href="https://other.example.test/p">hosted</a>"#,
            r#"</td></tr></table>"#,
        );

        assert_eq!(
            html_to_adoc_with_links(html, Some(&rewriter)),
            concat!(
                "++++\n",
                r#"<table><tr><td>"#,
                r#"<a href="https://cdn.example.test/p">protocol</a>"#,
                r#"<a href="https://news.livejournal.com/tag/x">root</a>"#,
                r#"<a href="https://other.example.test/p">hosted</a>"#,
                r#"</td></tr></table>"#,
                "\n++++\n\n",
            ),
            "context-independent rewrites stay one block"
        );
    }

    #[test]
    fn direct_raw_passthrough_invents_no_archive_context() {
        let html = concat!(
            r#"<table><tr><td>"#,
            r#"<a href="/tag/x">root</a>"#,
            r#"<img src="../assets/a.png" alt="a">"#,
            r#"</td></tr></table>"#,
        );

        assert_eq!(
            html_to_adoc(html),
            format!("++++\n{html}\n++++\n\n"),
            "without a rewriter raw references stay exactly as delivered"
        );
    }

    #[test]
    fn raw_passthrough_with_a_conditional_directive_line_fails_closed() {
        let rewriter = archived_post_rewriter();
        let html = concat!(
            "<table><tr><td>\n",
            "endif::[]\n",
            r#"<a href="/166512.html">post</a>"#,
            "\n</td></tr></table>",
        );
        let rendered = html_to_adoc_with_links(html, Some(&rewriter));

        assert!(
            rendered.starts_with("[subs=specialcharacters]\n----\n"),
            "authored preprocessor directives must fail closed to a listing: {rendered}"
        );
        assert!(!rendered.contains("ifdef::"), "{rendered}");
        assert!(!rendered.contains("ifndef::"), "{rendered}");
    }

    /// Escaping runs on the *content* of each variant, never on the generated
    /// wrapper: those `ifndef::` / `ifdef::` / `endif::` lines are the
    /// renderer's own and must keep selecting exactly one variant.
    #[test]
    fn conditional_raw_passthrough_escapes_include_lines_in_both_variants() {
        let rewriter = archived_post_rewriter();
        let html = concat!(
            "<table><tr><td>\n",
            "include::secret.txt[]\n",
            r#"<a href="/166512.html">post</a>"#,
            "\n</td></tr></table>",
        );

        assert_eq!(
            html_to_adoc_with_links(html, Some(&rewriter)),
            concat!(
                "ifndef::lj-archiver-book[]\n",
                "++++\n",
                "<table><tr><td>\n\\include::secret.txt[]\n",
                r#"<a href="166512.html">post</a>"#,
                "\n</td></tr></table>\n",
                "++++\nendif::[]\n",
                "ifdef::lj-archiver-book[]\n",
                "++++\n",
                "<table><tr><td>\n\\include::secret.txt[]\n",
                r#"<a href="posts/166512.html">post</a>"#,
                "\n</td></tr></table>\n",
                "++++\nendif::[]\n\n",
            )
        );
    }

    #[test]
    fn raw_passthrough_localizes_unquoted_attribute_values() {
        let rewriter = archived_post_rewriter();
        let html = "<table><tr><td><a href=/166512.html>post</a></td></tr></table>";

        assert_eq!(
            html_to_adoc_with_links(html, Some(&rewriter)),
            concat!(
                "ifndef::lj-archiver-book[]\n",
                "++++\n",
                "<table><tr><td><a href=166512.html>post</a></td></tr></table>",
                "\n++++\nendif::[]\n",
                "ifdef::lj-archiver-book[]\n",
                "++++\n",
                "<table><tr><td><a href=posts/166512.html>post</a></td></tr></table>",
                "\n++++\nendif::[]\n\n",
            ),
            "a replacement must stay a single unquoted token"
        );
    }

    #[test]
    fn raw_passthrough_keeps_character_references_inside_localized_media() {
        let rewriter = archived_post_rewriter();
        let html = r#"<table><tr><td><img src="../assets/a&#38;b.png"></td></tr></table>"#;

        assert_eq!(
            html_to_adoc_with_links(html, Some(&rewriter)),
            concat!(
                "ifndef::lj-archiver-book[]\n",
                "++++\n",
                r#"<table><tr><td><img src="../assets/a&#38;b.png"></td></tr></table>"#,
                "\n++++\nendif::[]\n",
                "ifdef::lj-archiver-book[]\n",
                "++++\n",
                r#"<table><tr><td><img src="assets/a&#38;b.png"></td></tr></table>"#,
                "\n++++\nendif::[]\n\n",
            ),
            "the asset spellings are the original bytes, so no character \
             reference is decoded into the output"
        );
    }

    #[test]
    fn raw_passthrough_absolutization_does_not_sanitize_macro_syntax() {
        let rewriter = archived_post_rewriter();
        let html = r#"<table><tr><td><a href="/p?q=[1]&amp;r=a::b">brackets</a></td></tr></table>"#;

        assert_eq!(
            html_to_adoc_with_links(html, Some(&rewriter)),
            concat!(
                "++++\n",
                r#"<table><tr><td>"#,
                r#"<a href="https://news.livejournal.com/p?q=[1]&amp;r=a::b">brackets</a>"#,
                r#"</td></tr></table>"#,
                "\n++++\n\n",
            ),
            "raw HTML is not a macro target: brackets and `::` stay literal, and \
             only `&` is re-encoded for the attribute"
        );
    }

    /// Escaping and revalidation cover different halves of one attack. A
    /// *quoted* value is held by the escaper alone: `"` becomes `&quot;`, so the
    /// hostile filename stays one value. An *unquoted* value is not — the
    /// escaper leaves whitespace alone, so the same replacement splits into a
    /// second attribute, and only rescanning the spliced variant catches the
    /// `onerror` that appears.
    #[test]
    fn raw_passthrough_cannot_be_broken_out_of_by_a_hostile_post_filename() {
        let rewriter = LinkRewriter::with_targets(
            "news",
            std::sync::Arc::new(std::collections::HashMap::from([(
                166512u64,
                r#"x" onerror="alert(1)"#.to_string() + ".adoc",
            )])),
            Url::parse("https://news.livejournal.com/166511.html").unwrap(),
        );

        let quoted = r#"<table><tr><td><img src="/166512.html" alt="a"></td></tr></table>"#;
        let rendered = html_to_adoc_with_links(quoted, Some(&rewriter));
        assert!(
            rendered.contains(r#"src="x&quot; onerror=&quot;alert(1).html""#),
            "the replacement must stay inside its quoting: {rendered}"
        );
        assert!(!rendered.contains("onerror=\""), "{rendered}");

        let unquoted = "<table><tr><td><img src=/166512.html alt=a></td></tr></table>";
        let rendered = html_to_adoc_with_links(unquoted, Some(&rewriter));
        assert_eq!(
            rendered,
            format!("[subs=specialcharacters]\n----\n{unquoted}\n----\n\n"),
            "an unquoted value the replacement would split into a second \
             attribute must fail closed"
        );
        assert!(!rendered.contains("++++"), "{rendered}");
    }

    #[test]
    fn raw_passthrough_revalidation_rejects_a_replacement_with_an_active_scheme() {
        let rewriter = LinkRewriter::with_targets(
            "news",
            std::sync::Arc::new(std::collections::HashMap::from([(
                166512u64,
                "javascript:alert(1).adoc".to_string(),
            )])),
            Url::parse("https://news.livejournal.com/166511.html").unwrap(),
        );
        let html = r#"<table><tr><td><a href="/166512.html">post</a></td></tr></table>"#;

        assert_eq!(
            html_to_adoc_with_links(html, Some(&rewriter)),
            format!("[subs=specialcharacters]\n----\n{html}\n----\n\n"),
            "a spliced variant is re-checked under the same URL policy the \
             original passed"
        );
    }

    #[test]
    fn raw_passthrough_without_a_recovered_value_range_fails_closed() {
        let rewriter = archived_post_rewriter();
        let html = "<table><tr><td><a href>valueless</a></td></tr></table>";

        assert_eq!(
            html_to_adoc_with_links(html, Some(&rewriter)),
            format!("[subs=specialcharacters]\n----\n{html}\n----\n\n"),
            "a reference with no recovered range is never guessed at"
        );
    }

    /// A conditional in reviewed HTML degrades to a listing even standing alone,
    /// and the listing then escapes it. Escaping it *in place* is not an option:
    /// see `source_html::contains_conditional_directive_line`.
    #[test]
    fn unconditional_raw_passthrough_with_a_directive_line_fails_closed() {
        let html = concat!(
            "<table><tr><td>\n",
            "ifdef::never-defined[]\n",
            "</td></tr></table>",
        );
        let rendered = html_to_adoc(html);

        assert_eq!(
            rendered,
            concat!(
                "[subs=specialcharacters]\n----\n",
                "<table><tr><td>\n\\ifdef::never-defined[]\n</td></tr></table>",
                "\n----\n\n",
            ),
            "an authored directive would swallow the rest of the document"
        );
    }

    #[test]
    fn unsafe_namespaced_url_attributes_degrade_exact_passthrough_to_listing() {
        let source = concat!(
            r#"<section data-kind='svg'><svg xmlns="http://www.w3.org/2000/svg" "#,
            r#"xmlns:xlink="http://www.w3.org/1999/xlink">"#,
            r#"<a xlink:href="javascript:alert(1)"><text>script label</text></a>"#,
            r#"<a xlink:href="data:text/plain,boom"><text>data label</text></a>"#,
            r#"<a xlink:href="file:///secret.svg"><text>file label</text></a>"#,
            r#"<a xlink:href="mailto:reader@example.test"><text>mail label</text></a>"#,
            r#"</svg></section>"#,
        );
        let r = LinkRewriter::new(
            "news",
            HashSet::new(),
            Url::parse("https://news.livejournal.com/1.html").unwrap(),
        );

        let adoc = html_to_adoc_with_links(source, Some(&r));

        assert_eq!(
            adoc,
            format!("[subs=specialcharacters]\n----\n{source}\n----\n\n")
        );
        assert!(!adoc.contains("++++"), "got: {adoc:?}");
    }

    #[test]
    fn hosted_namespaced_url_attribute_still_fails_closed_to_listing() {
        let source = concat!(
            r#"<section data-kind='safe-svg'><svg xmlns="http://www.w3.org/2000/svg" "#,
            r#"xmlns:xlink="http://www.w3.org/1999/xlink">"#,
            r#"<a xlink:href="https://safe.example.test/page">"#,
            r#"<text>safe svg label</text></a></svg></section>"#,
        );
        let r = LinkRewriter::new(
            "news",
            HashSet::new(),
            Url::parse("https://news.livejournal.com/1.html").unwrap(),
        );

        let adoc = html_to_adoc_with_links(source, Some(&r));

        assert_eq!(
            adoc,
            format!("[subs=specialcharacters]\n----\n{source}\n----\n\n")
        );
    }

    #[test]
    fn foster_parented_unsafe_url_inside_emitted_source_degrades_to_listing() {
        let table = concat!(
            r#"<table><a href="javascript:alert(1)">fostered link</a>"#,
            r#"<tr><td>cell</td></tr></table>"#,
        );
        let r = LinkRewriter::new(
            "news",
            HashSet::new(),
            Url::parse("https://news.livejournal.com/1.html").unwrap(),
        );

        let adoc = html_to_adoc_with_links(table, Some(&r));

        assert!(
            adoc.contains(&format!(
                "[subs=specialcharacters]\n----\n{table}\n----\n\n"
            )),
            "got: {adoc:?}"
        );
        assert!(!adoc.contains("++++"), "got: {adoc:?}");
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn passthrough_url_safety_converts_unsafe_source_inert_and_safe_control_raw() {
        let unsafe_source = concat!(
            r#"<section><a href="javascript:alert(1)">script label</a>"#,
            r#"<a href="data:text/plain,boom">data label</a>"#,
            r#"<a href="file:///secret.txt">file label</a>"#,
            r#"<a href="mailto:reader@example.test">mail label</a>"#,
            r#"<img src="javascript:alert(2)" alt="script image">"#,
            r#"<img src="data:image/png,boom" alt="data image">"#,
            r#"<img src="file:///secret.jpg" alt="file image">"#,
            r#"<img src="mailto:image@example.test" alt="mail image"></section>"#,
        );
        let safe_source = concat!(
            r#"<section><a href="https://safe.example.test/page">safe label</a>"#,
            r#"<img src="https://safe.example.test/photo.jpg" alt="safe image"></section>"#,
        );
        let r = LinkRewriter::new(
            "news",
            HashSet::new(),
            Url::parse("https://news.livejournal.com/1.html").unwrap(),
        );
        let unsafe_adoc = html_to_adoc_with_links(unsafe_source, Some(&r));
        let safe_adoc = html_to_adoc_with_links(safe_source, Some(&r));

        let html = convert_with_asciidoctor(&format!("{unsafe_adoc}\n{safe_adoc}"));
        let document = Html::parse_document(&html);
        let url_attributes = Selector::parse("[href], [src]").unwrap();
        let active_values: Vec<_> = document
            .select(&url_attributes)
            .flat_map(|element| [element.attr("href"), element.attr("src")])
            .flatten()
            .collect();

        for value in &active_values {
            assert!(
                !["javascript:", "data:", "file:", "mailto:"]
                    .iter()
                    .any(|scheme| value.to_ascii_lowercase().starts_with(scheme)),
                "active unsafe URL {value:?} in: {html}"
            );
        }
        assert!(
            active_values.contains(&"https://safe.example.test/page"),
            "got: {html}"
        );
        assert!(
            active_values.contains(&"https://safe.example.test/photo.jpg"),
            "got: {html}"
        );
        let visible_text = document.root_element().text().collect::<String>();
        for visible in [
            "javascript:alert(1)",
            "data:image/png,boom",
            "file:///secret.jpg",
            "mailto:image@example.test",
            "script label",
            "mail image",
        ] {
            assert!(
                visible_text.contains(visible),
                "missing {visible:?} in: {html}"
            );
        }
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn active_capabilities_convert_inert_and_reviewed_table_stays_raw() {
        let event_handler = r#"<table onclick="alert(1)"><tr><td>event cell</td></tr></table>"#;
        let srcdoc = r#"<iframe srcdoc="<script>alert(2)</script>"></iframe>"#;
        let safe_table = concat!(
            r#"<table class="record"><thead><tr><th scope="col">Name</th></tr></thead>"#,
            "<tbody><tr><td>value</td></tr></tbody></table>",
        );
        let adoc = format!(
            "{}\n{}\n{}",
            html_to_adoc(event_handler),
            html_to_adoc(srcdoc),
            html_to_adoc(safe_table)
        );

        let html = convert_with_asciidoctor(&adoc);
        let document = Html::parse_document(&html);

        assert!(
            document
                .select(&Selector::parse("[onclick], iframe, script").unwrap())
                .next()
                .is_none()
        );
        assert!(
            document
                .root_element()
                .text()
                .collect::<String>()
                .contains("onclick")
        );
        assert!(
            document
                .select(&Selector::parse("table.record").unwrap())
                .next()
                .is_some()
        );
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn custom_inline_capability_converts_inert_visible_and_block_separated() {
        let source = concat!(
            "before",
            r#"<lj-embed data-id="1" onclick="alert(1)">inside</lj-embed>"#,
            "after",
        );
        let adoc = html_to_adoc(source);

        assert_eq!(
            adoc,
            concat!(
                "before\n\n",
                "[subs=specialcharacters]\n",
                "----\n",
                r#"<lj-embed data-id="1" onclick="alert(1)">inside</lj-embed>"#,
                "\n----\n\n",
                "after\n\n",
            )
        );

        let html = convert_with_asciidoctor(&adoc);
        let document = Html::parse_document(&html);
        assert!(
            document
                .select(&Selector::parse("[onclick], lj-embed").unwrap())
                .next()
                .is_none(),
            "got: {html}"
        );
        let visible = document.root_element().text().collect::<String>();
        for text in ["before", "lj-embed", "onclick", "inside", "after"] {
            assert!(visible.contains(text), "missing {text:?} in: {html}");
        }
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn raw_text_containers_with_custom_descendants_convert_inert_visible_and_separate() {
        let code_source = "<code>before<foo onclick='code-action'>inside</foo>after</code>";
        let pre_source = "<pre>before<foo onclick='pre-action'>inside</foo>after</pre>";
        let adoc = format!("{}{}", html_to_adoc(code_source), html_to_adoc(pre_source));

        assert_eq!(
            adoc,
            format!(
                concat!(
                    "[subs=specialcharacters]\n----\n{}\n----\n\n",
                    "[subs=specialcharacters]\n----\n{}\n----\n\n",
                ),
                code_source, pre_source,
            )
        );

        let html = convert_with_asciidoctor(&adoc);
        let document = Html::parse_document(&html);
        assert!(
            document
                .select(&Selector::parse("[onclick], foo").unwrap())
                .next()
                .is_none(),
            "got: {html}"
        );
        let listings = document
            .select(&Selector::parse("div.listingblock pre").unwrap())
            .map(|element| element.text().collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(listings, [code_source, pre_source]);
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn svg_xlink_passthrough_safety_is_inert_after_conversion() {
        let unsafe_source = concat!(
            r#"<section><svg xmlns="http://www.w3.org/2000/svg" "#,
            r#"xmlns:xlink="http://www.w3.org/1999/xlink">"#,
            r#"<a xlink:href="javascript:alert(1)"><text>script svg</text></a>"#,
            r#"<a xlink:href="data:text/plain,boom"><text>data svg</text></a>"#,
            r#"<a xlink:href="file:///secret.svg"><text>file svg</text></a>"#,
            r#"<a xlink:href="mailto:reader@example.test"><text>mail svg</text></a>"#,
            r#"</svg></section>"#,
        );
        let safe_source = concat!(
            r#"<section><svg xmlns="http://www.w3.org/2000/svg" "#,
            r#"xmlns:xlink="http://www.w3.org/1999/xlink">"#,
            r#"<a xlink:href="https://safe.example.test/page">"#,
            r#"<text>safe svg</text></a></svg></section>"#,
        );
        let r = LinkRewriter::new(
            "news",
            HashSet::new(),
            Url::parse("https://news.livejournal.com/1.html").unwrap(),
        );
        let unsafe_adoc = html_to_adoc_with_links(unsafe_source, Some(&r));
        let safe_adoc = html_to_adoc_with_links(safe_source, Some(&r));

        let html = convert_with_asciidoctor(&format!("{unsafe_adoc}\n{safe_adoc}"));
        let document = Html::parse_document(&html);
        let active_values: Vec<_> = document
            .tree
            .root()
            .descendants()
            .filter_map(ElementRef::wrap)
            .flat_map(|element| {
                element
                    .value()
                    .attrs()
                    .filter(|(local_name, _)| matches!(*local_name, "href" | "src"))
                    .map(|(_, value)| value)
            })
            .collect();

        for value in &active_values {
            assert!(
                !["javascript:", "data:", "file:", "mailto:"]
                    .iter()
                    .any(|scheme| value.to_ascii_lowercase().starts_with(scheme)),
                "active unsafe URL {value:?} in: {html}"
            );
        }
        assert!(
            !active_values.contains(&"https://safe.example.test/page"),
            "got: {html}"
        );
        assert!(
            document
                .select(&Selector::parse("svg").unwrap())
                .next()
                .is_none(),
            "got: {html}"
        );
        let visible_text = document.root_element().text().collect::<String>();
        for visible in [
            "javascript:alert(1)",
            "data:text/plain,boom",
            "file:///secret.svg",
            "mailto:reader@example.test",
            "script svg",
            "mail svg",
            "safe svg",
        ] {
            assert!(
                visible_text.contains(visible),
                "missing {visible:?} in: {html}"
            );
        }
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn additional_block_constructs_remain_literal_after_conversion() {
        let markers = [
            "<<<",
            "'''",
            ".Authored title",
            "--",
            "|===",
            ",===",
            ":===",
            "!===",
        ];
        let html = markers
            .iter()
            .enumerate()
            .map(|(index, marker)| format!("<p>{marker}<br>after-{index}</p>"))
            .collect::<String>();
        let converted = convert_with_asciidoctor(&html_to_adoc(&html));
        let document = Html::parse_document(&converted);
        let visible = document.root_element().text().collect::<String>();

        for (index, marker) in markers.iter().enumerate() {
            assert!(visible.contains(marker), "{marker}: {converted}");
            assert!(visible.contains(&format!("after-{index}")), "{converted}");
        }
        assert_eq!(
            document.select(&Selector::parse("table").unwrap()).count(),
            0
        );
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn space_prefixed_block_constructs_hide_protection_after_conversion() {
        let markers = [
            "<<<",
            "'''",
            ".Authored title",
            "--",
            "|===",
            ",===",
            ":===",
            "!===",
        ];
        let spaced = markers
            .iter()
            .enumerate()
            .map(|(index, marker)| format!("<p>before-{index}<br> {marker} <br>after-{index}</p>"))
            .collect::<String>();
        let html =
            format!("{spaced}<p>control-before<br><br>include::secret[]<br><br>control-after</p>");
        let converted = convert_with_asciidoctor(&html_to_adoc(&html));
        let document = Html::parse_document(&converted);
        let visible = document.root_element().text().collect::<String>();

        for (index, marker) in markers.iter().enumerate() {
            assert!(visible.contains(marker), "{marker}: {converted}");
            assert!(visible.contains(&format!("after-{index}")), "{converted}");
        }
        assert!(visible.contains("include::secret[]"), "{converted}");
        assert!(visible.contains("control-after"), "{converted}");
        assert!(
            !visible.contains('\\'),
            "visible protection slash: {converted}"
        );
        assert!(
            document
                .select(&Selector::parse("table, hr, .title, [style*='page-break']").unwrap())
                .next()
                .is_none(),
            "activated authored construct: {converted}"
        );
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn explicit_non_http_image_schemes_are_not_emitted_after_conversion() {
        let html = concat!(
            r#"<p><img src="data:image/svg+xml;base64,PHN2Zy8+" alt="data"></p>"#,
            r#"<p><img src="javascript:alert(1)" alt="javascript"></p>"#,
            r#"<p><img src="file:///etc/passwd" alt="file"></p>"#,
            r#"<p><img src="mailto:user@example.test" alt="mailto"></p>"#,
            r#"<p><img src="relative/pic.png" alt="relative"></p>"#,
            r#"<p><img src="https://cdn.example/pic.png" alt="remote"></p>"#,
        );
        let converted = convert_with_asciidoctor(&html_to_adoc(html));
        let document = Html::parse_document(&converted);
        let images = document
            .select(&Selector::parse("img").unwrap())
            .collect::<Vec<_>>();
        let sources = images
            .iter()
            .map(|image| image.attr("src").unwrap())
            .collect::<Vec<_>>();

        assert_eq!(sources, ["relative/pic.png", "https://cdn.example/pic.png"]);
        assert!(sources.iter().all(|src| {
            !["data:", "javascript:", "file:", "mailto:"]
                .iter()
                .any(|scheme| src.to_ascii_lowercase().starts_with(scheme))
        }));
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn explicit_non_http_anchor_schemes_are_not_emitted_after_conversion() {
        let html = concat!(
            r#"<p><a href="javascript:alert(1)">javascript</a></p>"#,
            r#"<p><a href="data:text/html,x">data</a></p>"#,
            r#"<p><a href="file:///etc/passwd">file</a></p>"#,
            r#"<p><a href="mailto:user@example.test">mailto</a></p>"#,
            r#"<p><a href="ftp://example.test/f">ftp</a></p>"#,
            r#"<p><a href="relative/page.html">relative</a></p>"#,
            r#"<p><a href="https://example.test/p">remote</a></p>"#,
        );
        let converted = convert_with_asciidoctor(&html_to_adoc(html));
        let document = Html::parse_document(&converted);
        let targets = document
            .select(&Selector::parse("a").unwrap())
            .filter_map(|anchor| anchor.attr("href"))
            .collect::<Vec<_>>();

        // The relative href converts to inert, non-hyperlinked text, not an
        // `<a>` element: Asciidoctor's bare `target[text]` autolink
        // recognition requires a URI scheme (unlike the explicit `link:` or
        // `xref:` macro forms used elsewhere in this renderer), and a
        // scheme-less reference keeps that inert shape both before and after
        // this policy change. Only the hosted HTTP(S) target survives as a
        // real link.
        assert_eq!(targets, ["https://example.test/p"]);
        assert!(targets.iter().all(|href| {
            !["javascript:", "data:", "file:", "mailto:", "ftp:"]
                .iter()
                .any(|scheme| href.to_ascii_lowercase().starts_with(scheme))
        }));
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn guarded_generated_inline_lines_convert_as_paragraph_text() {
        let converted = convert_with_asciidoctor(&html_to_adoc("<p>.<strong>Title</strong></p>"));
        let document = Html::parse_document(&converted);

        assert!(
            document
                .select(&Selector::parse(".title").unwrap())
                .next()
                .is_none(),
            "generated inline output produced a block title: {converted}"
        );
        let paragraph = document
            .select(&Selector::parse("p").unwrap())
            .next()
            .expect("guarded line converts to a paragraph");
        assert_eq!(paragraph.text().collect::<String>(), ".Title");
        assert_eq!(
            paragraph
                .select(&Selector::parse("strong").unwrap())
                .next()
                .map(|strong| strong.text().collect::<String>()),
            Some("Title".to_string())
        );
    }

    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn foster_parented_table_content_converts_once() {
        let html = r#"<table><tr><td><div>CELL</div></td></tr><div>FOSTER</div></table>"#;
        let converted = convert_with_asciidoctor(&html_to_adoc(html));

        assert_eq!(converted.matches("FOSTER").count(), 1, "{converted}");
        assert_eq!(converted.matches("CELL").count(), 1, "{converted}");
        assert_eq!(
            Html::parse_document(&converted)
                .select(&Selector::parse("table").unwrap())
                .count(),
            1
        );
    }

    /// Interleaved source ranges must not put unbalanced markup into the
    /// converted document: both ranges are emitted as inert listings, so the
    /// stray `</table>` stays text and the document grows no table at all.
    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn crossing_source_ranges_convert_without_unbalanced_markup() {
        let html = r#"<table><tr><td>c</td></tr><div>cross</table>tail</div>"#;
        let converted = convert_with_asciidoctor(&html_to_adoc(html));
        let document = Html::parse_document(&converted);

        assert_eq!(
            document.select(&Selector::parse("table").unwrap()).count(),
            0,
            "{converted}"
        );
        assert_eq!(
            document
                .select(&Selector::parse("div.listingblock").unwrap())
                .count(),
            2,
            "{converted}"
        );
    }

    /// An `include::` line reaching the preprocessor embeds the named file into
    /// the reader's converted archive. The target is created *inside* the
    /// conversion directory, so a regression splices recognizable content into
    /// the output rather than failing silently on a missing path.
    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn include_directives_in_archived_content_read_no_file_after_conversion() {
        let html = concat!(
            "<pre>include::secret.txt[]</pre>",
            "<script>include::secret.txt[]</script>",
            "<style>include::secret.txt[]</style>",
            "<table><tr><td>\ninclude::secret.txt[]\n</td></tr></table>",
            "<section style=\"x\">\ninclude::secret.txt[]\n</section>",
        );
        let (converted, stderr) = convert_with_asciidoctor_beside(
            &html_to_adoc(html),
            &[("secret.txt", "LEAKED-FILE-CONTENTS\n")],
        );

        assert!(
            !converted.contains("LEAKED-FILE-CONTENTS"),
            "the preprocessor read a local file: {converted}"
        );
        assert_eq!(
            converted.matches("include::secret.txt[]").count(),
            5,
            "every directive must survive as inert text: {converted}"
        );
        assert!(
            !converted.contains('\\'),
            "the protective backslash was visible: {converted}"
        );
        assert!(
            stderr.is_empty(),
            "asciidoctor diagnosed the archive: {stderr}"
        );
    }

    /// An unmatched conditional truncates the document at exit status 0 —
    /// everything after it is discarded — and a bare `endif::[]` is dropped from
    /// a listing's own content. Both are silent losses in the path that exists to
    /// preserve content inertly.
    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn conditional_directives_in_archived_content_lose_nothing_after_conversion() {
        let directives = [
            "ifdef::never-defined[]",
            "ifndef::never-defined[]",
            "ifeval::[1==1]",
            "endif::[]",
        ];
        let html = format!(
            "<p>BEFORE-MARKER</p>{}<table><tr><td>\nendif::[]\n</td></tr></table>\
             <p>AFTER-MARKER</p>",
            directives
                .iter()
                .map(|directive| format!("<pre>{directive}</pre>"))
                .collect::<String>(),
        );
        let (converted, stderr) = convert_with_asciidoctor_beside(&html_to_adoc(&html), &[]);
        let visible = Html::parse_document(&converted)
            .root_element()
            .text()
            .collect::<String>();

        assert!(
            visible.contains("AFTER-MARKER"),
            "the document was truncated: {converted}"
        );
        for directive in directives {
            assert!(
                visible.contains(directive),
                "{directive} was dropped from the listing: {converted}"
            );
        }
        assert!(
            !visible.contains('\\'),
            "the protective backslash was visible: {converted}"
        );
        assert!(
            stderr.is_empty(),
            "asciidoctor diagnosed the archive: {stderr}"
        );
    }

    fn convert_with_asciidoctor(adoc: &str) -> String {
        convert_with_asciidoctor_beside(adoc, &[]).0
    }

    /// Converts `adoc` in a temp directory seeded with `files`, returning the
    /// generated HTML and Asciidoctor's own diagnostics. Nothing outside that
    /// directory is readable by the conversion, and a preprocessor complaint is
    /// only ever an `ERROR` line — the process still exits zero — so stderr, not
    /// the exit status, is what proves the document survived intact.
    fn convert_with_asciidoctor_beside(adoc: &str, files: &[(&str, &str)]) -> (String, String) {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("input.adoc"), adoc).unwrap();
        for (name, contents) in files {
            fs::write(dir.path().join(name), contents).unwrap();
        }
        let output = Command::new("asciidoctor")
            .arg("--embedded")
            .arg("input.adoc")
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "asciidoctor failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        (
            fs::read_to_string(dir.path().join("input.html")).unwrap(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// HTML elements a live inline injection would produce. A neutralized
    /// construct renders every one of these vocabularies as inert text instead,
    /// so their *absence* from Asciidoctor's output is the proof of neutralization
    /// that holds uniformly across the whole macro vocabulary.
    fn assert_no_live_inline_construct(converted: &str, context: &str) {
        for live in [
            "<script",
            "<iframe",
            "<img",
            "<a href",
            "<a id",
            "<span class=\"image\"",
            "<span class=\"icon\"",
            "class=\"footnote\"",
            // Math renderers (stem/latexmath/asciimath) emit these delimiters.
            "\\(",
            "\\$",
        ] {
            assert!(
                !converted.contains(live),
                "live inline construct {live:?} survived for {context}: {converted}"
            );
        }
    }

    /// The load-bearing gate: a broad adversarial matrix fed through the real
    /// renderer and a real Asciidoctor, proving the injection-neutralization
    /// *class* is closed rather than the four reproductions that motivated it.
    ///
    /// Covers every builtin inline-macro name × every name-continuation prefix
    /// byte × a `javascript:`, a remote `https:`, and each macro's own natural
    /// target; the targetless math/index family (whose `indexterm` deletes text);
    /// and authored attribute references in prose. Inline anchors `[[id]]` and
    /// angle-xrefs `<<id>>` are a documented residual (see `AGENTS.md` Known
    /// Limitations): they cannot fetch, script, or delete a following construct's
    /// text, which is all this asserts of them.
    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn adversarial_differential_inline_injection_class_is_closed() {
        const PREFIXES: [&str; 6] = ["", "z", "9", "_", ".", "-"];

        // H2 — target-form builtins. Each target would fire live if the escape
        // landed at the token boundary instead of before the name.
        let target_form = [
            ("image", "https://evil.example/beacon.png"),
            ("image", "javascript:alert(1)"),
            ("icon", "home"),
            ("link", "javascript:alert(document.domain)"),
            ("link", "https://evil.example/x"),
            ("xref", "1.adoc"),
            ("xref", "javascript:alert(1)"),
            ("mailto", "evil@evil.example"),
            ("anchor", "anchorid"),
        ];
        for (name, target) in target_form {
            for prefix in PREFIXES {
                let html = format!("<p>{prefix}{name}:{target}[label]</p>");
                let converted = convert_with_asciidoctor(&html_to_adoc(&html));
                assert_no_live_inline_construct(&converted, &format!("H2 {html:?}"));
            }
        }

        // H3 — targetless math/index family. The macro name surviving as literal
        // text proves the construct was neutralized; the marker surviving proves
        // no text was deleted (the `indexterm` content-loss).
        let targetless = [
            "footnote",
            "stem",
            "latexmath",
            "asciimath",
            "indexterm",
            "indexterm2",
        ];
        for name in targetless {
            for prefix in PREFIXES {
                let marker = "SENTINELtext";
                let html = format!("<p>lead {prefix}{name}:[{marker}] tail</p>");
                let converted = convert_with_asciidoctor(&html_to_adoc(&html));
                assert_no_live_inline_construct(&converted, &format!("H3 {html:?}"));
                assert!(
                    converted.contains(&format!("{name}:")),
                    "targetless {name:?} was not neutralized: {converted}"
                );
                assert!(
                    converted.contains(marker),
                    "targetless {name:?} deleted its text: {converted}"
                );
            }
        }

        // H4 (documented residual) — inline anchors and angle-xrefs. They cannot
        // script, fetch, or hijack; assert exactly that, not full inertness.
        for html in [
            "<p>lead [[SENTINELid]] tail</p>",
            "<p>lead [[SENTINELid,ref]] tail</p>",
            "<p>lead &lt;&lt;SENTINELref&gt;&gt; tail</p>",
            "<p>lead &lt;&lt;SENTINELref,Label&gt;&gt; tail</p>",
        ] {
            let converted = convert_with_asciidoctor(&html_to_adoc(html));
            for live in [
                "<script",
                "<iframe",
                "<img",
                "href=\"javascript:",
                "href=\"http",
            ] {
                assert!(
                    !converted.contains(live),
                    "H4 {html:?} produced dangerous {live:?}: {converted}"
                );
            }
        }

        // Authored attribute references in prose. `{set:…}` is the H1 primitive;
        // it, `{counter:…}` and a plain `{attr}` must all survive as literal text
        // (a live reference is consumed) and never open a `javascript:` sink.
        let attr_prose = concat!(
            "<p>{set:lj-archiver-post-link-prefix:javascript:alert(1)//} ",
            "{counter:owned} {plain-attr}</p>"
        );
        let converted = convert_with_asciidoctor(&html_to_adoc(attr_prose));
        for marker in ["{set:", "{counter:", "{plain-attr}"] {
            assert!(
                converted.contains(marker),
                "attribute reference {marker:?} was consumed: {converted}"
            );
        }
        assert!(
            !converted.contains("<script") && !converted.contains("href=\"javascript:"),
            "attribute reference opened a live sink: {converted}"
        );
    }

    /// H1: an authored AsciiDoc attribute reference smuggled into a *macro target*
    /// — where authored prose escaping never reaches it — must not survive to
    /// `{set:}` the post-link prefix and hijack a later localized image link. The
    /// renderer's own `{lj-archiver-post-link-prefix}` (composed outside
    /// `sanitize_macro_target`) must still localize correctly.
    #[test]
    #[ignore = "requires asciidoctor; exercised explicitly by final integration gates"]
    fn adversarial_differential_attribute_reference_in_target_cannot_hijack_localization() {
        let rewriter = LinkRewriter::new(
            "news",
            HashSet::from([166512u64]),
            Url::parse("https://news.livejournal.com/").unwrap(),
        );

        // The poison sets the prefix to a `javascript:` sink from inside a URL
        // fragment, query, and path — the `url` crate leaves `{`/`}` intact in all
        // three. It precedes a same-journal linked image, whose localized
        // `link={lj-archiver-post-link-prefix}166512.adoc` the poison would hijack
        // if the reference were still live when Asciidoctor reached it.
        for sink in [
            "https://x.example/#{set:lj-archiver-post-link-prefix:javascript:alert(1)//}",
            "https://x.example/?q={set:lj-archiver-post-link-prefix:javascript:alert(1)//}",
            "https://x.example/{set:lj-archiver-post-link-prefix:javascript:alert(1)//}/p",
        ] {
            let body_html = format!(
                concat!(
                    r#"<p><a href="{sink}">poison</a></p>"#,
                    r#"<p><a href="https://news.livejournal.com/166512.html">"#,
                    r#"<img src="../assets/p.jpg" alt="pic"></a></p>"#,
                ),
                sink = sink
            );
            let body = html_to_adoc_with_links(&body_html, Some(&rewriter));

            // The renderer never escapes its own generated prefix reference, so it
            // is present verbatim and localization is intact.
            assert!(
                body.contains("link={lj-archiver-post-link-prefix}166512.adoc"),
                "localization was broken by the H1 fix: {body:?}"
            );

            // Mirror the archiver document header that supplies the safe default.
            let doc = format!(
                "= Title\n\nifndef::lj-archiver-post-link-prefix[:lj-archiver-post-link-prefix:]\n\n{body}"
            );
            let converted = convert_with_asciidoctor(&doc);

            assert!(
                !converted.contains("href=\"javascript:"),
                "attribute-reference target hijacked a link ({sink:?}): {converted}"
            );
            // The localized image link resolves to the safe empty-prefix default,
            // not the poisoned `javascript:…` value.
            assert!(
                converted.contains("href=\"166512.adoc\""),
                "localized image link did not resolve safely ({sink:?}): {converted}"
            );
        }
    }
}
