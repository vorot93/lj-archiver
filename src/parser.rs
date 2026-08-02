use chrono::{DateTime, Utc};
use scraper::{ElementRef, Html, Selector, node::Node};
use std::{collections::HashSet, sync::LazyLock};
use url::Url;

use crate::{
    error::{LjError, Result},
    models::{Comment, Post},
};

static ENTRY_TITLE: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".aentry-post__title-text, .entry-title").expect("static selector must parse")
});
static DOCUMENT_TITLE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("title").expect("static selector must parse"));
static ENTRY_DATE: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("time.entry-date[datetime]").expect("static selector must parse")
});
static ENTRY_CONTENT: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".aentry-post__text, .entry-content").expect("static selector must parse")
});
static COMMENT_LIST: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("#comments .comment").expect("static selector must parse"));
static COMMENT_CONTENT: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".comment-content").expect("static selector must parse"));
static COMMENT_AUTHOR: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".comment-author").expect("static selector must parse"));
static COMMENT_SUBJECT: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".comment-subject").expect("static selector must parse"));
static COMMENT_DATE: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".comment-date[datetime]").expect("static selector must parse")
});
static ARTICLE_WITH_JOURNAL_AND_ITEMID: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("article[data-journal][data-itemid]").expect("static selector must parse")
});
static ARTICLE_WITH_ITEMID: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("article[data-itemid]").expect("static selector must parse"));
static ARTICLE_WITH_JOURNAL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("article[data-journal]").expect("static selector must parse"));
static ARTICLE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("article").expect("static selector must parse"));
static ENTRY_TAGS: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".aentry-tags a, .entry-tags a, a[rel=tag]")
        .expect("static selector must parse")
});
static SCRIPT: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("script").expect("static selector must parse"));
static FORM_WITH_ACTION: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("form[action]").expect("static selector must parse"));

#[derive(serde::Deserialize)]
struct SiteEntry {
    #[serde(default)]
    ditemid: Option<u64>,
    #[serde(default)]
    journal: Option<String>,
    #[serde(default)]
    poster: Option<String>,
    #[serde(default)]
    eventtime: Option<i64>,
}

fn site_entry(document: &Html, expected_id: u64) -> Option<SiteEntry> {
    const ANCHOR: &str = "Site.entry";

    for script in document.select(&SCRIPT) {
        let text = script.text().collect::<String>();
        let bytes = text.as_bytes();
        let mut search_start = 0;

        while let Some(relative_anchor) = text[search_start..].find(ANCHOR) {
            let anchor = search_start + relative_anchor;
            let after_anchor = anchor + ANCHOR.len();
            search_start = after_anchor;

            if anchor
                .checked_sub(1)
                .and_then(|index| bytes.get(index))
                .is_some_and(|byte| is_js_identifier_byte(*byte))
                || bytes
                    .get(after_anchor)
                    .is_some_and(|byte| is_js_identifier_byte(*byte))
            {
                continue;
            }

            let mut cursor = after_anchor;
            while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
                cursor += 1;
            }
            if bytes.get(cursor) != Some(&b'=') {
                continue;
            }

            cursor += 1;
            while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
                cursor += 1;
            }
            if bytes.get(cursor) != Some(&b'{') {
                continue;
            }

            let entry = serde_json::Deserializer::from_str(&text[cursor..])
                .into_iter::<SiteEntry>()
                .next()
                .and_then(std::result::Result::ok);
            if entry.as_ref().and_then(|entry| entry.ditemid) == Some(expected_id) {
                return entry;
            }
        }
    }

    None
}

fn is_js_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn is_age_gate(document: &Html, page_url: &Url) -> bool {
    document.select(&FORM_WITH_ACTION).any(|form| {
        form.attr("action")
            .and_then(|action| page_url.join(action).ok())
            .is_some_and(|action| action.path().ends_with("/misc/adult_explicit.bml"))
    })
}

pub fn parse_post_page(html: &str, url: &Url) -> Result<Post> {
    let document = Html::parse_document(html);
    if is_age_gate(&document, url) {
        return Err(LjError::AgeGated {
            url: url.to_string(),
        });
    }
    let article = source_article(&document);

    let id = article
        .and_then(|article| non_empty_attr(article, "data-itemid"))
        .map(|id| parse_id(&id))
        .transpose()
        .map_err(|_| parse_error(url, "invalid data-itemid"))?
        .or_else(|| id_from_url(url))
        .ok_or_else(|| parse_error(url, "missing post id"))?;

    let entry = site_entry(&document, id);

    let journal = entry
        .as_ref()
        .and_then(|entry| entry.journal.as_deref())
        .map(str::trim)
        .filter(|journal| !journal.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| article.and_then(|article| non_empty_attr(article, "data-journal")))
        .or_else(|| journal_from_url(url))
        .ok_or_else(|| parse_error(url, "missing journal"))?;

    let title = article
        .and_then(|article| first_text_in(article, &ENTRY_TITLE))
        .or_else(|| first_text(&document, &DOCUMENT_TITLE))
        .unwrap_or_else(|| format!("LiveJournal post {id}"));
    let author = journal.clone();

    let poster = entry
        .as_ref()
        .and_then(|entry| entry.poster.as_deref())
        .map(str::trim)
        .filter(|poster| !poster.is_empty())
        .map(ToOwned::to_owned);

    let published_at = match entry.as_ref().and_then(|entry| entry.eventtime) {
        Some(seconds) => DateTime::from_timestamp(seconds, 0),
        None => article
            .map(|article| first_datetime_in_result(article, &ENTRY_DATE, url))
            .transpose()?
            .flatten(),
    };
    let body = match article {
        Some(article) => article.select(&ENTRY_CONTENT).next(),
        None => document.select(&ENTRY_CONTENT).next(),
    }
    .ok_or_else(|| parse_error(url, "missing post body"))?;
    let body_html = inner_html(body);

    if body_html.trim().is_empty() {
        return Err(parse_error(url, "missing post body"));
    }

    Ok(Post {
        journal,
        id,
        url: url.clone(),
        title,
        author,
        poster,
        published_at,
        tags: article.map(tags_in).unwrap_or_else(|| tags(&document)),
        body_html,
        comments: comments(&document),
    })
}

fn comments(document: &Html) -> Vec<Comment> {
    document
        .select(&COMMENT_LIST)
        .map(|comment| {
            let body_html = comment
                .select(&COMMENT_CONTENT)
                .next()
                .map(inner_html)
                .unwrap_or_default();
            let body_text = comment
                .select(&COMMENT_CONTENT)
                .next()
                .map(text_content)
                .unwrap_or_default();

            Comment {
                id: non_empty_attr(comment, "id"),
                parent_id: non_empty_attr(comment, "data-parent"),
                depth: non_empty_attr(comment, "data-depth")
                    .and_then(|depth| depth.parse::<usize>().ok())
                    .unwrap_or_default(),
                author: first_text_in(comment, &COMMENT_AUTHOR)
                    .unwrap_or_else(|| "anonymous".to_string()),
                subject: first_text_in(comment, &COMMENT_SUBJECT),
                posted_at: first_datetime_in(comment, &COMMENT_DATE),
                body_html,
                deleted: has_class(comment, "deleted") || body_text.contains("Deleted comment"),
            }
        })
        .collect()
}

fn source_article(document: &Html) -> Option<ElementRef<'_>> {
    document
        .select(&ARTICLE_WITH_JOURNAL_AND_ITEMID)
        .next()
        .or_else(|| document.select(&ARTICLE_WITH_ITEMID).next())
        .or_else(|| document.select(&ARTICLE_WITH_JOURNAL).next())
        .or_else(|| document.select(&ARTICLE).next())
}

fn tags(document: &Html) -> Vec<String> {
    collect_tags(document.select(&ENTRY_TAGS))
}

fn tags_in(element: ElementRef<'_>) -> Vec<String> {
    collect_tags(element.select(&ENTRY_TAGS))
}

fn collect_tags<'a>(elements: impl Iterator<Item = ElementRef<'a>>) -> Vec<String> {
    let mut tags = Vec::new();
    let mut seen = HashSet::new();

    for tag in elements {
        let tag = text_content(tag);
        if !tag.is_empty() && seen.insert(tag.clone()) {
            tags.push(tag);
        }
    }

    tags
}

fn first_text(document: &Html, selector: &Selector) -> Option<String> {
    document
        .select(selector)
        .map(text_content)
        .find(|text| !text.is_empty())
}

fn first_text_in(element: ElementRef<'_>, selector: &Selector) -> Option<String> {
    element
        .select(selector)
        .map(text_content)
        .find(|text| !text.is_empty())
}

fn first_datetime_in_result(
    element: ElementRef<'_>,
    selector: &Selector,
    url: &Url,
) -> Result<Option<DateTime<Utc>>> {
    match element
        .select(selector)
        .find_map(|element| non_empty_attr(element, "datetime"))
    {
        Some(value) => parse_datetime(&value)
            .map(Some)
            .map_err(|_| parse_error(url, "invalid post datetime")),
        None => Ok(None),
    }
}

fn first_datetime_in(element: ElementRef<'_>, selector: &Selector) -> Option<DateTime<Utc>> {
    element
        .select(selector)
        .find_map(|element| non_empty_attr(element, "datetime"))
        .and_then(|value| parse_datetime(&value).ok())
}

fn parse_datetime(value: &str) -> std::result::Result<DateTime<Utc>, chrono::ParseError> {
    DateTime::parse_from_rfc3339(value).map(|datetime| datetime.with_timezone(&Utc))
}

fn inner_html(element: ElementRef<'_>) -> String {
    element
        .children()
        .map(|child| match child.value() {
            Node::Text(text) => escape_html_text(text),
            Node::Element(_) => ElementRef::wrap(child)
                .map(|element| element.html())
                .unwrap_or_default(),
            _ => String::new(),
        })
        .collect()
}

fn escape_html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn text_content(element: ElementRef<'_>) -> String {
    element
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn non_empty_attr(element: ElementRef<'_>, name: &str) -> Option<String> {
    element
        .attr(name)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn journal_from_url(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    if host == "users.livejournal.com" {
        return url
            .path_segments()?
            .find(|segment| !segment.is_empty())
            .map(ToOwned::to_owned);
    }

    host.strip_suffix(".livejournal.com")
        .filter(|journal| !journal.is_empty() && *journal != "www")
        .map(ToOwned::to_owned)
}

fn id_from_url(url: &Url) -> Option<u64> {
    let segment = url.path_segments()?.next_back()?;
    let id = segment.strip_suffix(".html").unwrap_or(segment);
    parse_id(id).ok()
}

fn parse_id(value: &str) -> std::result::Result<u64, std::num::ParseIntError> {
    value.parse::<u64>()
}

fn has_class(element: ElementRef<'_>, class_name: &str) -> bool {
    element
        .attr("class")
        .is_some_and(|classes| classes.split_whitespace().any(|class| class == class_name))
}

fn parse_error(url: &Url, reason: impl Into<String>) -> LjError {
    LjError::Parse {
        url: url.to_string(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_post() {
        let html = include_str!("../tests/fixtures/post_basic.html");
        let url = Url::parse("https://news.livejournal.com/166511.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.journal, "news");
        assert_eq!(post.id, 166511);
        assert_eq!(post.title, "Post Title");
        assert_eq!(post.author, "news");
        assert_eq!(post.tags, vec!["updates", "release-notes"]);
        assert!(post.body_html.contains("<strong>world</strong>"));
        assert!(post.comments.is_empty());
        assert!(post.poster.is_none());
        assert_eq!(
            post.published_at,
            Some(
                DateTime::parse_from_rfc3339("2024-12-17T16:01:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
    }

    #[test]
    fn parses_real_livejournal_aentry_markup() {
        // Real LiveJournal post markup (as served at news.livejournal.com/166511.html):
        // `<article class="aentry-post">` with `.aentry-post__title-text` and
        // `.aentry-post__text` — not the synthetic `.entry-title`/`.entry-content`.
        let html = include_str!("../tests/fixtures/post_real.html");
        let url = Url::parse("https://news.livejournal.com/166511.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.journal, "news");
        assert_eq!(post.id, 166511);
        assert_eq!(
            post.title,
            "Join in and create your own personal calendar for 2026! 🌟🎄📅"
        );
        assert_eq!(post.author, "news");
        assert_eq!(post.poster.as_deref(), Some("livejournal"));
        assert_eq!(post.published_at, DateTime::from_timestamp(1765987260, 0));
        assert!(post.body_html.contains("Dear friends"));
        assert!(post.body_html.contains("create your own calendar"));
        assert!(post.body_html.contains("aentry-post__figure"));
        assert_eq!(
            post.tags,
            vec!["#mycalendar2026", "christmas", "new year", "photos"]
        );
        // Comments are JS-loaded on modern LiveJournal and absent from static
        // HTML, so they degrade to empty rather than failing the archive.
        assert!(post.comments.is_empty());
    }

    #[test]
    fn parses_threaded_comments() {
        let html = include_str!("../tests/fixtures/post_with_comments.html");
        let url = Url::parse("https://news.livejournal.com/166512.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.comments.len(), 3);
        assert_eq!(post.comments[0].id.as_deref(), Some("t1"));
        assert_eq!(post.comments[0].depth, 0);
        assert_eq!(post.comments[1].parent_id.as_deref(), Some("t1"));
        assert_eq!(post.comments[1].depth, 1);
        assert!(post.comments[2].deleted);
    }

    #[test]
    fn preserves_entities_in_post_body_html() {
        let html = r#"
            <html>
                <body>
                    <article data-journal="news" data-itemid="166512">
                        <div class="entry-content">A &lt; B &amp; C</div>
                    </article>
                </body>
            </html>
        "#;
        let url = Url::parse("https://news.livejournal.com/166512.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert!(post.body_html.contains("A &lt; B &amp; C"));
        assert!(!post.body_html.contains("A < B & C"));
    }

    #[test]
    fn preserves_entities_in_comment_body_html() {
        let html = r#"
            <html>
                <body>
                    <article data-journal="news" data-itemid="166512">
                        <div class="entry-content"><p>body</p></div>
                    </article>
                    <div id="comments">
                        <div class="comment" id="t1">
                            <span class="comment-author">reader</span>
                            <div class="comment-content">A &lt; B &amp; C</div>
                        </div>
                    </div>
                </body>
            </html>
        "#;
        let url = Url::parse("https://news.livejournal.com/166512.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.comments.len(), 1);
        assert!(post.comments[0].body_html.contains("A &lt; B &amp; C"));
        assert!(!post.comments[0].body_html.contains("A < B & C"));
    }

    #[test]
    fn parses_post_from_article_with_source_attrs() {
        let html = r#"
            <html>
                <body>
                    <article>
                        <div class="entry-content"><p>wrong</p></div>
                    </article>
                    <article data-journal="news" data-itemid="166513">
                        <div class="entry-content"><p>right</p></div>
                    </article>
                </body>
            </html>
        "#;
        let url = Url::parse("https://example.com/post.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.journal, "news");
        assert_eq!(post.id, 166513);
        assert!(post.body_html.contains("<p>right</p>"));
        assert!(!post.body_html.contains("<p>wrong</p>"));
    }

    #[test]
    fn uses_single_source_article_for_attrs_and_body() {
        let html = r#"
            <html>
                <body>
                    <article data-journal="wrong">
                        <div class="entry-content"><p>wrong journal only</p></div>
                    </article>
                    <article data-itemid="999">
                        <div class="entry-content"><p>wrong id only</p></div>
                    </article>
                    <article data-journal="news" data-itemid="166514">
                        <div class="entry-content"><p>right</p></div>
                    </article>
                </body>
            </html>
        "#;
        let url = Url::parse("https://example.com/post.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.journal, "news");
        assert_eq!(post.id, 166514);
        assert!(post.body_html.contains("<p>right</p>"));
        assert!(!post.body_html.contains("wrong journal only"));
        assert!(!post.body_html.contains("wrong id only"));
    }

    #[test]
    fn uses_selected_article_for_post_metadata() {
        let html = r#"
            <html>
                <head>
                    <title>Document fallback title</title>
                </head>
                <body>
                    <article>
                        <h1 class="entry-title">Wrong title</h1>
                        <span class="entry-author">wrong-author</span>
                        <time class="entry-date" datetime="2001-01-01T00:00:00Z">old</time>
                        <a rel="tag">wrong-tag</a>
                        <div class="entry-content"><p>wrong body</p></div>
                    </article>
                    <article data-journal="news" data-itemid="166515">
                        <h1 class="entry-title">Right title</h1>
                        <span class="entry-author">right-author</span>
                        <time class="entry-date" datetime="2026-07-02T12:34:56Z">new</time>
                        <a rel="tag">alpha</a>
                        <div class="entry-tags">
                            <a href="/tags/beta">beta</a>
                            <a rel="tag">alpha</a>
                        </div>
                        <div class="entry-content"><p>right body</p></div>
                    </article>
                </body>
            </html>
        "#;
        let url = Url::parse("https://example.com/post.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.journal, "news");
        assert_eq!(post.id, 166515);
        assert_eq!(post.title, "Right title");
        assert_eq!(post.author, "news");
        assert_eq!(
            post.published_at,
            Some(
                DateTime::parse_from_rfc3339("2026-07-02T12:34:56Z")
                    .unwrap()
                    .into()
            )
        );
        assert_eq!(post.tags, vec!["alpha", "beta"]);
        assert!(post.body_html.contains("<p>right body</p>"));
        assert!(!post.body_html.contains("wrong body"));
    }

    #[test]
    fn parses_tags_in_document_order_without_duplicates() {
        let html = r#"
            <html>
                <body>
                    <article data-journal="news" data-itemid="166514">
                        <a rel="tag">alpha</a>
                        <div class="entry-tags">
                            <a href="/tags/beta">beta</a>
                            <a href="/tags/alpha">alpha</a>
                        </div>
                        <a rel="tag">gamma</a>
                        <div class="entry-tags">
                            <a href="/tags/delta">delta</a>
                        </div>
                        <div class="entry-content"><p>body</p></div>
                    </article>
                </body>
            </html>
        "#;
        let url = Url::parse("https://news.livejournal.com/166514.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.tags, vec!["alpha", "beta", "gamma", "delta"]);
    }

    #[test]
    fn extracts_poster_and_date_from_site_entry() {
        let html = r#"
            <html><body>
                <article data-journal="news" data-itemid="166511">
                    <div class="entry-content"><p>body</p></div>
                </article>
                <script>
                Site.entry = {"ditemid":166511,"poster":"livejournal","poster_display_name":"livejournal","journal":"news","eventtime":1765987260};
                </script>
            </body></html>
        "#;
        let url = Url::parse("https://news.livejournal.com/166511.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.author, "news");
        assert_eq!(post.poster.as_deref(), Some("livejournal"));
        assert_eq!(post.published_at, DateTime::from_timestamp(1765987260, 0));
    }

    #[test]
    fn body_site_entry_prose_does_not_shadow_real_script_assignment() {
        let html = r#"
            <html><body>
                <article data-journal="news" data-itemid="123">
                    <div class="entry-content">
                        <p>Site.entry {"ditemid":999,"poster":"fake"}</p>
                    </div>
                </article>
                <script>
                Site.entry = {"ditemid":123,"journal":"news","poster":"real_poster","eventtime":1700000000};
                </script>
            </body></html>
        "#;
        let url = Url::parse("https://news.livejournal.com/123.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.journal, "news");
        assert_eq!(post.author, "news");
        assert_eq!(post.poster.as_deref(), Some("real_poster"));
        assert_eq!(post.published_at, DateTime::from_timestamp(1700000000, 0));
    }

    #[test]
    fn site_entry_scans_past_malformed_and_unrelated_assignments() {
        let html = r#"
            <html><body>
                <article data-journal="fallback" data-itemid="123">
                    <div class="entry-content"><p>body</p></div>
                </article>
                <script>
                NotSite.entry = {"ditemid":1,"journal":"wrong","poster":"wrong"};
                Site.entryExtra = {"ditemid":2,"journal":"wrong","poster":"wrong"};
                Site.entry = {"ditemid":
                Site.entry = {"ditemid":123,"journal":"news","poster":"winner","eventtime":1700000000};
                </script>
            </body></html>
        "#;
        let url = Url::parse("https://news.livejournal.com/123.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.journal, "news");
        assert_eq!(post.author, "news");
        assert_eq!(post.poster.as_deref(), Some("winner"));
        assert_eq!(post.published_at, DateTime::from_timestamp(1700000000, 0));
    }

    #[test]
    fn canonical_journal_is_not_overridden_by_legacy_author() {
        let html = r#"
            <html><body>
                <article data-journal="community" data-itemid="1">
                    <span class="entry-author">alice</span>
                    <div class="entry-content"><p>body</p></div>
                </article>
                <script>
                Site.entry = {"ditemid":1,"journal":"community","poster":"alice"};
                </script>
            </body></html>
        "#;
        let url = Url::parse("https://community.livejournal.com/1.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.journal, "community");
        assert_eq!(post.author, "community");
        assert_eq!(post.poster.as_deref(), Some("alice"));
    }

    #[test]
    fn ignores_site_entry_when_ditemid_mismatches() {
        let html = r#"
            <html><body>
                <article data-journal="news" data-itemid="166511">
                    <div class="entry-content"><p>body</p></div>
                </article>
                <script>Site.entry = {"ditemid":999999,"poster":"livejournal","eventtime":1765987260};</script>
            </body></html>
        "#;
        let url = Url::parse("https://news.livejournal.com/166511.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert!(post.poster.is_none());
        assert!(post.published_at.is_none());
    }

    #[test]
    fn missing_site_entry_leaves_poster_and_date_absent() {
        let html = r#"
            <html><body>
                <article data-journal="news" data-itemid="166511">
                    <div class="entry-content"><p>body</p></div>
                </article>
            </body></html>
        "#;
        let url = Url::parse("https://news.livejournal.com/166511.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert!(post.poster.is_none());
        assert!(post.published_at.is_none());
    }

    #[test]
    fn malformed_site_entry_json_degrades_gracefully() {
        let html = r#"
            <html><body>
                <article data-journal="news" data-itemid="166511">
                    <div class="entry-content"><p>body</p></div>
                </article>
                <script>Site.entry = {"ditemid":166511,"poster": };</script>
            </body></html>
        "#;
        let url = Url::parse("https://news.livejournal.com/166511.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert!(post.poster.is_none());
        assert!(post.published_at.is_none());
    }

    #[test]
    fn parses_site_entry_with_braces_and_quotes_in_ignored_fields() {
        let html = r#"
            <html><body>
                <article data-journal="news" data-itemid="166511">
                    <div class="entry-content"><p>body</p></div>
                </article>
                <script>Site.entry = {"ditemid":166511,"title":"weird } title \" ok","poster":"livejournal","eventtime":1765987260};</script>
            </body></html>
        "#;
        let url = Url::parse("https://news.livejournal.com/166511.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.poster.as_deref(), Some("livejournal"));
        assert_eq!(post.published_at, DateTime::from_timestamp(1765987260, 0));
    }

    #[test]
    fn poster_may_equal_journal_for_personal_journals() {
        let html = r#"
            <html><body>
                <article data-journal="alice" data-itemid="500">
                    <div class="entry-content"><p>body</p></div>
                </article>
                <script>Site.entry = {"ditemid":500,"poster":"alice","eventtime":1700000000};</script>
            </body></html>
        "#;
        let url = Url::parse("https://alice.livejournal.com/500.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.author, "alice");
        assert_eq!(post.poster.as_deref(), Some("alice"));
        assert_eq!(post.published_at, DateTime::from_timestamp(1700000000, 0));
    }

    #[test]
    fn extracts_site_entry_for_users_livejournal_scheme() {
        // Underscore journals live at users.livejournal.com/<journal>/<id>.html;
        // the post id comes from the URL's last path segment.
        let html = r#"
            <html><body>
                <article data-journal="a_b">
                    <div class="entry-content"><p>body</p></div>
                </article>
                <script>Site.entry = {"ditemid":500,"poster":"a_b","eventtime":1700000000};</script>
            </body></html>
        "#;
        let url = Url::parse("https://users.livejournal.com/a_b/500.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.id, 500);
        assert_eq!(post.poster.as_deref(), Some("a_b"));
        assert_eq!(post.published_at, DateTime::from_timestamp(1700000000, 0));
    }

    #[test]
    fn users_host_fallback_uses_first_path_segment() {
        let html = r#"
            <html><body>
                <article data-itemid="123">
                    <div class="entry-content"><p>body</p></div>
                </article>
            </body></html>
        "#;
        let url = Url::parse("https://users.livejournal.com/evo_lutio/123.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.journal, "evo_lutio");
        assert_eq!(post.author, "evo_lutio");
    }

    #[test]
    fn resolves_journal_from_site_entry_for_users_scheme() {
        // Site.entry.journal remains canonical when the article carries no
        // data-journal on the users.livejournal.com URL scheme.
        let html = r#"
            <html><body>
                <script>Site.entry = {"ditemid":123,"journal":"some_journal","poster":"some_journal"};</script>
                <article><div class="entry-content"><p>body</p></div></article>
            </body></html>
        "#;
        let url = Url::parse("https://users.livejournal.com/some_journal/123.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.journal, "some_journal");
        assert_eq!(post.author, "some_journal");
    }

    #[test]
    fn prefers_canonical_site_entry_journal_over_subdomain_spelling() {
        let html = r#"
            <html><body>
                <script>Site.entry = {"ditemid":240331,"journal":"evo_lutio","poster":"evo_lutio"};</script>
                <article><div class="entry-content"><p>body</p></div></article>
            </body></html>
        "#;
        let url = Url::parse("https://evo-lutio.livejournal.com/240331.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.journal, "evo_lutio");
        assert_eq!(post.author, "evo_lutio");
    }

    #[test]
    fn ignores_site_entry_journal_when_ditemid_mismatches() {
        let html = r#"
            <html><body>
                <script>Site.entry = {"ditemid":999,"journal":"wrong_journal"};</script>
                <article data-journal="right_journal" data-itemid="123">
                    <div class="entry-content"><p>body</p></div>
                </article>
            </body></html>
        "#;
        let url = Url::parse("https://right-journal.livejournal.com/123.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.journal, "right_journal");
    }

    #[test]
    fn age_gate_marker_in_prose_or_link_does_not_reject_post() {
        let html = r#"
            <html><body>
                <article data-journal="news" data-itemid="123">
                    <div class="entry-content">
                        <p>The path /misc/adult_explicit.bml is discussed here.</p>
                        <a href="/misc/adult_explicit.bml">ordinary link</a>
                    </div>
                </article>
            </body></html>
        "#;
        let url = Url::parse("https://news.livejournal.com/123.html").unwrap();
        let post = parse_post_page(html, &url).unwrap();

        assert_eq!(post.id, 123);
        assert!(!post.body_html.trim().is_empty());
    }

    #[test]
    fn structural_age_gate_form_is_rejected() {
        let html = include_str!("../tests/fixtures/post_age_gated.html");
        let url = Url::parse("https://evo-lutio.livejournal.com/47545.html").unwrap();
        let err = parse_post_page(html, &url).unwrap_err();
        match err {
            LjError::AgeGated { url } => {
                assert_eq!(url, "https://evo-lutio.livejournal.com/47545.html");
            }
            other => panic!("got {other:?}"),
        }

        let relative_form = r#"<form action="/misc/adult_explicit.bml"></form>"#;
        let err = parse_post_page(relative_form, &url).unwrap_err();
        match err {
            LjError::AgeGated { url } => {
                assert_eq!(url, "https://evo-lutio.livejournal.com/47545.html");
            }
            other => panic!("got {other:?}"),
        }
    }
}
