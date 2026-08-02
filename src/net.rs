use std::sync::Arc;

use reqwest::cookie::Jar;
use url::{Host, Url};

/// Returns a DNS host without its optional root-label dot. IP literals remain
/// distinguishable through `Url::host()` and are deliberately not returned.
pub(crate) fn normalized_domain_host(url: &Url) -> Option<&str> {
    match url.host()? {
        Host::Domain(host) => Some(host.strip_suffix('.').unwrap_or(host)),
        Host::Ipv4(_) | Host::Ipv6(_) => None,
    }
}

/// A loopback HTTP test server: `http` scheme, an explicit port, and a
/// loopback host (`localhost` or any loopback IP literal). Never matches a
/// production `https://*.livejournal.com` base URL.
///
/// Uses `Url::host()` (a pre-parsed `Host`) rather than `Url::host_str()` +
/// `str::parse::<IpAddr>()`: the `url` crate renders IPv6 hosts in
/// `host_str()` wrapped in brackets (e.g. `"[::1]"`), which `IpAddr::parse`
/// rejects, silently under-matching every IPv6 loopback literal.
pub(crate) fn is_loopback_test_server(url: &Url) -> bool {
    if url.scheme() != "http" || url.port().is_none() {
        return false;
    }
    match url.host() {
        Some(Host::Domain(_)) => {
            normalized_domain_host(url).is_some_and(|host| host.eq_ignore_ascii_case("localhost"))
        }
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// A reqwest cookie jar pre-seeded with LiveJournal's age-affirmation cookies,
/// scoped to `livejournal.com`. Both production clients install it via
/// `reqwest::ClientBuilder::cookie_provider`.
///
/// A jar rather than a static `Cookie` header: reqwest strips a manually-set
/// `Cookie` when a redirect crosses to a different host, and underscore
/// journals are fetched via `users.livejournal.com/<journal>/<id>.html`, which
/// 301-redirects to `<journal>.livejournal.com` — a cross-host hop that would
/// drop the affirmation and re-gate the post. A domain-scoped jar is
/// re-consulted per hop, so the cookie survives the redirect; the
/// `livejournal.com` scope also keeps it off non-LiveJournal hosts (e.g.
/// third-party media).
///
/// `adult_explicit=1` (18+) is the value LiveJournal sets after the
/// interstitial's confirmation (verified on the wire); `adult_concepts=1` is a
/// harmless precaution for the legacy 14+ tier. Both are static and tokenless.
pub(crate) fn affirmation_cookie_jar() -> Arc<Jar> {
    let jar = Jar::default();
    let lj = Url::parse("https://www.livejournal.com/").expect("static url is valid");
    jar.add_cookie_str("adult_explicit=1; Domain=livejournal.com; Path=/", &lj);
    jar.add_cookie_str("adult_concepts=1; Domain=livejournal.com; Path=/", &lj);
    Arc::new(jar)
}

/// A `reqwest::ClientBuilder` with the shared production HTTP configuration
/// every LiveJournal client needs: the age-affirmation cookie jar and the
/// shared rustls config, plus a `User-Agent` header when `user_agent` is
/// `Some`. Callers add their own timeout and redirect policy. With `None`
/// (the default) no `User-Agent` is sent.
pub(crate) fn configured_client_builder(user_agent: Option<&str>) -> reqwest::ClientBuilder {
    let mut builder = reqwest::Client::builder()
        .cookie_provider(affirmation_cookie_jar())
        .use_preconfigured_tls(crate::tls::rustls_config());
    if let Some(user_agent) = user_agent {
        builder = builder.user_agent(user_agent);
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_loopback_http_with_port() {
        for u in [
            "http://127.0.0.1:8080/",
            "http://localhost:8080/",
            "http://[::1]:8080/",
        ] {
            assert!(is_loopback_test_server(&Url::parse(u).unwrap()), "{u}");
        }
    }

    #[test]
    fn matches_any_loopback_ip_and_case_insensitive_localhost() {
        // Breadth the D2 safety argument relies on: ANY loopback IPv4 (not just
        // 127.0.0.1) and a case-varied "localhost" both count as a loopback test
        // server.
        for u in ["http://127.4.5.6:8080/", "http://LocalHost:8080/"] {
            assert!(is_loopback_test_server(&Url::parse(u).unwrap()), "{u}");
        }
    }

    #[test]
    fn matches_dns_trailing_dot_localhost() {
        let url = Url::parse("http://localhost.:8080/").unwrap();

        assert!(is_loopback_test_server(&url));
    }

    #[test]
    fn rejects_production_and_portless_and_https() {
        for u in [
            "https://news.livejournal.com/", // production
            "http://127.0.0.1/",             // no port
            "https://127.0.0.1:8080/",       // https
            "http://example.com:8080/",      // not loopback
        ] {
            assert!(!is_loopback_test_server(&Url::parse(u).unwrap()), "{u}");
        }
    }

    #[test]
    fn affirmation_jar_serves_cookie_across_livejournal_subdomains_only() {
        use reqwest::cookie::CookieStore;

        let jar = affirmation_cookie_jar();

        // Sent to every livejournal.com subdomain — including BOTH sides of the
        // users.livejournal.com -> <journal>.livejournal.com redirect that would
        // otherwise strip a manually-set Cookie header on the cross-host hop.
        for url in [
            "https://news.livejournal.com/166511.html",
            "https://users.livejournal.com/evo_lutio/47545.html",
            "https://evo-lutio.livejournal.com/47545.html",
        ] {
            let parsed = Url::parse(url).unwrap();
            let value = jar
                .cookies(&parsed)
                .unwrap_or_else(|| panic!("no cookie for {url}"));
            let value = value.to_str().unwrap();
            assert!(value.contains("adult_explicit=1"), "{url}: {value}");
            assert!(value.contains("adult_concepts=1"), "{url}: {value}");
        }

        // Never sent to non-livejournal hosts: no leak to third-party media
        // hosts or the local test server.
        for url in [
            "https://example.com/pic.jpg",
            "http://127.0.0.1:8080/post.html",
        ] {
            let parsed = Url::parse(url).unwrap();
            assert!(jar.cookies(&parsed).is_none(), "cookie leaked to {url}");
        }
    }
}
