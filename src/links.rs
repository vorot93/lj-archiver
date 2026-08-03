use crate::{
    adoc::sanitize_macro_target,
    target::{journals_alias, post_ref_from_url},
};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use url::Url;

pub(crate) const POST_LINK_PREFIX_ATTRIBUTE: &str = "lj-archiver-post-link-prefix";
pub(crate) const POST_LINK_PREFIX_REFERENCE: &str = "{lj-archiver-post-link-prefix}";
const PENDING_LINK_SEPARATOR: char = '\u{1f}';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveLinkContext {
    Xref,
    ImageLink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveLink {
    pub start: usize,
    pub end: usize,
    pub target_id: u64,
    pub context: ActiveLinkContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedLinks {
    pub contents: String,
    pub links: Vec<ActiveLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PendingLinkError {
    #[error("pending token was recorded more than once")]
    DuplicateRecordedToken,
    #[error("pending token occurrence count was {actual}, expected {expected}")]
    OccurrenceCount { expected: usize, actual: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingLink {
    pub token: String,
    pub target_id: u64,
    pub local_target: String,
    pub external_target: String,
    pub context: ActiveLinkContext,
}

#[derive(Clone, Copy)]
enum LocalTargetContext {
    XrefMacro,
    ImageLinkAttribute,
}

enum RewriteMode {
    Direct,
    Pending { namespace: String },
}

pub struct LinkRewriter {
    journal: String,
    targets: Arc<HashMap<u64, String>>,
    base_url: Url,
    mode: RewriteMode,
    pending_links: Mutex<Vec<PendingLink>>,
}

impl LinkRewriter {
    pub fn new(journal: impl Into<String>, ids: HashSet<u64>, base_url: Url) -> Self {
        Self::with_targets(journal, canonical_targets(ids), base_url)
    }

    pub(crate) fn with_targets(
        journal: impl Into<String>,
        targets: Arc<HashMap<u64, String>>,
        base_url: Url,
    ) -> Self {
        Self {
            journal: journal.into(),
            targets,
            base_url,
            mode: RewriteMode::Direct,
            pending_links: Mutex::new(Vec::new()),
        }
    }

    pub fn pending(
        journal: impl Into<String>,
        ids: HashSet<u64>,
        base_url: Url,
        namespace: impl Into<String>,
    ) -> Self {
        Self::pending_with_targets(journal, canonical_targets(ids), base_url, namespace)
    }

    pub(crate) fn pending_with_targets(
        journal: impl Into<String>,
        targets: Arc<HashMap<u64, String>>,
        base_url: Url,
        namespace: impl Into<String>,
    ) -> Self {
        Self {
            journal: journal.into(),
            targets,
            base_url,
            mode: RewriteMode::Pending {
                namespace: namespace.into(),
            },
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
        if !journals_alias(&journal, &self.journal) {
            return None;
        }
        let filename = self.targets.get(&id)?;

        let local_target = match context {
            LocalTargetContext::XrefMacro => format!("xref:{filename}"),
            LocalTargetContext::ImageLinkAttribute => {
                format!("{POST_LINK_PREFIX_REFERENCE}{filename}")
            }
        };

        match &self.mode {
            RewriteMode::Direct => Some(local_target),
            RewriteMode::Pending { namespace } => {
                let mut links = self
                    .pending_links
                    .lock()
                    .expect("pending link recorder mutex poisoned");
                let token = format!(
                    "{PENDING_LINK_SEPARATOR}lj-archiver-link:{namespace}:{id}:{}{PENDING_LINK_SEPARATOR}",
                    links.len()
                );
                links.push(PendingLink {
                    token: token.clone(),
                    target_id: id,
                    local_target,
                    external_target: pending_external_target(&url, context),
                    context: match context {
                        LocalTargetContext::XrefMacro => ActiveLinkContext::Xref,
                        LocalTargetContext::ImageLinkAttribute => ActiveLinkContext::ImageLink,
                    },
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

fn canonical_targets(ids: HashSet<u64>) -> Arc<HashMap<u64, String>> {
    Arc::new(
        ids.into_iter()
            .map(|id| (id, format!("{id}.adoc")))
            .collect(),
    )
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
) -> std::result::Result<ResolvedLinks, PendingLinkError> {
    resolve_pending_links_with_scan_stats(text, links, local_ids).map(|(resolved, _)| resolved)
}

pub(crate) fn validate_pending_links(
    text: &str,
    links: &[PendingLink],
) -> std::result::Result<(), PendingLinkError> {
    let mut occurrences = HashMap::with_capacity(links.len());
    for link in links {
        if occurrences.insert(link.token.as_str(), 0_usize).is_some() {
            return Err(PendingLinkError::DuplicateRecordedToken);
        }
    }

    let mut cursor = 0;
    while let Some(relative_start) = text[cursor..].find(PENDING_LINK_SEPARATOR) {
        let start = cursor + relative_start;
        let terminator_search_start = start + PENDING_LINK_SEPARATOR.len_utf8();
        let Some(relative_end) = text[terminator_search_start..].find(PENDING_LINK_SEPARATOR)
        else {
            break;
        };
        let end = terminator_search_start + relative_end + PENDING_LINK_SEPARATOR.len_utf8();
        if let Some(actual) = occurrences.get_mut(&text[start..end]) {
            *actual += 1;
            cursor = end - PENDING_LINK_SEPARATOR.len_utf8();
        } else {
            cursor = terminator_search_start;
        }
    }

    for link in links {
        let actual = occurrences[link.token.as_str()];
        if actual != 1 {
            return Err(PendingLinkError::OccurrenceCount {
                expected: 1,
                actual,
            });
        }
    }
    Ok(())
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

#[derive(Clone, Copy)]
struct PendingCandidate {
    start: usize,
    end: usize,
    recorded: bool,
    pending_token_shape: bool,
    pending_token_prefix: bool,
}

fn has_pending_token_prefix(candidate: &str) -> bool {
    candidate
        .strip_prefix(PENDING_LINK_SEPARATOR)
        .and_then(|candidate| candidate.strip_suffix(PENDING_LINK_SEPARATOR))
        .is_some_and(|body| body.starts_with("lj-archiver-link:"))
}

fn has_pending_token_shape(candidate: &str) -> bool {
    let Some(body) = candidate
        .strip_prefix(PENDING_LINK_SEPARATOR)
        .and_then(|candidate| candidate.strip_suffix(PENDING_LINK_SEPARATOR))
    else {
        return false;
    };
    let mut parts = body.split(':');
    let Some("lj-archiver-link") = parts.next() else {
        return false;
    };
    let Some(namespace) = parts.next() else {
        return false;
    };
    let Some(target_id) = parts.next() else {
        return false;
    };
    let Some(ordinal) = parts.next() else {
        return false;
    };

    namespace.len() == 32
        && namespace
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && target_id.parse::<u64>().is_ok()
        && ordinal.parse::<usize>().is_ok()
        && parts.next().is_none()
}

fn preserves_complete_unrecorded_candidate(candidates: &[PendingCandidate], index: usize) -> bool {
    let candidate = candidates[index];
    debug_assert!(!candidate.recorded);
    if candidate.pending_token_shape {
        return true;
    }
    if candidate.pending_token_prefix {
        return false;
    }

    let preceded_by_token =
        index > 0 && (candidates[index - 1].recorded || candidates[index - 1].pending_token_shape);
    let followed_by_token = index + 1 < candidates.len()
        && (candidates[index + 1].recorded || candidates[index + 1].pending_token_shape);
    !(preceded_by_token && followed_by_token)
}

fn resolve_pending_links_with_scan_stats(
    text: &str,
    links: &[PendingLink],
    local_ids: &HashSet<u64>,
) -> std::result::Result<(ResolvedLinks, PendingLinkScanStats), PendingLinkError> {
    validate_pending_links(text, links)?;
    let mut replacements = HashMap::with_capacity(links.len());
    for link in links {
        let target = if local_ids.contains(&link.target_id) {
            link.local_target.as_str()
        } else {
            link.external_target.as_str()
        };
        replacements.insert(link.token.as_str(), (target, link.target_id, link.context));
    }
    let mut resolved = String::with_capacity(text.len());
    let mut active_links = Vec::with_capacity(links.len());
    let mut stats = PendingLinkScanStats::default();
    let separator_len = PENDING_LINK_SEPARATOR.len_utf8();
    let mut separator_positions = Vec::new();
    let mut scan_cursor = 0;
    while let Some(relative_start) = text[scan_cursor..].find(PENDING_LINK_SEPARATOR) {
        let start = scan_cursor + relative_start;
        separator_positions.push(start);
        scan_cursor = start + separator_len;
    }

    let candidates = separator_positions
        .windows(2)
        .map(|positions| {
            let start = positions[0];
            let end = positions[1] + separator_len;
            let candidate = &text[start..end];
            PendingCandidate {
                start,
                end,
                recorded: replacements.contains_key(candidate),
                pending_token_shape: has_pending_token_shape(candidate),
                pending_token_prefix: has_pending_token_prefix(candidate),
            }
        })
        .collect::<Vec<_>>();
    stats.candidates = candidates
        .iter()
        .enumerate()
        .filter(|(index, candidate)| {
            candidate.recorded
                || candidate.pending_token_shape
                || *index == 0
                || !candidates[*index - 1].recorded
        })
        .count();

    let mut cursor = 0;
    for (index, candidate) in candidates.iter().enumerate() {
        if !candidate.recorded {
            continue;
        }

        let mut replacement_start = candidate.start;
        if index > 0
            && (candidates[index - 1].recorded
                || preserves_complete_unrecorded_candidate(&candidates, index - 1))
        {
            replacement_start += separator_len;
        }
        let mut replacement_end = candidate.end;
        if index + 1 < candidates.len()
            && !candidates[index + 1].recorded
            && preserves_complete_unrecorded_candidate(&candidates, index + 1)
        {
            replacement_end -= separator_len;
        }

        resolved.push_str(&text[cursor..replacement_start]);
        stats.account_input_range(cursor, replacement_start, false);
        let (target, target_id, context) = replacements[&text[candidate.start..candidate.end]];
        let output_start = resolved.len();
        resolved.push_str(target);
        active_links.push(ActiveLink {
            start: output_start,
            end: resolved.len(),
            target_id,
            context,
        });
        stats.account_input_range(replacement_start, replacement_end, true);
        stats.cursor_advanced_bytes += replacement_end - cursor;
        cursor = replacement_end;
    }

    if cursor < text.len() {
        resolved.push_str(&text[cursor..]);
        stats.account_input_range(cursor, text.len(), false);
        stats.cursor_advanced_bytes += text.len() - cursor;
    }

    active_links.sort_by_key(|link| link.start);
    Ok((
        ResolvedLinks {
            contents: resolved,
            links: active_links,
        },
        stats,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_NAMESPACE: &str = "00112233445566778899aabbccddeeff";

    fn direct_rewriter_with_targets(targets: HashMap<u64, String>, base_url: Url) -> LinkRewriter {
        LinkRewriter::with_targets("news", Arc::new(targets), base_url)
    }

    fn pending_rewriter_with_targets(targets: HashMap<u64, String>, base_url: Url) -> LinkRewriter {
        LinkRewriter::pending_with_targets("news", Arc::new(targets), base_url, TEST_NAMESPACE)
    }

    fn pending_rewriter_with_namespace(namespace: &str) -> LinkRewriter {
        LinkRewriter::pending(
            "news",
            HashSet::from([7]),
            Url::parse("https://news.livejournal.com/").unwrap(),
            namespace,
        )
    }

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
            context: ActiveLinkContext::Xref,
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
    fn target_aware_direct_targets_preserve_aliases_in_text_and_image_contexts() {
        let r = direct_rewriter_with_targets(
            HashMap::from([(1, "01.adoc".to_string()), (2, "2.adoc".to_string())]),
            Url::parse("https://news.livejournal.com/").unwrap(),
        );

        assert_eq!(
            r.xref_target("https://news.livejournal.com/1.html"),
            Some("xref:01.adoc".to_string())
        );
        assert_eq!(
            r.image_link_target("https://news.livejournal.com/1.html"),
            Some("{lj-archiver-post-link-prefix}01.adoc".to_string())
        );
        assert_eq!(
            r.xref_target("https://news.livejournal.com/2.html"),
            Some("xref:2.adoc".to_string())
        );
    }

    #[test]
    fn target_aware_pending_records_exact_alias_targets_for_both_contexts() {
        let r = pending_rewriter_with_targets(
            HashMap::from([(1, "01.adoc".to_string())]),
            Url::parse("https://news.livejournal.com/").unwrap(),
        );
        let text_token = r
            .xref_target("https://news.livejournal.com/1.html")
            .unwrap();
        let image_token = r
            .image_link_target("https://news.livejournal.com/1.html")
            .unwrap();
        let links = r.pending_links();

        assert_eq!(links.len(), 2);
        assert_eq!(links[0].token, text_token);
        assert_eq!(links[0].local_target, "xref:01.adoc");
        assert_eq!(links[1].token, image_token);
        assert_eq!(
            links[1].local_target,
            "{lj-archiver-post-link-prefix}01.adoc"
        );
        assert_eq!(
            resolve_pending_links(
                &format!("{text_token}[One] {image_token}"),
                &links,
                &HashSet::from([1]),
            )
            .unwrap()
            .contents,
            "xref:01.adoc[One] {lj-archiver-post-link-prefix}01.adoc"
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
            TEST_NAMESPACE,
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
    fn pending_tokens_include_namespace_ordinal_and_context() {
        let rewriter = pending_rewriter_with_namespace(TEST_NAMESPACE);

        let xref = rewriter
            .xref_target("https://news.livejournal.com/7.html")
            .unwrap();
        let image = rewriter
            .image_link_target("https://news.livejournal.com/7.html")
            .unwrap();

        assert_eq!(
            xref,
            "\u{1f}lj-archiver-link:00112233445566778899aabbccddeeff:7:0\u{1f}"
        );
        assert_eq!(
            image,
            "\u{1f}lj-archiver-link:00112233445566778899aabbccddeeff:7:1\u{1f}"
        );
        let links = rewriter.pending_links();
        assert_eq!(links[0].context, ActiveLinkContext::Xref);
        assert_eq!(links[1].context, ActiveLinkContext::ImageLink);
    }

    #[test]
    fn pending_validation_rejects_duplicate_recorded_token() {
        let token = format!("\u{1f}lj-archiver-link:{TEST_NAMESPACE}:7:0\u{1f}");
        let links = [
            PendingLink {
                token: token.clone(),
                target_id: 7,
                local_target: "xref:7.adoc".to_string(),
                external_target: "https://news.livejournal.com/7.html".to_string(),
                context: ActiveLinkContext::Xref,
            },
            PendingLink {
                token: token.clone(),
                target_id: 7,
                local_target: "xref:7.adoc".to_string(),
                external_target: "https://news.livejournal.com/7.html".to_string(),
                context: ActiveLinkContext::Xref,
            },
        ];

        assert_eq!(
            validate_pending_links(&token, &links).unwrap_err(),
            PendingLinkError::DuplicateRecordedToken
        );
    }

    #[test]
    fn pending_resolution_rejects_duplicate_recorded_token_occurrence() {
        let rewriter = pending_rewriter_with_namespace(TEST_NAMESPACE);
        let token = rewriter
            .xref_target("https://news.livejournal.com/7.html")
            .unwrap();
        let links = rewriter.pending_links();
        let text = format!("{token}[one] and {token}[two]");

        assert_eq!(
            resolve_pending_links(&text, &links, &HashSet::from([7])).unwrap_err(),
            PendingLinkError::OccurrenceCount {
                expected: 1,
                actual: 2,
            }
        );
    }

    #[test]
    fn pending_validation_rejects_partial_prefix_shared_boundary_collision() {
        let rewriter = pending_rewriter_with_namespace(TEST_NAMESPACE);
        let token = rewriter
            .xref_target("https://news.livejournal.com/7.html")
            .unwrap();
        let body = &token
            [PENDING_LINK_SEPARATOR.len_utf8()..token.len() - PENDING_LINK_SEPARATOR.len_utf8()];
        let text = format!("authored {PENDING_LINK_SEPARATOR}{body}{token}[seven]");

        assert_eq!(
            validate_pending_links(&text, &rewriter.pending_links()).unwrap_err(),
            PendingLinkError::OccurrenceCount {
                expected: 1,
                actual: 2,
            }
        );
    }

    #[test]
    fn pending_validation_rejects_partial_suffix_shared_boundary_collision() {
        let rewriter = pending_rewriter_with_namespace(TEST_NAMESPACE);
        let token = rewriter
            .xref_target("https://news.livejournal.com/7.html")
            .unwrap();
        let body = &token
            [PENDING_LINK_SEPARATOR.len_utf8()..token.len() - PENDING_LINK_SEPARATOR.len_utf8()];
        let text = format!("{token}{body}{PENDING_LINK_SEPARATOR} authored");

        assert_eq!(
            validate_pending_links(&text, &rewriter.pending_links()).unwrap_err(),
            PendingLinkError::OccurrenceCount {
                expected: 1,
                actual: 2,
            }
        );
    }

    #[test]
    fn pending_resolution_rejects_shared_boundary_collision_before_replacement() {
        let rewriter = pending_rewriter_with_namespace(TEST_NAMESPACE);
        let token = rewriter
            .xref_target("https://news.livejournal.com/7.html")
            .unwrap();
        let body = &token
            [PENDING_LINK_SEPARATOR.len_utf8()..token.len() - PENDING_LINK_SEPARATOR.len_utf8()];
        let text = format!("authored {PENDING_LINK_SEPARATOR}{body}{token}[seven]");

        assert_eq!(
            resolve_pending_links(&text, &rewriter.pending_links(), &HashSet::from([7]))
                .unwrap_err(),
            PendingLinkError::OccurrenceCount {
                expected: 1,
                actual: 2,
            }
        );
    }

    #[test]
    fn pending_resolution_resolves_distinct_recorded_tokens_sharing_one_separator() {
        let first = "\u{1f}first\u{1f}";
        let second = "\u{1f}second\u{1f}";
        let links = [
            pending_link(
                first,
                1,
                "xref:1.adoc",
                "https://news.livejournal.com/1.html",
            ),
            pending_link(
                second,
                2,
                "xref:2.adoc",
                "https://news.livejournal.com/2.html",
            ),
        ];
        let text = "before \u{1f}first\u{1f}second\u{1f} after";

        assert_eq!(
            resolve_pending_links(text, &links, &HashSet::from([1, 2])).unwrap(),
            ResolvedLinks {
                contents: "before xref:1.adocxref:2.adoc after".to_string(),
                links: vec![
                    ActiveLink {
                        start: 7,
                        end: 18,
                        target_id: 1,
                        context: ActiveLinkContext::Xref,
                    },
                    ActiveLink {
                        start: 18,
                        end: 29,
                        target_id: 2,
                        context: ActiveLinkContext::Xref,
                    },
                ],
            }
        );
    }

    #[test]
    fn pending_resolution_preserves_unrecorded_token_sharing_recorded_closing_separator() {
        let recorded = "\u{1f}recorded\u{1f}";
        let links = [pending_link(
            recorded,
            7,
            "xref:7.adoc",
            "https://news.livejournal.com/7.html",
        )];
        let text = concat!(
            "before \u{1f}recorded\u{1f}lj-archiver-link:",
            "ffeeddccbbaa99887766554433221100:8:0\u{1f} after",
        );

        assert_eq!(
            resolve_pending_links(text, &links, &HashSet::from([7])).unwrap(),
            ResolvedLinks {
                contents: concat!(
                    "before xref:7.adoc\u{1f}lj-archiver-link:",
                    "ffeeddccbbaa99887766554433221100:8:0\u{1f} after",
                )
                .to_string(),
                links: vec![ActiveLink {
                    start: 7,
                    end: 18,
                    target_id: 7,
                    context: ActiveLinkContext::Xref,
                }],
            }
        );
    }

    #[test]
    fn pending_resolution_preserves_ordinary_text_sharing_recorded_closing_separator() {
        let recorded = "\u{1f}recorded\u{1f}";
        let links = [pending_link(
            recorded,
            7,
            "xref:7.adoc",
            "https://news.livejournal.com/7.html",
        )];
        let text = "before \u{1f}recorded\u{1f}ordinary\u{1f} after";

        assert_eq!(
            resolve_pending_links(text, &links, &HashSet::from([7])).unwrap(),
            ResolvedLinks {
                contents: "before xref:7.adoc\u{1f}ordinary\u{1f} after".to_string(),
                links: vec![ActiveLink {
                    start: 7,
                    end: 18,
                    target_id: 7,
                    context: ActiveLinkContext::Xref,
                }],
            }
        );
    }

    #[test]
    fn pending_resolution_preserves_ordinary_text_sharing_recorded_opening_separator() {
        let recorded = "\u{1f}recorded\u{1f}";
        let links = [pending_link(
            recorded,
            7,
            "xref:7.adoc",
            "https://news.livejournal.com/7.html",
        )];
        let text = "before \u{1f}ordinary\u{1f}recorded\u{1f} after";

        assert_eq!(
            resolve_pending_links(text, &links, &HashSet::from([7])).unwrap(),
            ResolvedLinks {
                contents: "before \u{1f}ordinary\u{1f}xref:7.adoc after".to_string(),
                links: vec![ActiveLink {
                    start: 17,
                    end: 28,
                    target_id: 7,
                    context: ActiveLinkContext::Xref,
                }],
            }
        );
    }

    #[test]
    fn pending_validation_rejects_recorded_token_missing_from_text() {
        let rewriter = pending_rewriter_with_namespace(TEST_NAMESPACE);
        rewriter
            .xref_target("https://news.livejournal.com/7.html")
            .unwrap();

        assert_eq!(
            validate_pending_links("authored text only", &rewriter.pending_links()).unwrap_err(),
            PendingLinkError::OccurrenceCount {
                expected: 1,
                actual: 0,
            }
        );
    }

    #[test]
    fn pending_validation_recognizes_token_after_odd_ordinary_separator() {
        let rewriter = pending_rewriter_with_namespace(TEST_NAMESPACE);
        let token = rewriter
            .xref_target("https://news.livejournal.com/7.html")
            .unwrap();
        let text = format!("authored \u{1f} text {token}[seven]");

        validate_pending_links(&text, &rewriter.pending_links()).unwrap();
    }

    #[test]
    fn pending_resolution_preserves_malformed_separator_fragment_before_token() {
        let rewriter = pending_rewriter_with_namespace(TEST_NAMESPACE);
        let token = rewriter
            .xref_target("https://news.livejournal.com/7.html")
            .unwrap();
        let prefix = "before \u{1f}lj-archiver-link:malformed ";
        let text = format!("{prefix}{token}[seven]");
        let contents = format!("{prefix}xref:7.adoc[seven]");

        assert_eq!(
            resolve_pending_links(&text, &rewriter.pending_links(), &HashSet::from([7])).unwrap(),
            ResolvedLinks {
                contents,
                links: vec![ActiveLink {
                    start: prefix.len(),
                    end: prefix.len() + "xref:7.adoc".len(),
                    target_id: 7,
                    context: ActiveLinkContext::Xref,
                }],
            }
        );
    }

    #[test]
    fn pending_resolution_preserves_unrecorded_token_and_returns_exact_xref_span() {
        let rewriter = pending_rewriter_with_namespace(TEST_NAMESPACE);
        let token = rewriter
            .xref_target("https://news.livejournal.com/7.html")
            .unwrap();
        let unknown = "\u{1f}lj-archiver-link:ffeeddccbbaa99887766554433221100:7:0\u{1f}";
        let text = format!("before {token}[seven] {unknown} after");
        let contents = format!("before xref:7.adoc[seven] {unknown} after");

        assert_eq!(
            resolve_pending_links(&text, &rewriter.pending_links(), &HashSet::from([7])).unwrap(),
            ResolvedLinks {
                contents,
                links: vec![ActiveLink {
                    start: 7,
                    end: 18,
                    target_id: 7,
                    context: ActiveLinkContext::Xref,
                }],
            }
        );
    }

    #[test]
    fn pending_resolution_returns_exact_image_link_span_and_context() {
        let rewriter = pending_rewriter_with_namespace(TEST_NAMESPACE);
        let token = rewriter
            .image_link_target("https://news.livejournal.com/7.html")
            .unwrap();
        let text = format!("image:photo.jpg[photo,link={token}]");
        let target = "{lj-archiver-post-link-prefix}7.adoc";
        let contents = format!("image:photo.jpg[photo,link={target}]");

        assert_eq!(
            resolve_pending_links(&text, &rewriter.pending_links(), &HashSet::from([7])).unwrap(),
            ResolvedLinks {
                contents,
                links: vec![ActiveLink {
                    start: 27,
                    end: 63,
                    target_id: 7,
                    context: ActiveLinkContext::ImageLink,
                }],
            }
        );
    }

    #[test]
    fn preserves_pending_query_and_fragment_target() {
        let r = LinkRewriter::pending(
            "news",
            HashSet::from([2u64]),
            Url::parse("https://news.livejournal.com/").unwrap(),
            TEST_NAMESPACE,
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
            TEST_NAMESPACE,
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
            TEST_NAMESPACE,
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
            TEST_NAMESPACE,
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
            TEST_NAMESPACE,
        );
        let token = r
            .xref_target("https://news.livejournal.com/2.html")
            .unwrap();
        let rendered = format!("{token}[Two]");
        let links = r.pending_links();

        assert_eq!(
            resolve_pending_links(&rendered, &links, &HashSet::from([2u64]))
                .unwrap()
                .contents,
            "xref:2.adoc[Two]"
        );
        assert_eq!(
            resolve_pending_links(&rendered, &links, &HashSet::new())
                .unwrap()
                .contents,
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
            )
            .unwrap()
            .contents,
            "before xref:1.adochttps://news.livejournal.com/2.html after"
        );
    }

    #[test]
    fn rejects_repeated_occurrences_of_one_recorded_token() {
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
            )
            .unwrap_err(),
            PendingLinkError::OccurrenceCount {
                expected: 1,
                actual: 3,
            }
        );
    }

    #[test]
    fn preserves_token_prefix_suffix_and_unknown_complete_lookalikes() {
        let links = [];
        let text = concat!(
            "\u{1f}prefix-lj-archiver-link:7:0\u{1f}",
            "|\u{1f}lj-archiver-link:7:0-suffix\u{1f}",
            "|\u{1f}lj-archiver-link:999:0\u{1f}",
        );

        assert_eq!(
            resolve_pending_links(text, &links, &HashSet::from([7]))
                .unwrap()
                .contents,
            text
        );
    }

    #[test]
    fn preserves_malformed_candidate_and_all_remaining_text() {
        let links = [];
        let text = "before \u{1f}lj-archiver-link:7:0 and trailing bytes";

        assert_eq!(
            resolve_pending_links(text, &links, &HashSet::from([7]))
                .unwrap()
                .contents,
            text
        );
    }

    #[test]
    fn preserves_ordinary_unit_separator_delimited_text() {
        let links = [];
        let text = "before \u{1f}ordinary text\u{1f} and \u{1f}\u{1f} after";

        assert_eq!(
            resolve_pending_links(text, &links, &HashSet::from([7]))
                .unwrap()
                .contents,
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
            resolve_pending_links(&format!("{first}|{second}"), &links, &HashSet::from([2]),)
                .unwrap()
                .contents,
            format!("{second}|xref:2.adoc")
        );
    }

    #[test]
    fn rejects_duplicate_recorded_tokens_during_resolution() {
        let token = "\u{1f}lj-archiver-link:1:0\u{1f}";
        let links = [
            pending_link(token, 1, "xref:first.adoc", "https://example.test/first"),
            pending_link(token, 1, "xref:second.adoc", "https://example.test/second"),
        ];

        assert_eq!(
            resolve_pending_links(token, &links, &HashSet::from([1])).unwrap_err(),
            PendingLinkError::DuplicateRecordedToken
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

        let (resolved, stats) =
            resolve_pending_links_with_scan_stats(&text, &links, &local_ids).unwrap();

        assert_eq!(resolved.contents, expected);
        assert_eq!(resolved.contents.len(), EXPECTED_OUTPUT_LEN);
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
}
