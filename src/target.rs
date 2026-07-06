use url::Url;

use crate::{
    error::{LjError, Result},
    models::ArchiveTarget,
};

pub fn parse_target(input: &str, journal_context: Option<&str>) -> Result<ArchiveTarget> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return invalid(input, "target is empty");
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return parse_url_target(trimmed);
    }

    // A bare LiveJournal host or host+path (e.g. "evo-lutio.livejournal.com" or
    // "evo-lutio.livejournal.com/123.html") is treated as its https:// URL.
    if is_livejournal_host(trimmed) {
        return parse_url_target(&format!("https://{trimmed}"));
    }

    if let Some(id_text) = trimmed.strip_prefix('@') {
        let journal = journal_context.ok_or_else(|| LjError::InvalidTarget {
            input: input.to_string(),
            reason: "bare @id targets require --journal <NAME>".to_string(),
        })?;
        validate_journal(input, journal)?;
        let id = parse_post_id(input, id_text)?;
        return Ok(ArchiveTarget::Post {
            journal: journal.to_string(),
            id,
            url: post_url(journal, id)?,
        });
    }

    if let Some((journal, id_text)) = trimmed.split_once('/') {
        validate_journal(input, journal)?;
        let id = parse_post_id(input, id_text)?;
        return Ok(ArchiveTarget::Post {
            journal: journal.to_string(),
            id,
            url: post_url(journal, id)?,
        });
    }

    validate_journal(input, trimmed)?;
    Ok(ArchiveTarget::Journal {
        journal: trimmed.to_string(),
    })
}

/// The `https://…/` root URL string for a journal: the
/// `users.livejournal.com/<journal>/` form for underscore names, the
/// `<journal>.livejournal.com/` subdomain form otherwise. Callers append the
/// rest of the path (e.g. `<id>.html`).
fn journal_root(journal: &str) -> String {
    if journal.contains('_') {
        format!("https://users.livejournal.com/{journal}/")
    } else {
        format!("https://{journal}.livejournal.com/")
    }
}

pub fn post_url(journal: &str, id: u64) -> Result<Url> {
    validate_journal(journal, journal)?;
    if id == 0 {
        return invalid(journal, "post id must be a positive integer");
    }
    Url::parse(&format!("{}{id}.html", journal_root(journal))).map_err(|error| {
        LjError::InvalidTarget {
            input: journal.to_string(),
            reason: error.to_string(),
        }
    })
}

/// The journal's root URL, used as a base for resolving relative in-body links.
pub fn journal_base_url(journal: &str) -> Result<Url> {
    validate_journal(journal, journal)?;
    Url::parse(&journal_root(journal)).map_err(|error| LjError::InvalidTarget {
        input: journal.to_string(),
        reason: error.to_string(),
    })
}

/// Lenient `Option`-returning counterpart to `parse_url_target` (which is the
/// strict, error-reporting CLI parser) used for opportunistic link rewriting.
///
/// Extracts `(journal, post_id)` from a LiveJournal post URL for either scheme
/// (`<journal>.livejournal.com/<id>.html` or
/// `users.livejournal.com/<journal>/<id>.html`). Query and fragment are ignored.
/// Returns `None` for non-post or non-livejournal URLs.
pub fn post_ref_from_url(url: &Url) -> Option<(String, u64)> {
    let host = url.host_str()?;

    if host.eq_ignore_ascii_case("users.livejournal.com") {
        let mut segments = url.path_segments()?;
        let journal = segments.next()?;
        let file = segments.next()?;
        if journal.is_empty() || segments.next().is_some() {
            return None;
        }
        let id = file.strip_suffix(".html")?.parse::<u64>().ok()?;
        return (id > 0).then(|| (journal.to_string(), id));
    }

    let journal = host.strip_suffix(".livejournal.com")?;
    if journal.is_empty() || journal.eq_ignore_ascii_case("www") {
        return None;
    }
    let id = url
        .path()
        .strip_prefix('/')?
        .strip_suffix(".html")?
        .parse::<u64>()
        .ok()?;
    (id > 0).then(|| (journal.to_string(), id))
}

fn parse_url_target(input: &str) -> Result<ArchiveTarget> {
    let url = Url::parse(input).map_err(|error| LjError::InvalidTarget {
        input: input.to_string(),
        reason: error.to_string(),
    })?;
    let host = url.host_str().ok_or_else(|| LjError::InvalidTarget {
        input: input.to_string(),
        reason: "URL has no host".to_string(),
    })?;

    if host == "users.livejournal.com" {
        let segments: Vec<_> = url
            .path_segments()
            .map(|segments| segments.collect())
            .unwrap_or_default();

        // Post: /<journal>/<id>.html
        if segments.len() == 2 && !segments[1].is_empty() {
            let journal = segments[0];
            validate_journal(input, journal)?;
            let id = parse_html_id(input, segments[1])?;
            return Ok(ArchiveTarget::Post {
                journal: journal.to_string(),
                id,
                url,
            });
        }

        // Journal home: /<journal> or /<journal>/
        let journal = match segments.as_slice() {
            [journal] => Some(*journal),
            [journal, ""] => Some(*journal),
            _ => None,
        };
        if let Some(journal) = journal.filter(|journal| !journal.is_empty()) {
            validate_journal(input, journal)?;
            return Ok(ArchiveTarget::Journal {
                journal: journal.to_string(),
            });
        }

        return invalid(
            input,
            "users.livejournal.com URLs must be /<journal> or /<journal>/<id>.html",
        );
    }

    if let Some(journal) = host.strip_suffix(".livejournal.com") {
        if journal.is_empty() || journal.eq_ignore_ascii_case("www") {
            return invalid(input, "expected a livejournal.com post URL");
        }
        validate_journal(input, journal)?;
        let path = url.path();
        if path == "/" || path.is_empty() {
            return Ok(ArchiveTarget::Journal {
                journal: journal.to_string(),
            });
        }
        let id = id_from_path(input, path)?;
        return Ok(ArchiveTarget::Post {
            journal: journal.to_string(),
            id,
            url,
        });
    }

    if crate::net::is_loopback_test_server(&url) {
        let segments: Vec<_> = url
            .path_segments()
            .map(|segments| segments.collect())
            .unwrap_or_default();
        if segments.len() == 2 {
            let journal = segments[0];
            validate_journal(input, journal)?;
            let id = parse_html_id(input, segments[1])?;
            return Ok(ArchiveTarget::Post {
                journal: journal.to_string(),
                id,
                url,
            });
        }
        return invalid(
            input,
            "local test-server post URLs must be /<journal>/<id>.html",
        );
    }

    invalid(input, "expected a livejournal.com post URL")
}

fn id_from_path(input: &str, path: &str) -> Result<u64> {
    let segment = path.trim_start_matches('/');
    parse_html_id(input, segment)
}

fn parse_html_id(input: &str, segment: &str) -> Result<u64> {
    let Some(id_text) = segment.strip_suffix(".html") else {
        return invalid(input, "post URL path must end with <id>.html");
    };
    parse_post_id(input, id_text)
}

fn parse_post_id(input: &str, id_text: &str) -> Result<u64> {
    let id = id_text.parse::<u64>().map_err(|_| LjError::InvalidTarget {
        input: input.to_string(),
        reason: "post id must be a positive integer".to_string(),
    })?;

    if id == 0 {
        invalid(input, "post id must be a positive integer")
    } else {
        Ok(id)
    }
}

pub(crate) fn is_valid_journal_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => {
            first.is_ascii_alphanumeric()
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        }
        None => false,
    }
}

fn validate_journal(input: &str, journal: &str) -> Result<()> {
    if is_valid_journal_name(journal) {
        Ok(())
    } else {
        invalid(input, "journal name contains unsupported characters")
    }
}

fn is_livejournal_host(input: &str) -> bool {
    input
        .split('/')
        .next()
        .unwrap_or(input)
        .to_ascii_lowercase()
        .ends_with(".livejournal.com")
}

fn invalid<T>(input: &str, reason: &str) -> Result<T> {
    Err(LjError::InvalidTarget {
        input: input.to_string(),
        reason: reason.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_journal_name() {
        assert_eq!(
            parse_target("news", None).unwrap(),
            ArchiveTarget::Journal {
                journal: "news".to_string()
            }
        );
    }

    #[test]
    fn parses_journal_post_pair() {
        let parsed = parse_target("news/166511", None).unwrap();
        assert_post(
            parsed,
            "news",
            166511,
            "https://news.livejournal.com/166511.html",
        );
    }

    #[test]
    fn parses_underscore_journal_post_pair_with_users_url() {
        let parsed = parse_target("example_user/166511", None).unwrap();
        assert_post(
            parsed,
            "example_user",
            166511,
            "https://users.livejournal.com/example_user/166511.html",
        );
    }

    #[test]
    fn rejects_zero_id_in_journal_post_pair() {
        let error = parse_target("news/0", None).unwrap_err().to_string();
        assert!(error.contains("positive integer"));
    }

    #[test]
    fn parses_bare_id_with_journal_context() {
        let parsed = parse_target("@166511", Some("news")).unwrap();
        assert_post(
            parsed,
            "news",
            166511,
            "https://news.livejournal.com/166511.html",
        );
    }

    #[test]
    fn parses_bare_id_with_underscore_journal_context_using_users_url() {
        let parsed = parse_target("@166511", Some("example_user")).unwrap();
        assert_post(
            parsed,
            "example_user",
            166511,
            "https://users.livejournal.com/example_user/166511.html",
        );
    }

    #[test]
    fn rejects_zero_id_with_journal_context() {
        let error = parse_target("@0", Some("news")).unwrap_err().to_string();
        assert!(error.contains("positive integer"));
    }

    #[test]
    fn rejects_bare_id_without_context() {
        let error = parse_target("@166511", None).unwrap_err().to_string();
        assert!(error.contains("--journal"));
    }

    #[test]
    fn rejects_invalid_journal_context_for_bare_id() {
        let error = parse_target("@166511", Some("bad.name"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid target"));
    }

    #[test]
    fn parses_livejournal_url() {
        let parsed = parse_target("https://news.livejournal.com/166511.html", None).unwrap();
        assert_post(
            parsed,
            "news",
            166511,
            "https://news.livejournal.com/166511.html",
        );
    }

    #[test]
    fn parses_local_test_server_post_url() {
        let parsed = parse_target("http://127.0.0.1:37185/news/166511.html", None).unwrap();
        assert_post(
            parsed,
            "news",
            166511,
            "http://127.0.0.1:37185/news/166511.html",
        );

        let parsed = parse_target("http://localhost:37185/news/166511.html", None).unwrap();
        assert_post(
            parsed,
            "news",
            166511,
            "http://localhost:37185/news/166511.html",
        );
    }

    #[test]
    fn rejects_zero_id_in_livejournal_url() {
        let error = parse_target("https://news.livejournal.com/0.html", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("positive integer"));
    }

    #[test]
    fn post_url_rejects_zero_id() {
        let error = post_url("news", 0).unwrap_err().to_string();
        assert!(error.contains("positive"));
    }

    #[test]
    fn rejects_invalid_livejournal_subdomain() {
        let error = parse_target("https://bad.name.livejournal.com/166511.html", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid target"));
    }

    #[test]
    fn parses_users_livejournal_url() {
        let parsed = parse_target(
            "https://users.livejournal.com/example_user/166511.html",
            None,
        )
        .unwrap();
        assert_post(
            parsed,
            "example_user",
            166511,
            "https://users.livejournal.com/example_user/166511.html",
        );
    }

    #[test]
    fn rejects_users_livejournal_url_without_journal_segment() {
        let error = parse_target("https://users.livejournal.com/166511.html", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid target"));
    }

    #[test]
    fn rejects_users_livejournal_name_with_leading_underscore() {
        let error = parse_target("https://users.livejournal.com/_bad/166511.html", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid target"));
    }

    #[test]
    fn parses_users_livejournal_name_with_trailing_underscore() {
        let parsed = parse_target("https://users.livejournal.com/bad_/166511.html", None).unwrap();
        assert_post(
            parsed,
            "bad_",
            166511,
            "https://users.livejournal.com/bad_/166511.html",
        );
    }

    #[test]
    fn rejects_www_livejournal_url() {
        let error = parse_target("https://www.livejournal.com/166511.html", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid target"));
    }

    #[test]
    fn parses_bare_hostname_as_journal() {
        assert_eq!(
            parse_target("evo-lutio.livejournal.com", None).unwrap(),
            ArchiveTarget::Journal {
                journal: "evo-lutio".to_string()
            }
        );
    }

    #[test]
    fn parses_journal_home_url_as_journal() {
        assert_eq!(
            parse_target("https://evo-lutio.livejournal.com/", None).unwrap(),
            ArchiveTarget::Journal {
                journal: "evo-lutio".to_string()
            }
        );
    }

    #[test]
    fn parses_journal_home_url_without_trailing_slash_as_journal() {
        assert_eq!(
            parse_target("https://evo-lutio.livejournal.com", None).unwrap(),
            ArchiveTarget::Journal {
                journal: "evo-lutio".to_string()
            }
        );
    }

    #[test]
    fn parses_bare_hostname_with_post_as_post() {
        let parsed = parse_target("evo-lutio.livejournal.com/1753760.html", None).unwrap();
        assert_post(
            parsed,
            "evo-lutio",
            1753760,
            "https://evo-lutio.livejournal.com/1753760.html",
        );
    }

    #[test]
    fn parses_users_home_url_as_journal() {
        assert_eq!(
            parse_target("https://users.livejournal.com/example_user/", None).unwrap(),
            ArchiveTarget::Journal {
                journal: "example_user".to_string()
            }
        );
    }

    #[test]
    fn parses_users_home_url_without_trailing_slash_as_journal() {
        assert_eq!(
            parse_target("https://users.livejournal.com/example_user", None).unwrap(),
            ArchiveTarget::Journal {
                journal: "example_user".to_string()
            }
        );
    }

    #[test]
    fn rejects_www_home_url() {
        let error = parse_target("https://www.livejournal.com/", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid target"));
    }

    #[test]
    fn post_ref_from_subdomain_and_users_urls() {
        assert_eq!(
            post_ref_from_url(&Url::parse("https://news.livejournal.com/166512.html").unwrap()),
            Some(("news".to_string(), 166512))
        );
        assert_eq!(
            post_ref_from_url(
                &Url::parse("https://users.livejournal.com/example_user/166512.html").unwrap()
            ),
            Some(("example_user".to_string(), 166512))
        );
    }

    #[test]
    fn post_ref_none_for_non_post_urls() {
        for url in [
            "https://news.livejournal.com/",
            "https://news.livejournal.com/tag/x",
            "https://www.livejournal.com/166512.html",
            "https://example.com/166512.html",
            "https://users.livejournal.com/166512.html",
            "https://news.livejournal.com/0.html",
        ] {
            assert_eq!(post_ref_from_url(&Url::parse(url).unwrap()), None, "{url}");
        }
    }

    #[test]
    fn builds_journal_base_urls_for_both_schemes() {
        assert_eq!(
            journal_base_url("news").unwrap().as_str(),
            "https://news.livejournal.com/"
        );
        assert_eq!(
            journal_base_url("example_user").unwrap().as_str(),
            "https://users.livejournal.com/example_user/"
        );
    }

    fn assert_post(parsed: ArchiveTarget, journal: &str, id: u64, url: &str) {
        assert_eq!(
            parsed,
            ArchiveTarget::Post {
                journal: journal.to_string(),
                id,
                url: Url::parse(url).unwrap(),
            }
        );
    }
}
