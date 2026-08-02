use crate::{
    Result,
    adoc::sanitize_macro_target,
    target::{journals_alias, post_ref_from_url, post_url},
};
use std::{collections::HashSet, sync::Mutex};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingLink {
    pub token: String,
    pub target_id: u64,
    pub external_target: String,
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
        let url = self.base_url.join(href).ok()?;
        let (journal, id) = post_ref_from_url(&url)?;
        if !journals_alias(&journal, &self.journal) || !self.ids.contains(&id) {
            return None;
        }

        match self.mode {
            RewriteMode::Direct => Some(format!("xref:{id}.adoc")),
            RewriteMode::Pending => {
                let mut links = self
                    .pending_links
                    .lock()
                    .expect("pending link recorder mutex poisoned");
                let token = format!("\u{1f}lj-archiver-link:{id}:{}\u{1f}", links.len());
                links.push(PendingLink {
                    token: token.clone(),
                    target_id: id,
                    external_target: sanitize_macro_target(url.as_str()),
                });
                Some(token)
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn pending_links(&self) -> Vec<PendingLink> {
        self.pending_links
            .lock()
            .expect("pending link recorder mutex poisoned")
            .clone()
    }
}

#[allow(dead_code)]
pub(crate) fn resolve_pending_links(
    text: &str,
    links: &[PendingLink],
    local_ids: &HashSet<u64>,
) -> String {
    let mut resolved = text.to_string();

    for link in links {
        let target = if local_ids.contains(&link.target_id) {
            format!("xref:{}.adoc", link.target_id)
        } else {
            link.external_target.clone()
        };
        resolved = resolved.replace(&link.token, &target);
    }

    resolved
}

#[allow(dead_code)]
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
