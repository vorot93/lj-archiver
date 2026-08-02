use crate::{
    error::{LjError, Result},
    models::MediaAsset,
    storage,
};
use reqwest::header::LOCATION;
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::HashMap,
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::sleep;
use url::{Host, Url};

const MEDIA_DOWNLOAD_RETRIES: usize = 2;
const MEDIA_REDIRECT_LIMIT: usize = 10;
const MAX_MEDIA_BYTES: usize = 50 * 1024 * 1024;

pub struct RewrittenHtml {
    pub html: String,
    pub assets: Vec<MediaAsset>,
}

pub async fn rewrite_inline_media(
    html: &str,
    base_url: &Url,
    assets_dir: &Path,
    download: bool,
    user_agent: Option<&str>,
) -> Result<RewrittenHtml> {
    if !download {
        return Ok(RewrittenHtml {
            html: html.to_string(),
            assets: Vec::new(),
        });
    }

    let client = media_client(user_agent)?;
    rewrite_inline_media_with_client(html, base_url, assets_dir, &client).await
}

async fn rewrite_inline_media_with_client(
    html: &str,
    base_url: &Url,
    assets_dir: &Path,
    client: &reqwest::Client,
) -> Result<RewrittenHtml> {
    let targets = image_targets(html);
    let mut downloads: HashMap<Url, Option<PathBuf>> = HashMap::new();
    let mut assets = Vec::new();
    let mut replacements = Vec::with_capacity(targets.len());
    let allow_local_media = crate::net::is_loopback_test_server(base_url);

    for target in &targets {
        let source = decode_html_character_references(&target.source);
        let url = match base_url.join(source.as_ref()) {
            Ok(url) => url,
            Err(_) => {
                replacements.push(None);
                continue;
            }
        };

        if let Some(relative_path) = downloads.get(&url) {
            replacements.push(relative_path.clone());
            continue;
        }

        if !allow_local_media && is_restricted_media_url(&url) {
            downloads.insert(url, None);
            replacements.push(None);
            continue;
        }

        let filename = filename_for_url(&url);
        let relative_path = relative_asset_path(&filename);

        if download_asset(client, &url, assets_dir, &filename, allow_local_media).await? {
            downloads.insert(url.clone(), Some(relative_path.clone()));
            replacements.push(Some(relative_path.clone()));
            assets.push(MediaAsset {
                original_url: url,
                relative_path,
            });
        } else {
            downloads.insert(url, None);
            replacements.push(None);
        }
    }

    Ok(RewrittenHtml {
        html: rewrite_img_sources(html, &targets, &replacements),
        assets,
    })
}

fn media_client(user_agent: Option<&str>) -> Result<reqwest::Client> {
    crate::net::configured_client_builder(user_agent)
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|source| LjError::Http {
            url: "media client builder".to_string(),
            source,
        })
}

fn is_restricted_media_url(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return true;
    }

    let Some(host) = url.host() else {
        return true;
    };

    match host {
        Host::Domain(host) => host.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => is_restricted_ip_literal(IpAddr::V4(address)),
        Host::Ipv6(address) => is_restricted_ip_literal(IpAddr::V6(address)),
    }
}

fn is_restricted_ip_literal(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_restricted_ipv4_literal(address),
        IpAddr::V6(address) => {
            if let Some(v4) = embedded_ipv4(address) {
                return is_restricted_ipv4_literal(v4);
            }
            let first_segment = address.segments()[0];
            address.is_loopback()
                || address.is_unspecified()
                || (first_segment & 0xfe00) == 0xfc00
                || (first_segment & 0xffc0) == 0xfe80
        }
    }
}

fn is_restricted_ipv4_literal(address: Ipv4Addr) -> bool {
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_unspecified()
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

struct ImageTarget {
    source: String,
    value_start: usize,
    value_end: usize,
}

fn image_targets(html: &str) -> Vec<ImageTarget> {
    let mut targets = Vec::new();
    let mut cursor = 0;

    while let Some(tag_start) = find_next_img_tag(html, cursor) {
        let Some(tag_end) = find_tag_end(html, tag_start) else {
            cursor = tag_start + 1;
            continue;
        };
        cursor = tag_end;

        if let Some((value_start, value_end)) = find_src_value(html, tag_start, tag_end) {
            targets.push(ImageTarget {
                source: html[value_start..value_end].to_owned(),
                value_start,
                value_end,
            });
        }
    }

    targets
}

fn decode_html_character_references(value: &str) -> Cow<'_, str> {
    if !value.as_bytes().contains(&b'&') {
        return Cow::Borrowed(value);
    }

    let mut output = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(ampersand) = rest.find('&') {
        output.push_str(&rest[..ampersand]);
        let after_ampersand = &rest[ampersand + 1..];

        if let Some((decoded, consumed)) = decode_html_character_reference(after_ampersand) {
            output.push(decoded);
            rest = &after_ampersand[consumed..];
        } else {
            output.push('&');
            rest = after_ampersand;
        }
    }

    output.push_str(rest);
    Cow::Owned(output)
}

fn decode_html_character_reference(value: &str) -> Option<(char, usize)> {
    let semicolon = value.find(';')?;
    let entity = &value[..semicolon];
    let decoded = match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => decode_numeric_character_reference(entity)?,
    };

    Some((decoded, semicolon + 1))
}

fn decode_numeric_character_reference(entity: &str) -> Option<char> {
    let digits = entity.strip_prefix('#')?;
    let value = if let Some(hex) = digits
        .strip_prefix('x')
        .or_else(|| digits.strip_prefix('X'))
    {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        digits.parse().ok()?
    };

    char::from_u32(value)
}

async fn download_asset(
    client: &reqwest::Client,
    url: &Url,
    assets_dir: &Path,
    filename: &str,
    allow_local_media: bool,
) -> Result<bool> {
    let path = assets_dir.join(filename);
    if path.is_file() {
        return Ok(true); // already fetched this run or a prior --resume run
    }

    let mut bytes = None;
    for attempt in 0..=MEDIA_DOWNLOAD_RETRIES {
        match fetch_media_bytes(client, url, allow_local_media, MAX_MEDIA_BYTES).await {
            Ok(Some(response_bytes)) => {
                bytes = Some(response_bytes);
                break;
            }
            Ok(None) => return Ok(false),
            Err(_) if attempt < MEDIA_DOWNLOAD_RETRIES => {
                sleep(Duration::from_millis(150 * (attempt as u64 + 1))).await;
            }
            Err(_) => return Ok(false),
        }
    }

    let Some(bytes) = bytes else {
        return Ok(false);
    };

    fs::create_dir_all(assets_dir).map_err(|source| LjError::Write {
        path: assets_dir.to_path_buf(),
        source,
    })?;

    storage::write_atomic(&path, &bytes)?;

    Ok(true)
}

async fn fetch_media_bytes(
    client: &reqwest::Client,
    url: &Url,
    allow_local_media: bool,
    max_bytes: usize,
) -> std::result::Result<Option<Vec<u8>>, reqwest::Error> {
    if !allow_local_media && is_restricted_media_url(url) {
        return Ok(None);
    }

    let mut next_url = url.clone();

    for redirects_followed in 0..=MEDIA_REDIRECT_LIMIT {
        let response = client.get(next_url.clone()).send().await?;

        if response.status().is_redirection() {
            if redirects_followed == MEDIA_REDIRECT_LIMIT {
                return Ok(None);
            }

            let Some(redirect_url) = redirect_location(&next_url, &response) else {
                return Ok(None);
            };

            if !allow_local_media && is_restricted_media_url(&redirect_url) {
                return Ok(None);
            }

            next_url = redirect_url;
            continue;
        }

        let response = match response.error_for_status() {
            Ok(response) => response,
            Err(_) => return Ok(None),
        };

        if response
            .content_length()
            .is_some_and(|len| len > max_bytes as u64)
        {
            return Ok(None);
        }

        let mut response = response;
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len() + chunk.len() > max_bytes {
                        return Ok(None);
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => return Ok(None),
            }
        }
        return Ok(Some(bytes));
    }

    Ok(None)
}

fn redirect_location(current_url: &Url, response: &reqwest::Response) -> Option<Url> {
    let location = response.headers().get(LOCATION)?.to_str().ok()?;
    current_url.join(location).ok()
}

fn filename_for_url(url: &Url) -> String {
    let digest = Sha256::digest(url.as_str().as_bytes());
    let mut hash = hex::encode(digest);
    hash.truncate(12);

    format!("{hash}.{}", extension_for_url(url))
}

fn extension_for_url(url: &Url) -> &str {
    Path::new(url.path())
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| !extension.is_empty())
        .unwrap_or("bin")
}

fn relative_asset_path(filename: &str) -> PathBuf {
    PathBuf::from("..").join("assets").join(filename)
}

fn rewrite_img_sources(
    html: &str,
    targets: &[ImageTarget],
    replacements: &[Option<PathBuf>],
) -> String {
    debug_assert_eq!(targets.len(), replacements.len());

    let mut output = String::with_capacity(html.len());
    let mut cursor = 0;

    for (target, replacement) in targets.iter().zip(replacements) {
        output.push_str(&html[cursor..target.value_start]);
        if let Some(path) = replacement {
            output.push_str(&storage::adoc_path(path));
        } else {
            output.push_str(&html[target.value_start..target.value_end]);
        }
        cursor = target.value_end;
    }

    output.push_str(&html[cursor..]);
    output
}

fn find_next_img_tag(html: &str, cursor: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut index = cursor;

    while index + 4 <= bytes.len() {
        if starts_with_at(bytes, index, b"<!--") {
            let comment_body_start = index + 4;
            let comment_end_offset = html[comment_body_start..].find("-->")?;
            index = comment_body_start + comment_end_offset + 3;
            continue;
        }

        if bytes[index] == b'<'
            && bytes[index + 1].eq_ignore_ascii_case(&b'i')
            && bytes[index + 2].eq_ignore_ascii_case(&b'm')
            && bytes[index + 3].eq_ignore_ascii_case(&b'g')
            && bytes
                .get(index + 4)
                .is_none_or(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
        {
            return Some(index);
        }
        index += 1;
    }

    None
}

fn starts_with_at(bytes: &[u8], index: usize, needle: &[u8]) -> bool {
    index + needle.len() <= bytes.len() && &bytes[index..index + needle.len()] == needle
}

fn find_tag_end(html: &str, tag_start: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut quote = None;
    let mut index = tag_start;

    while index < bytes.len() {
        match (bytes[index], quote) {
            (b'\'' | b'"', None) => quote = Some(bytes[index]),
            (byte, Some(active_quote)) if byte == active_quote => quote = None,
            (b'>', None) => return Some(index + 1),
            _ => {}
        }
        index += 1;
    }

    None
}

fn find_src_value(html: &str, tag_start: usize, tag_end: usize) -> Option<(usize, usize)> {
    let bytes = html.as_bytes();
    let mut index = tag_start + 4;

    while index < tag_end {
        while index < tag_end && bytes[index].is_ascii_whitespace() {
            index += 1;
        }

        if index >= tag_end || matches!(bytes[index], b'/' | b'>') {
            return None;
        }

        let name_start = index;
        while index < tag_end && is_attribute_name_byte(bytes[index]) {
            index += 1;
        }
        let name_end = index;

        while index < tag_end && bytes[index].is_ascii_whitespace() {
            index += 1;
        }

        if index >= tag_end || bytes[index] != b'=' {
            continue;
        }

        index += 1;
        while index < tag_end && bytes[index].is_ascii_whitespace() {
            index += 1;
        }

        let (value_start, value_end) = if index < tag_end && matches!(bytes[index], b'\'' | b'"') {
            let quote = bytes[index];
            index += 1;
            let value_start = index;
            while index < tag_end && bytes[index] != quote {
                index += 1;
            }
            (value_start, index)
        } else {
            let value_start = index;
            while index < tag_end && !bytes[index].is_ascii_whitespace() && bytes[index] != b'>' {
                index += 1;
            }
            (value_start, index)
        };

        if html[name_start..name_end].eq_ignore_ascii_case("src") {
            return Some((value_start, value_end));
        }

        if index < tag_end && matches!(bytes[index], b'\'' | b'"') {
            index += 1;
        }
    }

    None
}

fn is_attribute_name_byte(byte: u8) -> bool {
    !byte.is_ascii_whitespace() && !matches!(byte, b'=' | b'/' | b'>')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::SocketAddr,
        path::{Path, PathBuf},
    };
    use tempfile::tempdir;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    #[tokio::test]
    async fn downloads_and_rewrites_images() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pic.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let html = format!(
            r#"<p><img src="{}/pic.jpg" alt="Picture"></p>"#,
            server.uri()
        );
        let rewritten = rewrite_inline_media(
            &html,
            &Url::parse(&server.uri()).unwrap(),
            &assets_dir,
            true,
            None,
        )
        .await
        .unwrap();

        assert!(rewritten.html.contains("../assets/"));
        assert_eq!(rewritten.assets.len(), 1);
        assert!(rewritten.assets[0].relative_path.starts_with("../assets"));
        assert!(
            assets_dir
                .join(rewritten.assets[0].relative_path.file_name().unwrap())
                .exists()
        );
    }

    #[tokio::test]
    async fn dedups_shared_asset_and_leaves_no_temp_file() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pic.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .expect(1) // downloaded once despite two rewrite passes
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let assets = dir.path().join("assets");
        let base = Url::parse(&server.uri()).unwrap();
        let client = media_client(None).unwrap();
        let html = format!(r#"<img src="{}/pic.jpg">"#, server.uri());

        let r1 = rewrite_inline_media_with_client(&html, &base, &assets, &client)
            .await
            .unwrap();
        let r2 = rewrite_inline_media_with_client(&html, &base, &assets, &client)
            .await
            .unwrap();

        assert!(r1.html.contains("../assets/"));
        assert!(r2.html.contains("../assets/"));

        let names: Vec<String> = std::fs::read_dir(&assets)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 1, "one deduped asset, no temp: {names:?}");
        assert!(!names[0].ends_with(".tmp"), "{names:?}");
        // .expect(1) verifies the single download when the server drops.
    }

    #[tokio::test]
    async fn concurrent_rewrites_share_one_final_asset_without_temporary_files() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pic.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let assets = dir.path().join("assets");
        let base = Url::parse(&server.uri()).unwrap();
        let html = format!(r#"<img src="{}/pic.jpg">"#, server.uri());

        let (first, second) = tokio::join!(
            rewrite_inline_media(&html, &base, &assets, true, None),
            rewrite_inline_media(&html, &base, &assets, true, None)
        );
        let first = first.unwrap();
        let second = second.unwrap();

        assert_eq!(first.html, second.html);
        assert!(first.html.starts_with(r#"<img src="../assets/"#));
        assert!(first.html.ends_with(r#"">"#));
        assert_eq!(first.assets.len(), 1);
        assert_eq!(second.assets.len(), 1);
        assert_eq!(
            first.assets[0].relative_path,
            second.assets[0].relative_path
        );

        let entries: Vec<_> = std::fs::read_dir(&assets)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect();
        assert_eq!(entries.len(), 1, "one final asset and no temporary files");
        assert!(entries[0].file_type().unwrap().is_file());
        assert_eq!(
            entries[0].file_name(),
            first.assets[0].relative_path.file_name().unwrap()
        );
    }

    #[test]
    fn serializes_rewritten_image_paths_with_forward_slashes() {
        let html = r#"<img src="remote.png">"#;
        let targets = image_targets(html);
        let replacements = [Some(PathBuf::from(r"..\assets\image.png"))];

        assert_eq!(
            rewrite_img_sources(html, &targets, &replacements),
            r#"<img src="../assets/image.png">"#
        );
    }

    #[tokio::test]
    async fn downloads_media_with_no_user_agent_and_no_cookie_to_non_livejournal_host() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pic.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let html = r#"<p><img src="/pic.jpg" alt="Picture"></p>"#;
        let rewritten = rewrite_inline_media(
            html,
            &Url::parse(&server.uri()).unwrap(),
            &assets_dir,
            true,
            None,
        )
        .await
        .unwrap();

        assert!(rewritten.html.contains("../assets/"));
        assert_eq!(rewritten.assets.len(), 1);
        assert!(
            assets_dir
                .join(rewritten.assets[0].relative_path.file_name().unwrap())
                .exists()
        );

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        // No User-Agent, and the livejournal.com-scoped affirmation cookie is
        // not sent to the loopback (non-LiveJournal) media host.
        assert!(requests[0].headers.get("user-agent").is_none());
        assert!(requests[0].headers.get("cookie").is_none());
    }

    #[tokio::test]
    async fn downloads_media_with_user_agent_when_set() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pic.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let html = r#"<p><img src="/pic.jpg" alt="Picture"></p>"#;
        rewrite_inline_media(
            html,
            &Url::parse(&server.uri()).unwrap(),
            &assets_dir,
            true,
            Some("lj-archiver-test/1.0"),
        )
        .await
        .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests[0].headers.get("user-agent").unwrap(),
            "lj-archiver-test/1.0"
        );
    }

    #[tokio::test]
    async fn rejects_loopback_media_for_production_base_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/private.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let image_url = format!("{}/private.jpg", server.uri());
        let html = format!(r#"<p><img src="{image_url}" alt="Private"></p>"#);
        let rewritten = rewrite_inline_media(
            &html,
            &Url::parse("https://news.livejournal.com/166511.html").unwrap(),
            &assets_dir,
            true,
            None,
        )
        .await
        .unwrap();

        assert_eq!(rewritten.html, html);
        assert!(rewritten.assets.is_empty());
        assert!(!assets_dir.exists());

        let requests = server.received_requests().await.unwrap();
        assert!(requests.is_empty());
    }

    #[tokio::test]
    async fn leaves_ipv4_mapped_loopback_media_src_unchanged_for_production_base_url() {
        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let html = r#"<p><img src="http://[::ffff:127.0.0.1]/private.jpg" alt="Private"></p>"#;
        let rewritten = rewrite_inline_media(
            html,
            &Url::parse("https://news.livejournal.com/166511.html").unwrap(),
            &assets_dir,
            true,
            None,
        )
        .await
        .unwrap();

        assert_eq!(rewritten.html, html);
        assert!(rewritten.assets.is_empty());
        assert!(!assets_dir.exists());
    }

    #[test]
    fn treats_ipv4_mapped_private_addresses_as_restricted() {
        for url in [
            "http://[::ffff:127.0.0.1]/private.jpg",
            "http://[::ffff:10.0.0.1]/private.jpg",
        ] {
            let url = Url::parse(url).unwrap();
            assert!(is_restricted_media_url(&url), "{url} should be restricted");
        }
    }

    #[tokio::test]
    async fn rejects_redirect_to_loopback_media_when_local_media_not_allowed() {
        let public_server = MockServer::start().await;
        let private_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/private.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&private_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/redirect.jpg"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/private.jpg", private_server.uri())),
            )
            .mount(&public_server)
            .await;

        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let public_addr: SocketAddr = *public_server.address();
        let url = Url::parse(&format!(
            "http://media.example.test:{}/redirect.jpg",
            public_addr.port()
        ))
        .unwrap();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .resolve("media.example.test", public_addr)
            .use_preconfigured_tls(crate::tls::rustls_config())
            .build()
            .unwrap();
        let filename = filename_for_url(&url);

        let downloaded = download_asset(&client, &url, &assets_dir, &filename, false)
            .await
            .unwrap();

        assert!(!downloaded);
        assert!(!assets_dir.exists());
        assert_eq!(public_server.received_requests().await.unwrap().len(), 1);
        assert!(private_server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn leaves_image_src_unchanged_when_redirect_targets_loopback_for_production_base_url() {
        let public_server = MockServer::start().await;
        let private_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/private.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&private_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/redirect.jpg"))
            .respond_with(
                ResponseTemplate::new(303)
                    .insert_header("location", format!("{}/private.jpg", private_server.uri())),
            )
            .mount(&public_server)
            .await;

        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let public_addr: SocketAddr = *public_server.address();
        let image_url = format!(
            "http://media.example.test:{}/redirect.jpg",
            public_addr.port()
        );
        let html = format!(r#"<p><img src="{image_url}" alt="Redirect"></p>"#);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .resolve("media.example.test", public_addr)
            .use_preconfigured_tls(crate::tls::rustls_config())
            .build()
            .unwrap();

        let rewritten = rewrite_inline_media_with_client(
            &html,
            &Url::parse("https://news.livejournal.com/166511.html").unwrap(),
            &assets_dir,
            &client,
        )
        .await
        .unwrap();

        assert_eq!(rewritten.html, html);
        assert!(rewritten.assets.is_empty());
        assert!(!assets_dir.exists());
        assert_eq!(public_server.received_requests().await.unwrap().len(), 1);
        assert!(private_server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn leaves_src_unchanged_when_redirect_targets_ipv4_mapped_loopback() {
        let public_server = MockServer::start().await;
        let private_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/private.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&private_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/redirect.jpg"))
            .respond_with(ResponseTemplate::new(303).insert_header(
                "location",
                format!(
                    "http://[::ffff:127.0.0.1]:{}/private.jpg",
                    private_server.address().port()
                ),
            ))
            .mount(&public_server)
            .await;

        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let public_addr: SocketAddr = *public_server.address();
        let image_url = format!(
            "http://media.example.test:{}/redirect.jpg",
            public_addr.port()
        );
        let html = format!(r#"<p><img src="{image_url}" alt="Redirect"></p>"#);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .resolve("media.example.test", public_addr)
            .use_preconfigured_tls(crate::tls::rustls_config())
            .build()
            .unwrap();

        let rewritten = rewrite_inline_media_with_client(
            &html,
            &Url::parse("https://news.livejournal.com/166511.html").unwrap(),
            &assets_dir,
            &client,
        )
        .await
        .unwrap();

        assert_eq!(rewritten.html, html);
        assert!(rewritten.assets.is_empty());
        assert!(!assets_dir.exists());
        assert_eq!(public_server.received_requests().await.unwrap().len(), 1);
        assert!(private_server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn allows_loopback_media_for_loopback_base_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/private.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let image_url = format!("{}/private.jpg", server.uri());
        let html = format!(r#"<p><img src="{image_url}" alt="Private"></p>"#);
        let rewritten = rewrite_inline_media(
            &html,
            &Url::parse(&format!("{}/news/166511.html", server.uri())).unwrap(),
            &assets_dir,
            true,
            None,
        )
        .await
        .unwrap();

        assert!(rewritten.html.contains("../assets/"));
        assert_eq!(rewritten.assets.len(), 1);
        assert!(
            assets_dir
                .join(rewritten.assets[0].relative_path.file_name().unwrap())
                .exists()
        );
    }

    #[tokio::test]
    async fn allows_loopback_redirect_media_for_loopback_base_url() {
        let public_server = MockServer::start().await;
        let private_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/private.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&private_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/redirect.jpg"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/private.jpg", private_server.uri())),
            )
            .mount(&public_server)
            .await;

        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let image_url = format!("{}/redirect.jpg", public_server.uri());
        let html = format!(r#"<p><img src="{image_url}" alt="Redirect"></p>"#);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .use_preconfigured_tls(crate::tls::rustls_config())
            .build()
            .unwrap();
        let rewritten = rewrite_inline_media_with_client(
            &html,
            &Url::parse(&format!("{}/news/166511.html", public_server.uri())).unwrap(),
            &assets_dir,
            &client,
        )
        .await
        .unwrap();

        assert!(rewritten.html.contains("../assets/"));
        assert_eq!(rewritten.assets.len(), 1);
        assert!(
            assets_dir
                .join(rewritten.assets[0].relative_path.file_name().unwrap())
                .exists()
        );
        assert_eq!(public_server.received_requests().await.unwrap().len(), 1);
        assert_eq!(private_server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn preserves_src_less_images_before_rewritten_sources() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ok.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![4, 5, 6]))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let html = r#"<p>before<img alt="x"><img src="/ok.jpg"></p>"#;
        let url = Url::parse(&format!("{}/ok.jpg", server.uri())).unwrap();
        let expected_path = relative_asset_path(&filename_for_url(&url));
        let rewritten = rewrite_inline_media(
            html,
            &Url::parse(&server.uri()).unwrap(),
            &assets_dir,
            true,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            rewritten.html,
            format!(
                r#"<p>before<img alt="x"><img src="{}"></p>"#,
                expected_path.to_string_lossy()
            )
        );
        assert_eq!(rewritten.assets.len(), 1);
        assert!(assets_dir.join(expected_path.file_name().unwrap()).exists());
    }

    #[tokio::test]
    async fn resolves_html_entities_in_image_src_urls() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pic.jpg"))
            .and(query_param("a", "1"))
            .and(query_param("b", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7, 8, 9]))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let html = r#"<p><img src="/pic.jpg?a=1&amp;b=2"></p>"#;
        let url = Url::parse(&format!("{}/pic.jpg?a=1&b=2", server.uri())).unwrap();
        let expected_path = relative_asset_path(&filename_for_url(&url));
        let rewritten = rewrite_inline_media(
            html,
            &Url::parse(&server.uri()).unwrap(),
            &assets_dir,
            true,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            rewritten.html,
            format!(r#"<p><img src="{}"></p>"#, expected_path.to_string_lossy())
        );
        assert_eq!(rewritten.assets.len(), 1);
        assert!(assets_dir.join(expected_path.file_name().unwrap()).exists());

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/pic.jpg");
        assert_eq!(requests[0].url.query(), Some("a=1&b=2"));
    }

    #[tokio::test]
    async fn ignores_commented_img_tags_when_rewriting_sources() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/real.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7, 8, 9]))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let html = r#"<!-- <img src="/fake.jpg"> --><img src="/real.jpg">"#;
        let url = Url::parse(&format!("{}/real.jpg", server.uri())).unwrap();
        let expected_path = relative_asset_path(&filename_for_url(&url));
        let rewritten = rewrite_inline_media(
            html,
            &Url::parse(&server.uri()).unwrap(),
            &assets_dir,
            true,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            rewritten.html,
            format!(
                r#"<!-- <img src="/fake.jpg"> --><img src="{}">"#,
                expected_path.to_string_lossy()
            )
        );
        assert_eq!(rewritten.assets.len(), 1);
        assert!(assets_dir.join(expected_path.file_name().unwrap()).exists());

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/real.jpg");
    }

    #[tokio::test]
    async fn leaves_remote_urls_when_download_disabled() {
        let html = r#"<p><img src="https://cdn.example.test/pic.jpg"></p>"#;
        let rewritten = rewrite_inline_media(
            html,
            &Url::parse("https://news.livejournal.com/166511.html").unwrap(),
            Path::new("unused"),
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(rewritten.html, html);
        assert!(rewritten.assets.is_empty());
    }

    #[test]
    fn blocks_ipv6_embedded_ipv4_restricted_targets() {
        for host in [
            "[64:ff9b::a9fe:a9fe]", // NAT64 -> 169.254.169.254 (link-local metadata)
            "[64:ff9b::a00:1]",     // NAT64 -> 10.0.0.1 (private)
            "[2002:7f00:1::]",      // 6to4 -> 127.0.0.1 (loopback)
            "[::127.0.0.1]",        // IPv4-compatible -> 127.0.0.1 (loopback)
        ] {
            let url = Url::parse(&format!("http://{host}/x.jpg")).unwrap();
            assert!(is_restricted_media_url(&url), "{host} must be blocked");
        }
    }

    #[test]
    fn allows_ipv6_embedded_public_ipv4() {
        // NAT64 -> 93.184.216.34 (public) must stay allowed (no over-blocking).
        let url = Url::parse("http://[64:ff9b::5db8:d822]/x.jpg").unwrap();
        assert!(!is_restricted_media_url(&url));
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

    #[tokio::test]
    async fn rejects_media_larger_than_cap() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(100)))
            .mount(&server)
            .await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .use_preconfigured_tls(crate::tls::rustls_config())
            .build()
            .unwrap();
        let url = Url::parse(&format!("{}/big.jpg", server.uri())).unwrap();
        // Body is 100 bytes; cap of 10 must reject it.
        let over = fetch_media_bytes(&client, &url, true, 10).await.unwrap();
        assert!(over.is_none());
        // Cap of 1000 accepts it.
        let under = fetch_media_bytes(&client, &url, true, 1000).await.unwrap();
        assert_eq!(under.unwrap().len(), 100);
    }

    #[tokio::test]
    async fn rejects_media_that_inflates_past_cap() {
        use std::io::Write;

        use flate2::{Compression, write::GzEncoder};
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        // ~5000 bytes that gzip to a few dozen: small on the wire, large decoded.
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&b"x".repeat(5000)).unwrap();
        let gz = encoder.finish().unwrap();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bomb.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-encoding", "gzip")
                    .set_body_bytes(gz),
            )
            .mount(&server)
            .await;

        // gzip(true) => reqwest auto-decompresses and strips Content-Length, so the
        // Content-Length pre-check is bypassed and the streaming loop is the enforcer.
        let client = reqwest::Client::builder()
            .gzip(true)
            .redirect(reqwest::redirect::Policy::none())
            .use_preconfigured_tls(crate::tls::rustls_config())
            .build()
            .unwrap();
        let url = Url::parse(&format!("{}/bomb.jpg", server.uri())).unwrap();
        // Decoded size (5000) exceeds the 1000-byte cap; must be rejected by the loop.
        let result = fetch_media_bytes(&client, &url, true, 1000).await.unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn malformed_img_does_not_drop_later_images() {
        // First <img> has an unterminated quote; the second must still be found.
        let html = r#"<img src="a.jpg" alt="oops><img src="b.jpg">"#;
        let targets = image_targets(html);
        let sources: Vec<_> = targets.iter().map(|t| t.source.as_str()).collect();
        assert!(
            sources.contains(&"b.jpg"),
            "later image was dropped: {sources:?}"
        );
    }
}
