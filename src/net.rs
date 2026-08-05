use crate::error::LjError;
use reqwest::cookie::Jar;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
};
use url::{Host, Url};

/// Returns a DNS host without its optional root-label dot. IP literals remain
/// distinguishable through `Url::host()` and are deliberately not returned.
pub(crate) fn normalized_domain_host(url: &Url) -> Option<&str> {
    match url.host()? {
        Host::Domain(host) => Some(host.strip_suffix('.').unwrap_or(host)),
        Host::Ipv4(_) | Host::Ipv6(_) => None,
    }
}

/// Removes one terminal DNS root-label dot from a domain host. IP literals are
/// retained through their parsed `Host` variants and are deliberately unchanged.
pub(crate) fn canonicalize_domain_root_dot(url: &mut Url) -> crate::Result<()> {
    let Some(Host::Domain(host)) = url.host() else {
        return Ok(());
    };
    let Some(canonical) = host.strip_suffix('.').map(str::to_owned) else {
        return Ok(());
    };
    url.set_host(Some(&canonical))
        .map_err(|_| LjError::InvalidTarget {
            input: url.to_string(),
            reason: "could not canonicalize DNS root-label dot".to_string(),
        })
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

/// A URL a production client must refuse to connect to: a non-HTTP(S) scheme,
/// the `localhost` name, or an IP literal that is not a public unicast address
/// (loopback, private, link-local, and every other non-routable range). The
/// single trust boundary shared by the media fetcher and the page client's
/// redirect policy — one notion of "restricted", not two divergent copies.
pub(crate) fn is_restricted_target(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return true;
    }

    let Some(host) = url.host() else {
        return true;
    };

    match host {
        Host::Domain(host) => host.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => !is_public_unicast_ip_literal(IpAddr::V4(address)),
        Host::Ipv6(address) => !is_public_unicast_ip_literal(IpAddr::V6(address)),
    }
}

/// Whether `url` may be fetched. A restricted target is refused unless the
/// caller is operating against a loopback test server (`loopback_page_mode`)
/// and `url` is itself that loopback server — the affordance the test suite
/// relies on and production never enables.
pub(crate) fn target_is_allowed(url: &Url, loopback_page_mode: bool) -> bool {
    !is_restricted_target(url) || loopback_page_mode && is_loopback_test_server(url)
}

/// The page client's per-hop redirect decision. A production request (any
/// `original` that is not itself a loopback test server) may never be
/// redirected into a restricted target: that is the post-redirect trust seam
/// where the delivered URL becomes the identity key (`run.rs`) and the
/// discovery crawl base (`discover.rs`). A loopback test server as the original
/// keeps the loopback redirect affordance the tests need. Mirrors the per-hop
/// re-validation the media fetcher already performs.
pub(crate) fn redirect_hop_allowed(original: &Url, next: &Url) -> bool {
    target_is_allowed(next, is_loopback_test_server(original))
}

fn is_public_unicast_ip_literal(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_unicast_ipv4(address),
        IpAddr::V6(address) => {
            if let Some(embedded) = embedded_ipv4(address) {
                return is_public_unicast_ipv4(embedded);
            }
            is_public_unicast_ipv6(address)
        }
    }
}

fn is_public_unicast_ipv4(address: Ipv4Addr) -> bool {
    let [first, second, third, fourth] = address.octets();
    let is_shared = first == 100 && (second & 0xc0) == 0x40;
    let is_special_use = first == 192 && second == 0 && third == 0 && fourth != 9 && fourth != 10;
    let is_benchmarking = first == 198 && (second & 0xfe) == 18;
    let is_reserved = (first & 0xf0) == 0xf0 && !address.is_broadcast();

    // Rust 1.97's `Ipv4Addr::is_global` remains unstable. Mirror its current
    // IANA-based classification with stable predicates, then exclude multicast
    // and broadcast to require a routable unicast destination.
    first != 0
        && !address.is_private()
        && !is_shared
        && !address.is_loopback()
        && !address.is_link_local()
        && !is_special_use
        && !address.is_documentation()
        && !is_benchmarking
        && !is_reserved
        && !address.is_multicast()
        && !address.is_broadcast()
}

fn is_public_unicast_ipv6(address: Ipv6Addr) -> bool {
    let [first, second, third, fourth, fifth, sixth, seventh, eighth] = address.segments();
    let is_documentation =
        (first == 0x2001 && second == 0x0db8) || (first == 0x3fff && second <= 0x0fff);
    let is_benchmarking = first == 0x2001 && second == 2 && third == 0;
    let is_site_local = (first & 0xffc0) == 0xfec0;
    let is_local_use_translation = first == 0x0064 && second == 0xff9b && third == 1;
    let is_discard_only = first == 0x0100 && second == 0 && third == 0 && fourth == 0;
    let is_ietf_assignment = first == 0x2001
        && second < 0x0200
        && !matches!(
            [first, second, third, fourth, fifth, sixth, seventh, eighth],
            // Port Control Protocol and TURN anycast addresses.
            [0x2001, 1, 0, 0, 0, 0, 0, 1 | 2]
                // AMT and AS112-v6.
                | [0x2001, 3, _, _, _, _, _, _]
                | [0x2001, 4, 0x112, _, _, _, _, _]
                // ORCHIDv2 and Drone Remote ID Protocol Entity Tags.
                | [0x2001, 0x20..=0x3f, _, _, _, _, _, _]
        );
    let is_segment_routing = first == 0x5f00;

    // Rust 1.97's `Ipv6Addr::is_global` remains unstable. Mirror its current
    // IANA-based classification with stable predicates, then exclude IPv6
    // site-local addresses to require a public-routable unicast destination.
    !address.is_multicast()
        && !address.is_loopback()
        && !address.is_unicast_link_local()
        && !address.is_unique_local()
        && !address.is_unspecified()
        && !is_documentation
        && !is_benchmarking
        && !is_site_local
        && !is_local_use_translation
        && !is_discard_only
        && !is_ietf_assignment
        && !is_segment_routing
}

/// Extracts an embedded IPv4 address from IPv6 forms that carry one:
/// IPv4-mapped (`::ffff:0:0/96`), NAT64 (`64:ff9b::/96`), 6to4 (`2002::/16`),
/// and IPv4-compatible (`::/96`, excluding `::` and `::1`).
fn embedded_ipv4(address: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = address.to_ipv4_mapped() {
        return Some(v4);
    }
    let s = address.segments();
    // NAT64 well-known prefix 64:ff9b::/96
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
        return Some(ipv4_from_low32(s));
    }
    // 6to4 2002::/16 — embedded IPv4 is segments 1..3
    if s[0] == 0x2002 {
        return Some(Ipv4Addr::new(
            (s[1] >> 8) as u8,
            (s[1] & 0xff) as u8,
            (s[2] >> 8) as u8,
            (s[2] & 0xff) as u8,
        ));
    }
    // IPv4-compatible ::/96 (deprecated), excluding :: and ::1
    if s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
        let v4 = ipv4_from_low32(s);
        if !v4.is_unspecified() && v4 != Ipv4Addr::new(0, 0, 0, 1) {
            return Some(v4);
        }
    }
    None
}

fn ipv4_from_low32(segments: [u16; 8]) -> Ipv4Addr {
    Ipv4Addr::new(
        (segments[6] >> 8) as u8,
        (segments[6] & 0xff) as u8,
        (segments[7] >> 8) as u8,
        (segments[7] & 0xff) as u8,
    )
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
    fn canonicalizes_one_domain_root_dot_without_changing_url_components() {
        let mut dotted =
            Url::parse("https://news.livejournal.com.:8443/1.html?q=age#post").unwrap();
        canonicalize_domain_root_dot(&mut dotted).unwrap();
        assert_eq!(
            dotted.as_str(),
            "https://news.livejournal.com:8443/1.html?q=age#post"
        );

        for input in ["http://127.0.0.1:8080/1.html", "http://[::1]:8080/1.html"] {
            let mut url = Url::parse(input).unwrap();
            canonicalize_domain_root_dot(&mut url).unwrap();
            assert_eq!(url.as_str(), input);
        }
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

    #[test]
    fn canonicalized_dotted_livejournal_targets_receive_affirmation_cookies() {
        use reqwest::cookie::CookieStore;

        for input in [
            "https://news.livejournal.com./1.html",
            "https://users.livejournal.com./example_user/1.html",
        ] {
            let crate::models::ArchiveTarget::Post { url, .. } =
                crate::target::parse_target(input, None).unwrap()
            else {
                panic!("expected post target");
            };
            let cookies = affirmation_cookie_jar()
                .cookies(&url)
                .unwrap_or_else(|| panic!("no cookies for {url}"));
            let cookies = cookies.to_str().unwrap();
            assert!(cookies.contains("adult_explicit=1"), "{cookies}");
            assert!(cookies.contains("adult_concepts=1"), "{cookies}");
        }
    }

    #[test]
    fn pins_embedded_ipv4_classifier_boundaries() {
        assert_eq!(
            embedded_ipv4("64:ff9b::".parse().unwrap()),
            Some(Ipv4Addr::UNSPECIFIED)
        );
        assert_eq!(
            embedded_ipv4("64:ff9b::ffff:ffff".parse().unwrap()),
            Some(Ipv4Addr::BROADCAST)
        );
        assert_eq!(
            embedded_ipv4("64:ff9a:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap()),
            None
        );
        assert_eq!(embedded_ipv4("64:ff9b:0:1::".parse().unwrap()), None);

        assert_eq!(
            embedded_ipv4("2002::".parse().unwrap()),
            Some(Ipv4Addr::UNSPECIFIED)
        );
        assert_eq!(
            embedded_ipv4("2002:ffff:ffff::".parse().unwrap()),
            Some(Ipv4Addr::BROADCAST)
        );
        assert_eq!(
            embedded_ipv4("2001:ffff:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap()),
            None
        );
        assert_eq!(embedded_ipv4("2003::".parse().unwrap()), None);

        assert_eq!(embedded_ipv4("::".parse().unwrap()), None);
        assert_eq!(embedded_ipv4("::1".parse().unwrap()), None);
        assert_eq!(
            embedded_ipv4("::2".parse().unwrap()),
            Some(Ipv4Addr::new(0, 0, 0, 2))
        );
        assert_eq!(
            embedded_ipv4("::8.8.8.8".parse().unwrap()),
            Some(Ipv4Addr::new(8, 8, 8, 8))
        );
        assert_eq!(
            embedded_ipv4("::ffff:ffff".parse().unwrap()),
            Some(Ipv4Addr::BROADCAST)
        );
        assert_eq!(embedded_ipv4("::1:0:0".parse().unwrap()), None);
    }

    #[test]
    fn ipv4_compatible_unspecified_and_loopback_have_no_embedded_ipv4() {
        // `::` (unspecified) and `::1` (loopback) sit in the IPv4-compatible ::/96
        // range but must NOT decode to an embedded IPv4: `::1` -> 0.0.0.1 would be
        // an SSRF-bypass, and `::` is the unspecified address. Pins the
        // `!v4.is_unspecified() && v4 != 0.0.0.1` exclusion in embedded_ipv4's
        // ::/96 branch against a future refactor.
        assert_eq!(
            embedded_ipv4("::".parse::<std::net::Ipv6Addr>().unwrap()),
            None
        );
        assert_eq!(embedded_ipv4("::1".parse().unwrap()), None);
    }

    #[test]
    fn production_requests_may_not_be_redirected_into_restricted_targets() {
        let production = Url::parse("https://evo_lutio.livejournal.com/166511.html").unwrap();
        // From a production origin, a redirect into loopback, a private range,
        // or a non-HTTP scheme must be refused — the post-redirect trust seam
        // that otherwise repoints identity (run.rs) and the crawl base
        // (discover.rs) at attacker-served content.
        for blocked in [
            "http://127.0.0.1:8080/1.html",       // loopback test-server shape
            "http://127.0.0.1/1.html",            // bare loopback
            "http://10.0.0.1/1.html",             // private
            "http://169.254.169.254/latest/meta", // link-local metadata
            "http://[::1]:8080/1.html",           // ipv6 loopback
            "file:///etc/passwd",                 // non-http scheme
        ] {
            let next = Url::parse(blocked).unwrap();
            assert!(
                !redirect_hop_allowed(&production, &next),
                "{blocked} must be refused from a production origin"
            );
        }

        // The real users -> subdomain hop and ordinary outward redirects stay
        // allowed: this closes an SSRF seam, it is not a livejournal-only
        // allowlist (the journal-identity gate in run.rs enforces that).
        for allowed in [
            "https://evo_lutio.livejournal.com/166512.html",
            "https://users.livejournal.com/evo_lutio/166511.html",
            "https://cdn.example.com/img.jpg",
        ] {
            let next = Url::parse(allowed).unwrap();
            assert!(
                redirect_hop_allowed(&production, &next),
                "{allowed} must stay allowed from a production origin"
            );
        }
    }

    #[test]
    fn loopback_test_origin_keeps_its_loopback_redirect_affordance() {
        // When the ORIGINAL request is itself a loopback test server (tests
        // only, never production), loopback-to-loopback redirects stay allowed
        // so the suite's redirect fixtures keep working.
        let origin = Url::parse("http://127.0.0.1:8080/start.html").unwrap();
        for next in [
            "http://127.0.0.1:9090/next.html",
            "http://localhost:7070/next.html",
        ] {
            let next = Url::parse(next).unwrap();
            assert!(redirect_hop_allowed(&origin, &next), "{next}");
        }
        // Even a loopback test origin is only granted the loopback affordance,
        // not blanket access to every restricted target.
        let private = Url::parse("http://10.0.0.1/next.html").unwrap();
        assert!(!redirect_hop_allowed(&origin, &private));
    }
}
