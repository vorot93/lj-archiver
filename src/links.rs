use crate::{
    Result,
    adoc::sanitize_macro_target,
    target::{journals_alias, post_ref_from_url, post_url},
};
use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};
use url::Url;

pub(crate) const POST_LINK_PREFIX_ATTRIBUTE: &str = "lj-archiver-post-link-prefix";
pub(crate) const POST_LINK_PREFIX_REFERENCE: &str = "{lj-archiver-post-link-prefix}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingLink {
    pub token: String,
    pub target_id: u64,
    pub local_target: String,
    pub external_target: String,
}

#[derive(Clone, Copy)]
enum LocalTargetContext {
    XrefMacro,
    ImageLinkAttribute,
}

enum RewriteMode {
    Direct,
    Pending,
}

pub struct LinkRewriter {
    journal: String,
    ids: HashSet<u64>,
    base_url: Url,
    mode: RewriteMode,
    pending_links: Mutex<Vec<PendingLink>>,
}

impl LinkRewriter {
    pub fn new(journal: impl Into<String>, ids: HashSet<u64>, base_url: Url) -> Self {
        Self {
            journal: journal.into(),
            ids,
            base_url,
            mode: RewriteMode::Direct,
            pending_links: Mutex::new(Vec::new()),
        }
    }

    pub fn pending(journal: impl Into<String>, ids: HashSet<u64>, base_url: Url) -> Self {
        Self {
            journal: journal.into(),
            ids,
            base_url,
            mode: RewriteMode::Pending,
            pending_links: Mutex::new(Vec::new()),
        }
    }

    /// Returns a complete AsciiDoc macro target when `href` resolves to a post
    /// of this journal that is in the export's id set; otherwise `None`.
    pub fn xref_target(&self, href: &str) -> Option<String> {
        self.local_post_target(href, LocalTargetContext::XrefMacro)
    }

    /// Returns a URI/path suitable for an image macro's `link=` attribute when
    /// `href` resolves to an exported post of this journal.
    pub fn image_link_target(&self, href: &str) -> Option<String> {
        self.local_post_target(href, LocalTargetContext::ImageLinkAttribute)
    }

    fn local_post_target(&self, href: &str, context: LocalTargetContext) -> Option<String> {
        let url = self.resolved_http_url(href)?;
        let (journal, id) = post_ref_from_url(&url)?;
        if !journals_alias(&journal, &self.journal) || !self.ids.contains(&id) {
            return None;
        }

        let local_target = match context {
            LocalTargetContext::XrefMacro => format!("xref:{id}.adoc"),
            LocalTargetContext::ImageLinkAttribute => {
                format!("{POST_LINK_PREFIX_REFERENCE}{id}.adoc")
            }
        };

        match self.mode {
            RewriteMode::Direct => Some(local_target),
            RewriteMode::Pending => {
                let mut links = self
                    .pending_links
                    .lock()
                    .expect("pending link recorder mutex poisoned");
                let token = format!("\u{1f}lj-archiver-link:{id}:{}\u{1f}", links.len());
                links.push(PendingLink {
                    token: token.clone(),
                    target_id: id,
                    local_target,
                    external_target: pending_external_target(&url, context),
                });
                Some(token)
            }
        }
    }

    /// Resolves `href` against this delivered page and returns a sanitized
    /// generated macro target only for hosted HTTP(S) URLs.
    pub fn absolute_target(&self, href: &str) -> Option<String> {
        let url = self.resolved_http_url(href)?;
        Some(sanitize_macro_target(url.as_str()))
    }

    fn resolved_http_url(&self, href: &str) -> Option<Url> {
        let url = self.base_url.join(href).ok()?;
        (matches!(url.scheme(), "http" | "https") && url.has_host()).then_some(url)
    }

    pub(crate) fn pending_links(&self) -> Vec<PendingLink> {
        self.pending_links
            .lock()
            .expect("pending link recorder mutex poisoned")
            .clone()
    }
}

fn pending_external_target(url: &Url, context: LocalTargetContext) -> String {
    let target = sanitize_macro_target(url.as_str());
    match context {
        LocalTargetContext::XrefMacro => target,
        LocalTargetContext::ImageLinkAttribute => target.replace(',', "%2C"),
    }
}

pub(crate) fn resolve_pending_links(
    text: &str,
    links: &[PendingLink],
    local_ids: &HashSet<u64>,
) -> String {
    resolve_pending_links_with_scan_stats(text, links, local_ids).0
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PendingLinkScanStats {
    candidates: usize,
    cursor_advanced_bytes: usize,
    accounted_input_bytes: usize,
    copied_input_bytes: usize,
    candidate_input_bytes: usize,
    revisited_input_bytes: usize,
    skipped_input_bytes: usize,
    accounted_through: usize,
}

impl PendingLinkScanStats {
    fn account_input_range(&mut self, start: usize, end: usize, candidate: bool) {
        if start < self.accounted_through {
            self.revisited_input_bytes += end.min(self.accounted_through) - start;
        } else if start > self.accounted_through {
            self.skipped_input_bytes += start - self.accounted_through;
        }

        let bytes = end - start;
        self.accounted_input_bytes += bytes;
        if candidate {
            self.candidate_input_bytes += bytes;
        } else {
            self.copied_input_bytes += bytes;
        }
        self.accounted_through = self.accounted_through.max(end);
    }
}

fn resolve_pending_links_with_scan_stats(
    text: &str,
    links: &[PendingLink],
    local_ids: &HashSet<u64>,
) -> (String, PendingLinkScanStats) {
    const SEPARATOR: char = '\u{1f}';

    let mut replacements = HashMap::with_capacity(links.len());
    for link in links {
        let target = if local_ids.contains(&link.target_id) {
            link.local_target.as_str()
        } else {
            link.external_target.as_str()
        };
        replacements.entry(link.token.as_str()).or_insert(target);
    }
    let mut resolved = String::with_capacity(text.len());
    let mut stats = PendingLinkScanStats::default();
    let mut cursor = 0;

    while let Some(relative_start) = text[cursor..].find(SEPARATOR) {
        let start = cursor + relative_start;
        resolved.push_str(&text[cursor..start]);
        stats.account_input_range(cursor, start, false);

        let terminator_search_start = start + SEPARATOR.len_utf8();
        let Some(relative_end) = text[terminator_search_start..].find(SEPARATOR) else {
            resolved.push_str(&text[start..]);
            stats.account_input_range(start, text.len(), false);
            stats.cursor_advanced_bytes += text.len() - cursor;
            cursor = text.len();
            break;
        };
        let end = terminator_search_start + relative_end + SEPARATOR.len_utf8();
        let candidate = &text[start..end];
        if let Some(target) = replacements.get(candidate) {
            resolved.push_str(target);
        } else {
            resolved.push_str(candidate);
        }
        stats.account_input_range(start, end, true);
        stats.candidates += 1;
        stats.cursor_advanced_bytes += end - cursor;
        cursor = end;
    }

    if cursor < text.len() {
        resolved.push_str(&text[cursor..]);
        stats.account_input_range(cursor, text.len(), false);
        stats.cursor_advanced_bytes += text.len() - cursor;
    }

    (resolved, stats)
}

pub(crate) fn restore_missing_xrefs(
    text: &str,
    journal: &str,
    final_ids: &HashSet<u64>,
) -> Result<String> {
    const PREFIX: &str = "xref:";
    const SUFFIX: &str = ".adoc[";

    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;

    while let Some(relative_start) = text[cursor..].find(PREFIX) {
        let start = cursor + relative_start;
        let id_start = start + PREFIX.len();
        let id_end = text[id_start..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count()
            + id_start;

        let id = text[id_start..id_end].parse::<u64>().ok();
        let is_active = start == 0 || text.as_bytes()[start - 1] != b'\\';
        let is_post_xref = id_end > id_start && text[id_end..].starts_with(SUFFIX);

        if is_active
            && is_post_xref
            && let Some(id) = id
            && id > 0
            && !final_ids.contains(&id)
        {
            output.push_str(&text[cursor..start]);
            output.push_str(&sanitize_macro_target(post_url(journal, id)?.as_str()));
            cursor = id_end + ".adoc".len();
        } else {
            output.push_str(&text[cursor..id_start]);
            cursor = id_start;
        }
    }

    output.push_str(&text[cursor..]);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_link(
        token: impl Into<String>,
        target_id: u64,
        local_target: impl Into<String>,
        external_target: impl Into<String>,
    ) -> PendingLink {
        PendingLink {
            token: token.into(),
            target_id,
            local_target: local_target.into(),
            external_target: external_target.into(),
        }
    }

    fn rewriter() -> LinkRewriter {
        LinkRewriter::new(
            "news",
            HashSet::from([166512u64, 166513]),
            Url::parse("https://news.livejournal.com/").unwrap(),
        )
    }

    #[test]
    fn rewrites_absolute_relative_and_users_forms() {
        let r = rewriter();
        assert_eq!(
            r.xref_target("https://news.livejournal.com/166512.html"),
            Some("xref:166512.adoc".to_string())
        );
        assert_eq!(
            r.xref_target("/166513.html"),
            Some("xref:166513.adoc".to_string())
        );

        let users = LinkRewriter::new(
            "example_user",
            HashSet::from([166512u64]),
            Url::parse("https://users.livejournal.com/example_user/").unwrap(),
        );
        assert_eq!(
            users.xref_target("https://users.livejournal.com/example_user/166512.html"),
            Some("xref:166512.adoc".to_string())
        );
    }

    #[test]
    fn image_link_context_prefix_is_separate_from_ordinary_xref_targets() {
        let r = rewriter();
        let href = "https://news.livejournal.com/166512.html";

        assert_eq!(r.xref_target(href), Some("xref:166512.adoc".to_string()));
        assert_eq!(
            r.image_link_target(href),
            Some("{lj-archiver-post-link-prefix}166512.adoc".to_string())
        );
    }

    #[test]
    fn skips_other_journal_out_of_set_and_non_post() {
        let r = rewriter();
        assert_eq!(
            r.xref_target("https://other.livejournal.com/166512.html"),
            None
        );
        assert_eq!(
            r.xref_target("https://news.livejournal.com/999999.html"),
            None
        );
        assert_eq!(r.xref_target("https://news.livejournal.com/tag/x"), None);
        assert_eq!(r.xref_target("https://example.com/"), None);
    }

    #[test]
    fn resolves_only_hosted_http_targets_against_delivered_page() {
        let r = LinkRewriter::new(
            "news",
            HashSet::new(),
            Url::parse("https://news.livejournal.com/archive/2026/1.html").unwrap(),
        );

        for (href, expected) in [
            ("/profile", "https://news.livejournal.com/profile"),
            (
                "next.html",
                "https://news.livejournal.com/archive/2026/next.html",
            ),
            ("//cdn.example.test/x", "https://cdn.example.test/x"),
            (
                "http://outside.example.test/y",
                "http://outside.example.test/y",
            ),
        ] {
            assert_eq!(r.absolute_target(href).as_deref(), Some(expected), "{href}");
        }

        for href in [
            "javascript:alert(1)",
            "data:text/plain,boom",
            "mailto:reader@example.test",
            "file:///tmp/secret",
            "ftp://files.example.test/x",
            "http://",
            "http://[::1",
        ] {
            assert_eq!(r.absolute_target(href), None, "{href}");
        }
    }

    #[test]
    fn ignores_query_and_fragment() {
        assert_eq!(
            rewriter().xref_target("https://news.livejournal.com/166512.html?thread=5#c1"),
            Some("xref:166512.adoc".to_string())
        );
    }

    #[test]
    fn matches_hyphen_underscore_journal_aliases() {
        // underscore-form export, hyphen-subdomain body link
        let r = LinkRewriter::new(
            "evo_lutio",
            HashSet::from([123u64]),
            Url::parse("https://users.livejournal.com/evo_lutio/").unwrap(),
        );
        assert_eq!(
            r.xref_target("https://evo-lutio.livejournal.com/123.html"),
            Some("xref:123.adoc".to_string())
        );
        // hyphen-form export, users-form body link
        let r2 = LinkRewriter::new(
            "evo-lutio",
            HashSet::from([123u64]),
            Url::parse("https://evo-lutio.livejournal.com/").unwrap(),
        );
        assert_eq!(
            r2.xref_target("https://users.livejournal.com/evo_lutio/123.html"),
            Some("xref:123.adoc".to_string())
        );
    }

    #[test]
    fn matches_journal_case_insensitively() {
        let r = LinkRewriter::new(
            "News",
            HashSet::from([1u64]),
            Url::parse("https://news.livejournal.com/").unwrap(),
        );
        assert_eq!(
            r.xref_target("https://news.livejournal.com/1.html"),
            Some("xref:1.adoc".to_string())
        );
    }

    #[test]
    fn resolves_relative_href_against_users_base() {
        let r = LinkRewriter::new(
            "evo_lutio",
            HashSet::from([123u64]),
            Url::parse("https://users.livejournal.com/evo_lutio/").unwrap(),
        );
        // no-leading-slash relative href resolves against the users-scheme base
        assert_eq!(r.xref_target("123.html"), Some("xref:123.adoc".to_string()));
    }

    #[test]
    fn records_pending_link_targets() {
        let r = LinkRewriter::pending(
            "news",
            HashSet::from([2u64]),
            Url::parse("https://news.livejournal.com/").unwrap(),
        );

        let target = r
            .xref_target("https://news.livejournal.com/2.html")
            .unwrap();
        assert!(target.contains('\u{1f}'), "got: {target:?}");
        assert_ne!(target, "xref:2.adoc");

        let links = r.pending_links();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].token, target);
        assert_eq!(links[0].target_id, 2);
        assert_eq!(
            links[0].external_target,
            "https://news.livejournal.com/2.html"
        );
    }

    #[test]
    fn preserves_pending_query_and_fragment_target() {
        let r = LinkRewriter::pending(
            "news",
            HashSet::from([2u64]),
            Url::parse("https://news.livejournal.com/").unwrap(),
        );

        r.xref_target("https://news.livejournal.com/2.html?thread=5#c1")
            .unwrap();

        assert_eq!(
            r.pending_links()[0].external_target,
            "https://news.livejournal.com/2.html?thread=5#c1"
        );
    }

    #[test]
    fn preserves_resolved_pending_alternate_host_targets() {
        let subdomain = LinkRewriter::pending(
            "news",
            HashSet::from([2u64]),
            Url::parse("https://news.livejournal.com/").unwrap(),
        );
        subdomain
            .xref_target("https://users.livejournal.com/news/2.html")
            .unwrap();
        assert_eq!(
            subdomain.pending_links()[0].external_target,
            "https://users.livejournal.com/news/2.html"
        );

        let users = LinkRewriter::pending(
            "example_user",
            HashSet::from([2u64]),
            Url::parse("https://users.livejournal.com/example_user/").unwrap(),
        );
        users
            .xref_target("https://example-user.livejournal.com/2.html")
            .unwrap();
        assert_eq!(
            users.pending_links()[0].external_target,
            "https://example-user.livejournal.com/2.html"
        );
    }

    #[test]
    fn preserves_absolute_pending_target_against_loopback_base() {
        let r = LinkRewriter::pending(
            "news",
            HashSet::from([2u64]),
            Url::parse("http://127.0.0.1:1234/").unwrap(),
        );

        r.xref_target("https://news.livejournal.com/2.html?thread=5#c1")
            .unwrap();

        assert_eq!(
            r.pending_links()[0].external_target,
            "https://news.livejournal.com/2.html?thread=5#c1"
        );
    }

    #[test]
    fn resolves_failed_pending_links_externally() {
        let r = LinkRewriter::pending(
            "news",
            HashSet::from([2u64]),
            Url::parse("https://news.livejournal.com/").unwrap(),
        );
        let token = r
            .xref_target("https://news.livejournal.com/2.html")
            .unwrap();
        let rendered = format!("{token}[Two]");
        let links = r.pending_links();

        assert_eq!(
            resolve_pending_links(&rendered, &links, &HashSet::from([2u64])),
            "xref:2.adoc[Two]"
        );
        assert_eq!(
            resolve_pending_links(&rendered, &links, &HashSet::new()),
            "https://news.livejournal.com/2.html[Two]"
        );
    }

    #[test]
    fn resolves_adjacent_local_and_external_tokens_exactly() {
        let local = "\u{1f}lj-archiver-link:1:0\u{1f}";
        let external = "\u{1f}lj-archiver-link:2:1\u{1f}";
        let links = [
            pending_link(
                local,
                1,
                "xref:1.adoc",
                "https://news.livejournal.com/1.html",
            ),
            pending_link(
                external,
                2,
                "xref:2.adoc",
                "https://news.livejournal.com/2.html",
            ),
        ];

        assert_eq!(
            resolve_pending_links(
                &format!("before {local}{external} after"),
                &links,
                &HashSet::from([1]),
            ),
            "before xref:1.adochttps://news.livejournal.com/2.html after"
        );
    }

    #[test]
    fn resolves_every_repeated_occurrence_of_one_recorded_token() {
        let token = "\u{1f}lj-archiver-link:7:0\u{1f}";
        let links = [pending_link(
            token,
            7,
            "xref:7.adoc",
            "https://news.livejournal.com/7.html",
        )];

        assert_eq!(
            resolve_pending_links(
                &format!("{token}|{token}|{token}"),
                &links,
                &HashSet::from([7]),
            ),
            "xref:7.adoc|xref:7.adoc|xref:7.adoc"
        );
    }

    #[test]
    fn preserves_token_prefix_suffix_and_unknown_complete_lookalikes() {
        let token = "\u{1f}lj-archiver-link:7:0\u{1f}";
        let links = [pending_link(
            token,
            7,
            "xref:7.adoc",
            "https://news.livejournal.com/7.html",
        )];
        let text = concat!(
            "\u{1f}prefix-lj-archiver-link:7:0\u{1f}",
            "|\u{1f}lj-archiver-link:7:0-suffix\u{1f}",
            "|\u{1f}lj-archiver-link:999:0\u{1f}",
        );

        assert_eq!(
            resolve_pending_links(text, &links, &HashSet::from([7])),
            text
        );
    }

    #[test]
    fn preserves_malformed_candidate_and_all_remaining_text() {
        let token = "\u{1f}lj-archiver-link:7:0\u{1f}";
        let links = [pending_link(
            token,
            7,
            "xref:7.adoc",
            "https://news.livejournal.com/7.html",
        )];
        let text = "before \u{1f}lj-archiver-link:7:0 and trailing bytes";

        assert_eq!(
            resolve_pending_links(text, &links, &HashSet::from([7])),
            text
        );
    }

    #[test]
    fn preserves_ordinary_unit_separator_delimited_text() {
        let token = "\u{1f}lj-archiver-link:7:0\u{1f}";
        let links = [pending_link(
            token,
            7,
            "xref:7.adoc",
            "https://news.livejournal.com/7.html",
        )];
        let text = "before \u{1f}ordinary text\u{1f} and \u{1f}\u{1f} after";

        assert_eq!(
            resolve_pending_links(text, &links, &HashSet::from([7])),
            text
        );
    }

    #[test]
    fn does_not_recursively_rescan_replacement_text() {
        let first = "\u{1f}lj-archiver-link:1:0\u{1f}";
        let second = "\u{1f}lj-archiver-link:2:1\u{1f}";
        let links = [
            pending_link(first, 1, "xref:1.adoc", second),
            pending_link(
                second,
                2,
                "xref:2.adoc",
                "https://news.livejournal.com/2.html",
            ),
        ];

        assert_eq!(
            resolve_pending_links(first, &links, &HashSet::new()),
            second
        );
    }

    #[test]
    fn keeps_first_selected_target_for_duplicate_recorded_tokens() {
        let token = "\u{1f}lj-archiver-link:1:0\u{1f}";
        let links = [
            pending_link(token, 1, "xref:first.adoc", "https://example.test/first"),
            pending_link(token, 1, "xref:second.adoc", "https://example.test/second"),
        ];

        assert_eq!(
            resolve_pending_links(token, &links, &HashSet::from([1])),
            "xref:first.adoc"
        );
    }

    #[test]
    fn resolves_twenty_five_thousand_tokens_with_one_monotonic_input_pass() {
        const LINK_COUNT: u64 = 25_000;
        const EXPECTED_OUTPUT_LEN: usize = 175_009;

        let mut links = Vec::with_capacity(LINK_COUNT as usize);
        let mut local_ids = HashSet::with_capacity(LINK_COUNT as usize / 2);
        let mut text = String::from("start:");
        let mut expected = String::from("start:");

        for id in 1..=LINK_COUNT {
            if id > 1 {
                text.push('|');
                expected.push('|');
            }

            let token = format!("\u{1f}scale:{id:05}\u{1f}");
            let local_target = format!("L{id:05}");
            let external_target = format!("E{id:05}");
            text.push_str(&token);
            if id % 2 == 0 {
                local_ids.insert(id);
                expected.push_str(&local_target);
            } else {
                expected.push_str(&external_target);
            }
            links.push(pending_link(token, id, local_target, external_target));
        }
        text.push_str(":end");
        expected.push_str(":end");

        let (resolved, stats) = resolve_pending_links_with_scan_stats(&text, &links, &local_ids);

        assert_eq!(resolved, expected);
        assert_eq!(resolved.len(), EXPECTED_OUTPUT_LEN);
        assert_eq!(stats.candidates, LINK_COUNT as usize);
        assert_eq!(stats.cursor_advanced_bytes, text.len());
        assert_eq!(stats.accounted_input_bytes, text.len());
        assert_eq!(
            stats.copied_input_bytes + stats.candidate_input_bytes,
            text.len()
        );
        assert_eq!(stats.revisited_input_bytes, 0);
        assert_eq!(stats.skipped_input_bytes, 0);
    }

    #[test]
    fn restores_missing_resume_xrefs() {
        let final_ids = HashSet::from([3u64]);

        assert_eq!(
            restore_missing_xrefs(
                "xref:2.adoc[Two] and xref:3.adoc[Three]",
                "news",
                &final_ids,
            )
            .unwrap(),
            "https://news.livejournal.com/2.html[Two] and xref:3.adoc[Three]"
        );
    }
}
