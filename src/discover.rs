use crate::{
    client::LjClient,
    error::{LjError, Result},
    progress::Progress,
    target::journals_alias,
};
use scraper::{Html, Selector};
use std::{collections::HashSet, sync::LazyLock};
use url::{Host, Url};

static ANCHOR_HREF: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a[href]").expect("static selector must parse"));

/// Exact crawl bounds. LiveJournal has no journal with more than a century of
/// archives, so exceeding either bound means the calendar is hostile or the
/// navigation shape changed — either way, fail closed *before* the requests
/// rather than emitting thousands of partial fetches or silently truncating.
const MAX_DISCOVERY_YEAR_PAGES: usize = 128;
const MAX_DISCOVERY_MONTH_PAGES: usize = MAX_DISCOVERY_YEAR_PAGES * 12;

fn check_year_bound(journal: &str, count: usize) -> Result<()> {
    if count > MAX_DISCOVERY_YEAR_PAGES {
        return Err(discovery_error(
            journal,
            format!(
                "archive navigation lists {count} unique year pages, \
                 exceeding the {MAX_DISCOVERY_YEAR_PAGES}-page bound"
            ),
        ));
    }
    Ok(())
}

fn check_month_bound(journal: &str, count: usize) -> Result<()> {
    if count > MAX_DISCOVERY_MONTH_PAGES {
        return Err(discovery_error(
            journal,
            format!(
                "archive navigation lists {count} unique month pages, \
                 exceeding the {MAX_DISCOVERY_MONTH_PAGES}-page bound"
            ),
        ));
    }
    Ok(())
}

pub fn journal_calendar_url(journal: &str, base_url: &Url) -> Result<Url> {
    validate_journal(journal)?;

    if is_production_base_url(base_url) {
        let calendar_url = if journal.contains('_') {
            format!("https://users.livejournal.com/{journal}/calendar")
        } else {
            format!("https://{journal}.livejournal.com/calendar")
        };
        Url::parse(&calendar_url).map_err(|error| discovery_error(journal, error.to_string()))
    } else {
        base_url
            .join(&format!("{journal}/calendar"))
            .map_err(|error| discovery_error(journal, error.to_string()))
    }
}

/// LiveJournal's documented archive navigation is static and path based, so a
/// query or fragment on a year/month link addresses the same crawl page. Strip
/// both *before* deduplication, or one page is fetched once per decorated
/// spelling. Post URLs are unaffected — only archive navigation is canonicalized.
fn canonical_archive_url(mut url: Url) -> Url {
    url.set_query(None);
    url.set_fragment(None);
    url
}

fn dedup_archive_urls(urls: &mut Vec<Url>) {
    urls.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    urls.dedup();
}

fn collect_archive_links(
    html: &str,
    base_url: &Url,
    journal: &str,
    is_expected: impl Fn(&str, &Url, &str) -> bool,
) -> Vec<Url> {
    let document = Html::parse_document(html);
    let mut urls: Vec<_> = document
        .select(&ANCHOR_HREF)
        .filter_map(|element| element.value().attr("href"))
        .filter_map(|href| base_url.join(href).ok())
        .map(canonical_archive_url)
        .filter(|url| {
            if crate::net::is_loopback_test_server(base_url) {
                same_origin(url, base_url)
            } else {
                is_journal_archive_url(url, journal)
            }
        })
        .filter(|url| is_expected(url.path(), url, journal))
        .collect();

    dedup_archive_urls(&mut urls);
    urls
}

pub fn parse_month_links(html: &str, base_url: &Url, journal: &str) -> Vec<Url> {
    collect_archive_links(html, base_url, journal, is_expected_month_path)
}

pub fn parse_year_links(html: &str, base_url: &Url, journal: &str) -> Vec<Url> {
    collect_archive_links(html, base_url, journal, is_expected_year_path)
}

pub fn parse_post_ids(html: &str, base_url: &Url, journal: &str) -> Vec<u64> {
    let document = Html::parse_document(html);
    let mut ids: Vec<_> = document
        .select(&ANCHOR_HREF)
        .filter_map(|element| element.value().attr("href"))
        .filter_map(|href| base_url.join(href).ok())
        .filter_map(|url| post_id_from_url(&url, base_url, journal))
        .collect();

    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Discovers post IDs by walking the journal's calendar, its per-year pages,
/// and the month pages linked from them.
///
/// The calendar links months directly for the current year (and, on small or
/// legacy journals, for every year), plus one link per year. Older years are
/// reached by fetching each year page and reading its month links. Year links
/// are read from the calendar only — year pages re-expose the whole year nav,
/// so recursing would re-crawl every year.
///
/// Returns the discovered post IDs and the number of archive pages (year or
/// month) that failed to fetch (0 when every attempted page succeeded). A page
/// failure is surfaced as `Err` only when it left discovery with no month pages
/// to read at all, or when every attempted month page failed; otherwise the
/// failure count is returned so callers can still exit nonzero on a partial
/// discovery failure.
pub async fn discover_post_ids(
    client: &LjClient,
    journal: &str,
    base_url: &Url,
    progress: &Progress,
) -> Result<(Vec<u64>, usize)> {
    let calendar_url = journal_calendar_url(journal, base_url)?;
    let calendar_page = client.fetch_page(&calendar_url).await?;

    let mut month_urls = parse_month_links(&calendar_page.text, &calendar_page.final_url, journal);
    let year_urls = parse_year_links(&calendar_page.text, &calendar_page.final_url, journal);

    // Checked after the calendar parse, before a single year page is fetched.
    check_year_bound(journal, year_urls.len())?;
    dedup_archive_urls(&mut month_urls);
    check_month_bound(journal, month_urls.len())?;

    let mut ids = HashSet::new();
    let mut year_failed = 0usize;
    for year_url in year_urls {
        match client.fetch_page(&year_url).await {
            Ok(year_page) => {
                month_urls.extend(parse_month_links(
                    &year_page.text,
                    &year_page.final_url,
                    journal,
                ));
                // Checked after each successful merge, so an overflowing walk
                // stops at the year that caused it rather than after the last.
                dedup_archive_urls(&mut month_urls);
                check_month_bound(journal, month_urls.len())?;
                progress.discovery_tick(year_page.final_url.path(), ids.len());
            }
            Err(error) => {
                year_failed += 1;
                tracing::warn!(url = %year_url, %error, "skipping year page after fetch failure");
            }
        }
    }

    if month_urls.is_empty() {
        if year_failed > 0 {
            return Err(discovery_error(
                journal,
                "all archive pages failed to fetch",
            ));
        }
        return Ok((Vec::new(), 0));
    }

    let mut attempted = 0usize;
    let mut month_failed = 0usize;
    for month_url in month_urls {
        attempted += 1;
        match client.fetch_page(&month_url).await {
            Ok(month_page) => {
                ids.extend(parse_post_ids(
                    &month_page.text,
                    &month_page.final_url,
                    journal,
                ));
                progress.discovery_tick(month_page.final_url.path(), ids.len());
            }
            Err(error) => {
                month_failed += 1;
                tracing::warn!(url = %month_url, %error, "skipping month page after fetch failure");
            }
        }
    }
    if month_failed == attempted {
        return Err(discovery_error(journal, "all month pages failed to fetch"));
    }

    let mut ids: Vec<_> = ids.into_iter().collect();
    ids.sort_unstable();
    Ok((ids, year_failed + month_failed))
}

fn is_production_base_url(base_url: &Url) -> bool {
    base_url.scheme() == "https"
        && crate::net::normalized_domain_host(base_url)
            .is_some_and(|host| host.eq_ignore_ascii_case("livejournal.com"))
}

fn validate_journal(journal: &str) -> Result<()> {
    if crate::target::is_valid_journal_name(journal) {
        Ok(())
    } else {
        Err(discovery_error(
            journal,
            "journal name contains unsupported characters",
        ))
    }
}

fn is_month_path(path: &str) -> bool {
    // Matches only the observed LiveJournal shape: /YYYY/MM/ (9 bytes total),
    // with MM an actual calendar month. is_expected_month_path extends this to
    // the users.livejournal.com prefix form (/journal/YYYY/MM/).
    let bytes = path.as_bytes();
    if bytes.len() != 9
        || bytes[0] != b'/'
        || bytes[5] != b'/'
        || bytes[8] != b'/'
        || !bytes[1..5].iter().all(|byte| byte.is_ascii_digit())
        || !bytes[6..8].iter().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }

    matches!(path[6..8].parse::<u32>(), Ok(1..=12))
}

fn is_expected_month_path(path: &str, url: &Url, journal: &str) -> bool {
    if is_month_path(path) {
        return true;
    }
    // users.livejournal.com links month/year pages under a /<journal> prefix, on
    // both the calendar and the per-year pages. The journal is known by the caller
    // (like parse_post_ids), so strip it directly rather than requiring a /calendar base.
    if is_users_livejournal_url(url) {
        return strip_users_journal_prefix(path, journal).is_some_and(is_month_path);
    }
    false
}

fn is_year_path(path: &str) -> bool {
    // Matches only the observed LiveJournal shape: /YYYY/ (6 bytes total).
    let bytes = path.as_bytes();
    bytes.len() == 6
        && bytes[0] == b'/'
        && bytes[5] == b'/'
        && bytes[1..5].iter().all(|byte| byte.is_ascii_digit())
}

fn is_expected_year_path(path: &str, url: &Url, journal: &str) -> bool {
    if is_year_path(path) {
        return true;
    }
    if is_users_livejournal_url(url) {
        return strip_users_journal_prefix(path, journal).is_some_and(is_year_path);
    }
    false
}

fn post_id_from_url(url: &Url, base_url: &Url, journal: &str) -> Option<u64> {
    if is_journal_archive_url(url, journal) {
        if is_users_livejournal_url(url) {
            strip_users_journal_prefix(url.path(), journal).and_then(id_from_root_post_path)
        } else {
            id_from_root_post_path(url.path())
        }
    } else if crate::net::is_loopback_test_server(base_url) && same_origin(url, base_url) {
        id_from_test_server_post_path(url.path(), journal)
    } else {
        None
    }
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && hosts_equal(left, right)
        && left.port_or_known_default() == right.port_or_known_default()
}

fn hosts_equal(left: &Url, right: &Url) -> bool {
    match (left.host(), right.host()) {
        (Some(Host::Domain(_)), Some(Host::Domain(_))) => {
            match (
                crate::net::normalized_domain_host(left),
                crate::net::normalized_domain_host(right),
            ) {
                (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
                _ => false,
            }
        }
        (Some(Host::Ipv4(left)), Some(Host::Ipv4(right))) => left == right,
        (Some(Host::Ipv6(left)), Some(Host::Ipv6(right))) => left == right,
        _ => false,
    }
}

fn is_users_livejournal_url(url: &Url) -> bool {
    crate::net::normalized_domain_host(url)
        .is_some_and(|host| host.eq_ignore_ascii_case("users.livejournal.com"))
}

fn journal_from_subdomain(url: &Url) -> Option<&str> {
    let host = crate::net::normalized_domain_host(url)?;
    let journal = host.strip_suffix(".livejournal.com")?;
    (!journal.is_empty() && !journal.contains('.')).then_some(journal)
}

fn is_journal_archive_url(url: &Url, journal: &str) -> bool {
    if url.scheme() != "https" || url.port_or_known_default() != Some(443) {
        return false;
    }

    if is_users_livejournal_url(url) {
        return strip_users_journal_prefix(url.path(), journal).is_some();
    }

    journal_from_subdomain(url).is_some_and(|subdomain| {
        !subdomain.eq_ignore_ascii_case("www") && journals_alias(subdomain, journal)
    })
}

fn strip_users_journal_prefix<'a>(path: &'a str, journal: &str) -> Option<&'a str> {
    let path_without_leading_slash = path.strip_prefix('/')?;
    let segment_end = path_without_leading_slash.find('/')?;
    let path_journal = &path_without_leading_slash[..segment_end];

    journals_alias(path_journal, journal).then_some(&path_without_leading_slash[segment_end..])
}

fn id_from_root_post_path(path: &str) -> Option<u64> {
    let id = path.strip_prefix('/')?.strip_suffix(".html")?;
    parse_positive_id(id)
}

fn id_from_users_post_path(path: &str, journal: &str) -> Option<u64> {
    strip_users_journal_prefix(path, journal).and_then(id_from_root_post_path)
}

fn id_from_test_server_post_path(path: &str, journal: &str) -> Option<u64> {
    id_from_users_post_path(path, journal).or_else(|| id_from_root_post_path(path))
}

fn parse_positive_id(value: &str) -> Option<u64> {
    if value.is_empty() || value.contains('/') {
        return None;
    }

    let id = value.parse::<u64>().ok()?;
    (id > 0).then_some(id)
}

fn discovery_error(journal: &str, reason: impl Into<String>) -> LjError {
    LjError::Discovery {
        journal: journal.to_string(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subdomain_base() -> Url {
        Url::parse("https://news.livejournal.com/calendar").unwrap()
    }

    fn users_base() -> Url {
        Url::parse("https://users.livejournal.com/example_user/calendar").unwrap()
    }

    #[test]
    fn month_links_drop_query_and_fragment_before_deduplication() {
        let html = concat!(
            r#"<a href="/2020/01/">a</a>"#,
            r#"<a href="/2020/01/?skip=20">b</a>"#,
            r#"<a href="/2020/01/#top">c</a>"#,
            r#"<a href="/2020/01/?skip=20#top">d</a>"#,
        );

        let months = parse_month_links(html, &subdomain_base(), "news");
        assert_eq!(
            months.iter().map(|url| url.as_str()).collect::<Vec<_>>(),
            ["https://news.livejournal.com/2020/01/"]
        );
    }

    #[test]
    fn year_links_drop_query_and_fragment_for_both_url_schemes() {
        let subdomain = parse_year_links(
            r#"<a href="/2020/">a</a><a href="/2020/?x=1#f">b</a>"#,
            &subdomain_base(),
            "news",
        );
        assert_eq!(
            subdomain.iter().map(Url::as_str).collect::<Vec<_>>(),
            ["https://news.livejournal.com/2020/"]
        );

        let users = parse_year_links(
            concat!(
                r#"<a href="/example_user/2020/">a</a>"#,
                r#"<a href="/example_user/2020/?x=1#f">b</a>"#,
            ),
            &users_base(),
            "example_user",
        );
        assert_eq!(
            users.iter().map(Url::as_str).collect::<Vec<_>>(),
            ["https://users.livejournal.com/example_user/2020/"]
        );
    }

    #[test]
    fn month_links_require_a_real_calendar_month_for_both_url_schemes() {
        let invalid = concat!(
            r#"<a href="/2020/00/">a</a>"#,
            r#"<a href="/2020/13/">b</a>"#,
            r#"<a href="/2020/99/">c</a>"#,
        );
        assert!(parse_month_links(invalid, &subdomain_base(), "news").is_empty());

        let users_invalid = concat!(
            r#"<a href="/example_user/2020/00/">a</a>"#,
            r#"<a href="/example_user/2020/13/">b</a>"#,
        );
        assert!(parse_month_links(users_invalid, &users_base(), "example_user").is_empty());

        let valid = r#"<a href="/2020/01/">a</a><a href="/2020/12/">b</a>"#;
        assert_eq!(parse_month_links(valid, &subdomain_base(), "news").len(), 2);
    }

    #[test]
    fn discovery_bounds_are_exact() {
        assert!(check_year_bound("news", MAX_DISCOVERY_YEAR_PAGES).is_ok());
        let error = check_year_bound("news", MAX_DISCOVERY_YEAR_PAGES + 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("128"), "{error}");

        assert!(check_month_bound("news", MAX_DISCOVERY_MONTH_PAGES).is_ok());
        let error = check_month_bound("news", MAX_DISCOVERY_MONTH_PAGES + 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("1536") || error.contains("1,536"), "{error}");
    }

    #[test]
    fn parses_month_links_from_calendar() {
        let base = Url::parse("https://news.livejournal.com/calendar").unwrap();
        let months = parse_month_links(
            include_str!("../tests/fixtures/journal_calendar.html"),
            &base,
            "news",
        );
        assert_eq!(
            months,
            vec![
                Url::parse("https://news.livejournal.com/2024/12/").unwrap(),
                Url::parse("https://news.livejournal.com/2025/01/").unwrap(),
            ]
        );
    }

    #[test]
    fn parses_post_ids_from_month() {
        let base = Url::parse("https://news.livejournal.com/2024/12/").unwrap();
        let ids = parse_post_ids(
            include_str!("../tests/fixtures/journal_month.html"),
            &base,
            "news",
        );
        assert_eq!(ids, vec![166511, 166512]);
    }

    #[test]
    fn builds_production_calendar_url() {
        let base = Url::parse("https://livejournal.com").unwrap();
        let url = journal_calendar_url("news", &base).unwrap();
        assert_eq!(url.as_str(), "https://news.livejournal.com/calendar");
    }

    #[test]
    fn builds_production_calendar_url_for_underscore_journal() {
        let base = Url::parse("https://livejournal.com").unwrap();
        let url = journal_calendar_url("example_user", &base).unwrap();
        assert_eq!(
            url.as_str(),
            "https://users.livejournal.com/example_user/calendar"
        );
    }

    #[test]
    fn dotted_production_root_builds_calendar_urls_for_both_journal_schemes() {
        let base = Url::parse("https://livejournal.com.").unwrap();

        assert_eq!(
            journal_calendar_url("news", &base).unwrap().as_str(),
            "https://news.livejournal.com/calendar"
        );
        assert_eq!(
            journal_calendar_url("example_user", &base)
                .unwrap()
                .as_str(),
            "https://users.livejournal.com/example_user/calendar"
        );
    }

    #[test]
    fn dotted_subdomain_archive_accepts_calendar_year_month_and_post_links() {
        let calendar = Url::parse("https://news.livejournal.com./calendar").unwrap();
        let year_html = r#"<a href="https://news.livejournal.com./2024/">year</a>"#;
        let month_html = r#"<a href="https://news.livejournal.com./2024/12/">month</a>"#;
        let post_html = r#"<a href="https://news.livejournal.com./1.html">post</a>"#;

        assert_eq!(parse_year_links(year_html, &calendar, "news").len(), 1);
        assert_eq!(parse_month_links(month_html, &calendar, "news").len(), 1);
        assert_eq!(parse_post_ids(post_html, &calendar, "news"), [1]);
    }

    #[test]
    fn dotted_users_archive_accepts_calendar_year_month_and_post_links() {
        let calendar = Url::parse("https://users.livejournal.com./example_user/calendar").unwrap();
        let year_html = r#"<a href="https://users.livejournal.com./example_user/2024/">year</a>"#;
        let month_html =
            r#"<a href="https://users.livejournal.com./example_user/2024/12/">month</a>"#;
        let post_html = r#"<a href="https://users.livejournal.com./example_user/1.html">post</a>"#;

        assert_eq!(
            parse_year_links(year_html, &calendar, "example_user").len(),
            1
        );
        assert_eq!(
            parse_month_links(month_html, &calendar, "example_user").len(),
            1
        );
        assert_eq!(parse_post_ids(post_html, &calendar, "example_user"), [1]);
    }

    #[test]
    fn same_origin_normalizes_dns_root_dots_but_keeps_ip_identity() {
        let plain = Url::parse("https://news.livejournal.com/calendar").unwrap();
        let dotted = Url::parse("https://news.livejournal.com./2024/").unwrap();
        let ipv4 = Url::parse("https://127.0.0.1:443/").unwrap();

        assert!(same_origin(&plain, &dotted));
        assert!(!same_origin(&plain, &ipv4));
    }

    #[test]
    fn builds_test_server_calendar_url() {
        let base = Url::parse("http://127.0.0.1:4567").unwrap();
        let url = journal_calendar_url("news", &base).unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:4567/news/calendar");
    }

    #[test]
    fn ignores_off_journal_month_links() {
        let base = Url::parse("https://news.livejournal.com/calendar").unwrap();
        let months = parse_month_links(
            r#"<a href="https://elsewhere.example/2024/12/">x</a>"#,
            &base,
            "news",
        );
        assert!(months.is_empty());
    }

    #[test]
    fn parses_month_links_from_underscore_journal_calendar() {
        let base = Url::parse("https://users.livejournal.com/example_user/calendar").unwrap();
        let months = parse_month_links(
            r#"<a href="/example_user/2024/12/">December</a>"#,
            &base,
            "example_user",
        );
        assert_eq!(
            months,
            vec![Url::parse("https://users.livejournal.com/example_user/2024/12/").unwrap()]
        );
    }

    #[test]
    fn parses_month_links_from_underscore_journal_year_page() {
        // Regression (final-review Important #1): on a users.livejournal.com journal the
        // month links are the prefixed form /<journal>/YYYY/MM/ and appear on YEAR pages
        // whose base path is /<journal>/YYYY/ (NOT /calendar). Discovery must still extract
        // them — previously they were dropped because the journal was recoverable only from
        // a /calendar base, so underscore journals silently archived only the current year.
        let base = Url::parse("https://users.livejournal.com/example_user/2012/").unwrap();
        let months = parse_month_links(
            r#"<a href="/example_user/2012/07/">July</a>"#,
            &base,
            "example_user",
        );
        assert_eq!(
            months,
            vec![Url::parse("https://users.livejournal.com/example_user/2012/07/").unwrap()]
        );
    }

    #[test]
    fn ignores_test_server_post_paths_on_production_hosts() {
        let base = Url::parse("https://elsewhere.example/2024/12/").unwrap();
        let ids = parse_post_ids(r#"<a href="/news/166512.html">post</a>"#, &base, "news");
        assert!(ids.is_empty());
    }

    #[test]
    fn parses_post_ids_from_users_journal_month() {
        let base = Url::parse("https://users.livejournal.com/example_user/2024/12/").unwrap();
        let ids = parse_post_ids(
            r#"<a href="/example_user/166512.html">post</a>"#,
            &base,
            "example_user",
        );
        assert_eq!(ids, vec![166512]);
    }

    #[test]
    fn accepts_test_server_post_paths_on_loopback_hosts() {
        let base = Url::parse("http://127.0.0.1:4567/2024/12/").unwrap();
        let ids = parse_post_ids(r#"<a href="/news/166512.html">post</a>"#, &base, "news");
        assert_eq!(ids, vec![166512]);
    }

    #[test]
    fn accepts_root_post_paths_on_loopback_hosts() {
        let base = Url::parse("http://127.0.0.1:4567/2024/12/").unwrap();
        let ids = parse_post_ids(r#"<a href="/166511.html">post</a>"#, &base, "news");
        assert_eq!(ids, vec![166511]);
    }

    #[tokio::test]
    async fn discovers_post_ids_from_calendar_months() {
        use crate::client::LjClient;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a href="/2024/12/">December 2024</a><a href="/2025/01/">January 2025</a>"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a href="/news/166512.html">two</a><a href="/news/166511.html">one</a>"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2025/01/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a href="/news/166512.html">duplicate</a><a href="/profile">Profile</a>"#,
            ))
            .mount(&server)
            .await;

        let client = LjClient::new(None).unwrap();
        let base = Url::parse(&server.uri()).unwrap();
        let (ids, failed_months) = discover_post_ids(&client, "news", &base, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(ids, vec![166511, 166512]);
        assert_eq!(failed_months, 0);
    }

    #[tokio::test]
    async fn discovery_tick_counts_unique_posts_monotonically_across_pages() {
        use crate::{client::LjClient, progress::ProgressEvent};
        use futures::StreamExt;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"
                    <a href="/2023/">2023</a>
                    <a href="/2024/">2024</a>
                    <a href="/2024/03/">March</a>
                    <a href="/2024/03/">March duplicate</a>
                "#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2023/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"
                    <a href="/2023/01/">January</a>
                    <a href="/2023/02/">February</a>
                    <a href="/2023/02/">February duplicate</a>
                "#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"
                    <a href="/2024/03/">March repeated from calendar</a>
                    <a href="/2024/04/">April</a>
                "#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2023/01/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"
                    <a href="/news/30.html">thirty</a>
                    <a href="/news/10.html">ten</a>
                    <a href="/news/10.html">ten duplicate</a>
                "#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2023/02/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"
                    <a href="/news/30.html">thirty repeated</a>
                    <a href="/news/20.html">twenty</a>
                "#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/03/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"
                    <a href="/news/20.html">twenty repeated</a>
                    <a href="/news/40.html">forty</a>
                    <a href="/news/40.html">forty duplicate</a>
                "#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/04/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = LjClient::new(None).unwrap();
        let base = Url::parse(&server.uri()).unwrap();
        let (tx, rx) = futures::channel::mpsc::channel(8);
        let progress = Progress::channel(tx);
        let (ids, failed) = discover_post_ids(&client, "news", &base, &progress)
            .await
            .unwrap();
        drop(progress);
        let events: Vec<_> = rx.collect().await;

        let expected_ticks = vec![
            ProgressEvent::DiscoveryTick {
                label: "/2023/".to_string(),
                found: 0,
            },
            ProgressEvent::DiscoveryTick {
                label: "/2024/".to_string(),
                found: 0,
            },
            ProgressEvent::DiscoveryTick {
                label: "/2023/01/".to_string(),
                found: 2,
            },
            ProgressEvent::DiscoveryTick {
                label: "/2023/02/".to_string(),
                found: 3,
            },
            ProgressEvent::DiscoveryTick {
                label: "/2024/03/".to_string(),
                found: 4,
            },
        ];
        assert_eq!(events, expected_ticks);
        let found: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ProgressEvent::DiscoveryTick { found, .. } => Some(*found),
                _ => None,
            })
            .collect();
        assert!(found.windows(2).all(|counts| counts[0] <= counts[1]));
        assert_eq!(found.last().copied(), Some(ids.len()));
        assert_eq!(ids, vec![10, 20, 30, 40]);
        assert_eq!(failed, 1);
    }

    #[tokio::test]
    async fn discovery_continues_past_failed_month() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<a href="/2024/12/">Dec</a><a href="/2025/01/">Jan</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<a href="/news/166511.html">one</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2025/01/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let client = LjClient::new(None).unwrap();
        let base = Url::parse(&server.uri()).unwrap();
        let (ids, failed_months) = discover_post_ids(&client, "news", &base, &Progress::hidden())
            .await
            .unwrap();
        assert_eq!(ids, vec![166511]);
        assert_eq!(failed_months, 1);
    }

    #[tokio::test]
    async fn errors_when_all_month_pages_fail() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<a href="/2024/12/">Dec</a><a href="/2025/01/">Jan</a>"#),
            )
            .mount(&server)
            .await;
        // Both month pages fail => total failure.
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2025/01/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let client = LjClient::new(None).unwrap();
        let base = Url::parse(&server.uri()).unwrap();
        let result = discover_post_ids(&client, "news", &base, &Progress::hidden()).await;
        assert!(
            matches!(result, Err(LjError::Discovery { .. })),
            "all month pages failing must surface as a total-failure error, not Ok(empty)"
        );
    }

    #[test]
    fn is_year_path_matches_only_bare_year() {
        assert!(is_year_path("/2024/"));
        assert!(!is_year_path("/2024/12/"));
        assert!(!is_year_path("/abc/"));
        assert!(!is_year_path("/24/"));
        assert!(!is_year_path("/2024"));
    }

    #[test]
    fn parses_year_links_from_calendar() {
        let base = Url::parse("https://news.livejournal.com/calendar").unwrap();
        let years = parse_year_links(
            r#"<a href="/2011/">2011</a><a href="/2012/">2012</a><a href="/2012/07/">July</a>"#,
            &base,
            "news",
        );
        assert_eq!(
            years,
            vec![
                Url::parse("https://news.livejournal.com/2011/").unwrap(),
                Url::parse("https://news.livejournal.com/2012/").unwrap(),
            ]
        );
    }

    #[test]
    fn parse_year_links_ignores_month_and_offsite_links() {
        let base = Url::parse("https://news.livejournal.com/calendar").unwrap();
        let years = parse_year_links(
            r#"<a href="/2012/07/">month</a><a href="https://elsewhere.example/2011/">off</a>"#,
            &base,
            "news",
        );
        assert!(years.is_empty());
    }

    #[test]
    fn parses_year_links_from_underscore_journal_calendar() {
        let base = Url::parse("https://users.livejournal.com/example_user/calendar").unwrap();
        let years = parse_year_links(
            r#"<a href="/example_user/2012/">2012</a>"#,
            &base,
            "example_user",
        );
        assert_eq!(
            years,
            vec![Url::parse("https://users.livejournal.com/example_user/2012/").unwrap()]
        );
    }

    #[test]
    fn parses_years_and_current_months_from_real_shape_calendar() {
        let base = Url::parse("https://example.livejournal.com/calendar").unwrap();
        let html = include_str!("../tests/fixtures/journal_calendar_years.html");
        let years = parse_year_links(html, &base, "example");
        let months = parse_month_links(html, &base, "example");
        assert_eq!(
            years,
            vec![
                Url::parse("https://example.livejournal.com/2011/").unwrap(),
                Url::parse("https://example.livejournal.com/2012/").unwrap(),
                Url::parse("https://example.livejournal.com/2013/").unwrap(),
            ]
        );
        assert_eq!(
            months,
            vec![
                Url::parse("https://example.livejournal.com/2013/06/").unwrap(),
                Url::parse("https://example.livejournal.com/2013/07/").unwrap(),
            ]
        );
    }

    #[test]
    fn parses_redirected_underscore_journal_links() {
        let calendar = Url::parse("https://evo-lutio.livejournal.com/calendar/").unwrap();
        let calendar_html = r#"
            <a href="https://evo-lutio.livejournal.com/2024/">2024</a>
            <a href="https://evo-lutio.livejournal.com/2025/01/">January 2025</a>
            <a href="https://other-journal.livejournal.com/2023/">other journal</a>
            <a href="https://other-journal.livejournal.com/2023/12/">other month</a>
        "#;

        assert_eq!(
            parse_year_links(calendar_html, &calendar, "evo_lutio"),
            vec![Url::parse("https://evo-lutio.livejournal.com/2024/").unwrap()]
        );
        assert_eq!(
            parse_month_links(calendar_html, &calendar, "evo_lutio"),
            vec![Url::parse("https://evo-lutio.livejournal.com/2025/01/").unwrap()]
        );

        let month = Url::parse("https://evo-lutio.livejournal.com/2025/01/").unwrap();
        let month_html = r#"
            <a href="https://evo-lutio.livejournal.com/240331.html">post</a>
            <a href="https://other-journal.livejournal.com/999999.html">other journal</a>
        "#;
        assert_eq!(
            parse_post_ids(month_html, &month, "evo_lutio"),
            vec![240331]
        );
    }

    #[test]
    fn accepts_users_archive_link_from_subdomain_page() {
        let page = Url::parse("https://evo-lutio.livejournal.com/calendar/").unwrap();
        let html = r#"
            <a href="https://users.livejournal.com/evo_lutio/2025/01/">January 2025</a>
        "#;

        assert_eq!(
            parse_month_links(html, &page, "evo_lutio"),
            vec![Url::parse("https://users.livejournal.com/evo_lutio/2025/01/").unwrap()]
        );
    }

    #[test]
    fn rejects_prefixed_subdomain_archive_link_from_users_page() {
        let page = Url::parse("https://users.livejournal.com/evo_lutio/calendar/").unwrap();
        let html = r#"
            <a href="https://evo-lutio.livejournal.com/evo_lutio/2025/01/">invalid</a>
        "#;

        assert!(parse_month_links(html, &page, "evo_lutio").is_empty());
    }

    #[tokio::test]
    async fn discovers_from_final_redirect_urls() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let configured_server = MockServer::start().await;
        let final_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/evo_lutio/calendar"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/calendar/", final_server.uri())),
            )
            .mount(&configured_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/calendar/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"<a href="{0}/2024/">2024</a><a href="{0}/2025/01/">January 2025</a>"#,
                final_server.uri()
            )))
            .mount(&final_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"<a href="{}/2024/12/">December 2024</a>"#,
                final_server.uri()
            )))
            .mount(&final_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<a href="/evo_lutio/240330.html">older</a>"#),
            )
            .mount(&final_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2025/01/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<a href="/evo_lutio/240331.html">newer</a>"#),
            )
            .mount(&final_server)
            .await;

        let client = LjClient::new(None).unwrap();
        let configured_base = Url::parse(&configured_server.uri()).unwrap();
        let (ids, failed) =
            discover_post_ids(&client, "evo_lutio", &configured_base, &Progress::hidden())
                .await
                .unwrap();

        assert_eq!(ids, vec![240330, 240331]);
        assert_eq!(failed, 0);
    }

    #[tokio::test]
    async fn discovers_post_ids_across_year_pages() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        // Calendar: two YEAR links + one current-year MONTH link (also under /2012/).
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a href="/2011/">2011</a><a href="/2012/">2012</a><a href="/2012/07/">Jul</a>"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2011/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2011/03/">Mar</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2012/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2012/07/">Jul</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2011/03/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/news/111.html">a</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2012/07/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/news/222.html">b</a>"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = LjClient::new(None).unwrap();
        let base = Url::parse(&server.uri()).unwrap();
        let (ids, failed) = discover_post_ids(&client, "news", &base, &Progress::hidden())
            .await
            .unwrap();

        // /2012/07/ is linked both directly and via the year page: deduped, fetched once.
        assert_eq!(ids, vec![111, 222]);
        assert_eq!(failed, 0);
    }

    #[tokio::test]
    async fn discovery_continues_past_failed_year_page() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<a href="/2011/">2011</a><a href="/2012/">2012</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2011/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2012/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2012/07/">Jul</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2012/07/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/news/222.html">b</a>"#),
            )
            .mount(&server)
            .await;

        let client = LjClient::new(None).unwrap();
        let base = Url::parse(&server.uri()).unwrap();
        let (ids, failed) = discover_post_ids(&client, "news", &base, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(ids, vec![222]);
        assert_eq!(failed, 1);
    }

    #[tokio::test]
    async fn errors_when_all_year_pages_fail_with_no_direct_months() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        // Calendar has only year links (no direct month links); every year page fails.
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<a href="/2011/">2011</a><a href="/2012/">2012</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2011/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2012/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = LjClient::new(None).unwrap();
        let base = Url::parse(&server.uri()).unwrap();
        let result = discover_post_ids(&client, "news", &base, &Progress::hidden()).await;

        assert!(
            matches!(result, Err(LjError::Discovery { .. })),
            "reaching no month pages because all year pages failed must be a discovery error"
        );
    }

    #[tokio::test]
    async fn a_calendar_past_the_year_bound_fails_before_fetching_year_pages() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path, path_regex},
        };

        let server = MockServer::start().await;
        let links = (0..=MAX_DISCOVERY_YEAR_PAGES)
            .map(|offset| format!(r#"<a href="/{}/">y</a>"#, 1900 + offset))
            .collect::<String>();
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(ResponseTemplate::new(200).set_body_string(links))
            .expect(1)
            .mount(&server)
            .await;
        // No mock answers a year-page path (`/YYYY/`); this one exists only to
        // turn "the walk never fetches a year page" into a checked expectation
        // instead of an unmocked request that would merely 404 silently.
        Mock::given(method("GET"))
            .and(path_regex(r"^/\d{4}/$"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let client = LjClient::new(None).unwrap();
        let base = Url::parse(&server.uri()).unwrap();
        let error = discover_post_ids(&client, "news", &base, &Progress::hidden())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("128"), "{error}");
    }
}
