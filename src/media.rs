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
    let client = media_client(user_agent)?;
    rewrite_inline_media_configured(html, base_url, assets_dir, &client, download).await
}

#[cfg(test)]
async fn rewrite_inline_media_with_client(
    html: &str,
    base_url: &Url,
    assets_dir: &Path,
    client: &reqwest::Client,
) -> Result<RewrittenHtml> {
    rewrite_inline_media_configured(html, base_url, assets_dir, client, true).await
}

async fn rewrite_inline_media_configured(
    html: &str,
    base_url: &Url,
    assets_dir: &Path,
    client: &reqwest::Client,
    download: bool,
) -> Result<RewrittenHtml> {
    let targets = image_targets(html);
    let mut downloads: HashMap<Url, String> = HashMap::new();
    let mut assets = Vec::new();
    let mut replacements = Vec::with_capacity(targets.len());
    let allow_local_media = crate::net::is_loopback_test_server(base_url);

    for target in &targets {
        let source = decode_html_character_references(&target.source);
        let url = match base_url.join(source.as_ref()) {
            Ok(url) if is_hosted_http_url(&url) => url,
            Err(_) => {
                replacements.push(None);
                continue;
            }
            Ok(_) => {
                replacements.push(None);
                continue;
            }
        };

        if let Some(replacement) = downloads.get(&url) {
            replacements.push(Some(replacement.clone()));
            continue;
        }

        let absolute_target = url.to_string();
        if !download || (!allow_local_media && is_restricted_media_url(&url)) {
            downloads.insert(url, absolute_target.clone());
            replacements.push(Some(absolute_target));
            continue;
        }

        let filename = filename_for_url(&url);
        let relative_path = relative_asset_path(&filename);

        if download_asset(client, &url, assets_dir, &filename, allow_local_media).await? {
            let local_target = storage::adoc_path(&relative_path);
            downloads.insert(url.clone(), local_target.clone());
            replacements.push(Some(local_target));
            assets.push(MediaAsset {
                original_url: url,
                relative_path,
            });
        } else {
            downloads.insert(url, absolute_target.clone());
            replacements.push(Some(absolute_target));
        }
    }

    Ok(RewrittenHtml {
        html: rewrite_img_sources(html, &targets, &replacements),
        assets,
    })
}

fn is_hosted_http_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https") && url.has_host()
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
    image_targets_scanned(html).0
}

#[cfg(test)]
fn image_targets_with_scan_steps(html: &str) -> (Vec<ImageTarget>, usize) {
    image_targets_scanned(html)
}

fn image_targets_scanned(html: &str) -> (Vec<ImageTarget>, usize) {
    let bytes = html.as_bytes();
    let mut targets = Vec::new();
    let mut cursor = 0;
    let mut scan_steps = 0;

    while cursor < bytes.len() {
        if bytes[cursor] != b'<' {
            cursor += 1;
            scan_steps += 1;
            continue;
        }

        let tag_start = cursor;
        scan_steps += 1;

        if starts_with_at(bytes, tag_start, b"<!--") {
            cursor = find_html_comment_end(bytes, tag_start + 4, &mut scan_steps);
            continue;
        }

        if starts_with_at(bytes, tag_start, b"<?") {
            cursor = find_html_bogus_comment_end(bytes, tag_start + 2, &mut scan_steps);
            continue;
        }

        if starts_with_ascii_case_insensitive_at(bytes, tag_start, b"<!doctype") {
            cursor = find_html_doctype_end(bytes, tag_start + 9, &mut scan_steps);
            continue;
        }

        if starts_with_at(bytes, tag_start, b"<!") {
            cursor = find_html_bogus_comment_end(bytes, tag_start + 2, &mut scan_steps);
            continue;
        }

        match scan_html_tag(html, tag_start, &mut scan_steps) {
            HtmlTagScan::Invalid => cursor += 1,
            HtmlTagScan::Unclosed => break,
            HtmlTagScan::Complete(tag) => {
                cursor = tag.end;
                let name = &html[tag.name_start..tag.name_end];

                if !tag.is_closing
                    && name.eq_ignore_ascii_case("img")
                    && let Some((value_start, value_end)) = tag.src_value
                {
                    targets.push(ImageTarget {
                        source: html[value_start..value_end].to_owned(),
                        value_start,
                        value_end,
                    });
                }

                if !tag.is_closing && is_opaque_html_element(name) {
                    if name.eq_ignore_ascii_case("plaintext") {
                        break;
                    }

                    cursor = find_opaque_html_element_end(html, cursor, name, &mut scan_steps);
                }
            }
        }
    }

    (targets, scan_steps)
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
    let Some(extension) = Path::new(url.path())
        .extension()
        .and_then(|extension| extension.to_str())
    else {
        return "bin";
    };

    if !(1..=5).contains(&extension.len())
        || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return "bin";
    }

    if extension.eq_ignore_ascii_case("jpg") {
        "jpg"
    } else if extension.eq_ignore_ascii_case("jpeg") {
        "jpeg"
    } else if extension.eq_ignore_ascii_case("png") {
        "png"
    } else if extension.eq_ignore_ascii_case("gif") {
        "gif"
    } else if extension.eq_ignore_ascii_case("webp") {
        "webp"
    } else if extension.eq_ignore_ascii_case("svg") {
        "svg"
    } else if extension.eq_ignore_ascii_case("avif") {
        "avif"
    } else if extension.eq_ignore_ascii_case("bmp") {
        "bmp"
    } else if extension.eq_ignore_ascii_case("ico") {
        "ico"
    } else {
        "bin"
    }
}

fn relative_asset_path(filename: &str) -> PathBuf {
    PathBuf::from("..").join("assets").join(filename)
}

fn rewrite_img_sources(
    html: &str,
    targets: &[ImageTarget],
    replacements: &[Option<String>],
) -> String {
    debug_assert_eq!(targets.len(), replacements.len());

    let mut output = String::with_capacity(html.len());
    let mut cursor = 0;

    for (target, replacement) in targets.iter().zip(replacements) {
        output.push_str(&html[cursor..target.value_start]);
        if let Some(target) = replacement {
            push_html_attribute_escaped(&mut output, target);
        } else {
            output.push_str(&html[target.value_start..target.value_end]);
        }
        cursor = target.value_end;
    }

    output.push_str(&html[cursor..]);
    output
}

fn push_html_attribute_escaped(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            character => output.push(character),
        }
    }
}

enum HtmlTagScan {
    Invalid,
    Unclosed,
    Complete(ScannedHtmlTag),
}

struct ScannedHtmlTag {
    name_start: usize,
    name_end: usize,
    is_closing: bool,
    end: usize,
    src_value: Option<(usize, usize)>,
}

// Mirrors html5ever's tag/attribute tokenizer transitions while retaining byte
// offsets into the immutable input. Only quoted attribute-value states hide `>`.
#[derive(Clone, Copy)]
enum HtmlTagState {
    TagName,
    BeforeAttributeName,
    AttributeName,
    AfterAttributeName,
    BeforeAttributeValue,
    AttributeValueDoubleQuoted,
    AttributeValueSingleQuoted,
    AttributeValueUnquoted,
    AfterAttributeValueQuoted,
    SelfClosingStartTag,
}

fn scan_html_tag(html: &str, start: usize, scan_steps: &mut usize) -> HtmlTagScan {
    let bytes = html.as_bytes();

    if bytes.get(start) != Some(&b'<') || matches!(bytes.get(start + 1), Some(b'!' | b'?')) {
        return HtmlTagScan::Invalid;
    }

    let mut cursor = start + 1;
    let is_closing = bytes.get(cursor) == Some(&b'/');
    if is_closing {
        cursor += 1;
    }

    if !bytes.get(cursor).is_some_and(u8::is_ascii_alphabetic) {
        return HtmlTagScan::Invalid;
    }

    let name_start = cursor;
    let mut name_end = cursor;
    let mut attribute_name_start = cursor;
    let mut attribute_name_end = cursor;
    let mut attribute_value_start = cursor;
    let mut src_value = None;
    let mut state = HtmlTagState::TagName;

    while cursor < html.len() {
        let Some(current) = html[cursor..].chars().next() else {
            return HtmlTagScan::Unclosed;
        };
        let current_len = current.len_utf8();
        *scan_steps += 1;

        match state {
            HtmlTagState::TagName => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => {
                    cursor += current_len;
                    state = HtmlTagState::BeforeAttributeName;
                }
                '/' => {
                    cursor += current_len;
                    state = HtmlTagState::SelfClosingStartTag;
                }
                '>' => {
                    cursor += current_len;
                    return complete_html_tag(name_start, name_end, is_closing, cursor, src_value);
                }
                _ => {
                    cursor += current_len;
                    name_end = cursor;
                }
            },
            HtmlTagState::BeforeAttributeName => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => cursor += current_len,
                '/' => {
                    cursor += current_len;
                    state = HtmlTagState::SelfClosingStartTag;
                }
                '>' => {
                    cursor += current_len;
                    return complete_html_tag(name_start, name_end, is_closing, cursor, src_value);
                }
                _ => {
                    attribute_name_start = cursor;
                    cursor += current_len;
                    attribute_name_end = cursor;
                    state = HtmlTagState::AttributeName;
                }
            },
            HtmlTagState::AttributeName => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => {
                    cursor += current_len;
                    state = HtmlTagState::AfterAttributeName;
                }
                '/' => {
                    cursor += current_len;
                    state = HtmlTagState::SelfClosingStartTag;
                }
                '=' => {
                    cursor += current_len;
                    state = HtmlTagState::BeforeAttributeValue;
                }
                '>' => {
                    cursor += current_len;
                    return complete_html_tag(name_start, name_end, is_closing, cursor, src_value);
                }
                _ => {
                    cursor += current_len;
                    attribute_name_end = cursor;
                }
            },
            HtmlTagState::AfterAttributeName => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => cursor += current_len,
                '/' => {
                    cursor += current_len;
                    state = HtmlTagState::SelfClosingStartTag;
                }
                '=' => {
                    cursor += current_len;
                    state = HtmlTagState::BeforeAttributeValue;
                }
                '>' => {
                    cursor += current_len;
                    return complete_html_tag(name_start, name_end, is_closing, cursor, src_value);
                }
                _ => state = HtmlTagState::BeforeAttributeName,
            },
            HtmlTagState::BeforeAttributeValue => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => cursor += current_len,
                '"' => {
                    cursor += current_len;
                    attribute_value_start = cursor;
                    state = HtmlTagState::AttributeValueDoubleQuoted;
                }
                '\'' => {
                    cursor += current_len;
                    attribute_value_start = cursor;
                    state = HtmlTagState::AttributeValueSingleQuoted;
                }
                '>' => {
                    record_src_value(
                        html,
                        &mut src_value,
                        attribute_name_start,
                        attribute_name_end,
                        cursor,
                        cursor,
                    );
                    cursor += current_len;
                    return complete_html_tag(name_start, name_end, is_closing, cursor, src_value);
                }
                _ => {
                    attribute_value_start = cursor;
                    state = HtmlTagState::AttributeValueUnquoted;
                }
            },
            HtmlTagState::AttributeValueDoubleQuoted => {
                if current == '"' {
                    record_src_value(
                        html,
                        &mut src_value,
                        attribute_name_start,
                        attribute_name_end,
                        attribute_value_start,
                        cursor,
                    );
                    state = HtmlTagState::AfterAttributeValueQuoted;
                }
                cursor += current_len;
            }
            HtmlTagState::AttributeValueSingleQuoted => {
                if current == '\'' {
                    record_src_value(
                        html,
                        &mut src_value,
                        attribute_name_start,
                        attribute_name_end,
                        attribute_value_start,
                        cursor,
                    );
                    state = HtmlTagState::AfterAttributeValueQuoted;
                }
                cursor += current_len;
            }
            HtmlTagState::AttributeValueUnquoted => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => {
                    record_src_value(
                        html,
                        &mut src_value,
                        attribute_name_start,
                        attribute_name_end,
                        attribute_value_start,
                        cursor,
                    );
                    cursor += current_len;
                    state = HtmlTagState::BeforeAttributeName;
                }
                '>' => {
                    record_src_value(
                        html,
                        &mut src_value,
                        attribute_name_start,
                        attribute_name_end,
                        attribute_value_start,
                        cursor,
                    );
                    cursor += current_len;
                    return complete_html_tag(name_start, name_end, is_closing, cursor, src_value);
                }
                _ => cursor += current_len,
            },
            HtmlTagState::AfterAttributeValueQuoted => match current {
                '\t' | '\n' | '\r' | '\x0C' | ' ' => {
                    cursor += current_len;
                    state = HtmlTagState::BeforeAttributeName;
                }
                '/' => {
                    cursor += current_len;
                    state = HtmlTagState::SelfClosingStartTag;
                }
                '>' => {
                    cursor += current_len;
                    return complete_html_tag(name_start, name_end, is_closing, cursor, src_value);
                }
                _ => state = HtmlTagState::BeforeAttributeName,
            },
            HtmlTagState::SelfClosingStartTag => {
                if current == '>' {
                    cursor += current_len;
                    return complete_html_tag(name_start, name_end, is_closing, cursor, src_value);
                }
                state = HtmlTagState::BeforeAttributeName;
            }
        }
    }

    HtmlTagScan::Unclosed
}

fn complete_html_tag(
    name_start: usize,
    name_end: usize,
    is_closing: bool,
    end: usize,
    src_value: Option<(usize, usize)>,
) -> HtmlTagScan {
    HtmlTagScan::Complete(ScannedHtmlTag {
        name_start,
        name_end,
        is_closing,
        end,
        src_value,
    })
}

fn record_src_value(
    html: &str,
    src_value: &mut Option<(usize, usize)>,
    name_start: usize,
    name_end: usize,
    value_start: usize,
    value_end: usize,
) {
    if src_value.is_none() && html[name_start..name_end].eq_ignore_ascii_case("src") {
        *src_value = Some((value_start, value_end));
    }
}

fn is_opaque_html_element(name: &str) -> bool {
    [
        "script",
        "style",
        "textarea",
        "xmp",
        "iframe",
        "noembed",
        "noframes",
        "plaintext",
    ]
    .iter()
    .any(|opaque_name| name.eq_ignore_ascii_case(opaque_name))
}

fn find_opaque_html_element_end(
    html: &str,
    content_start: usize,
    name: &str,
    scan_steps: &mut usize,
) -> usize {
    let bytes = html.as_bytes();
    let mut cursor = content_start;
    let closing_name_end_offset = 2 + name.len();

    while cursor + closing_name_end_offset <= bytes.len() {
        *scan_steps += 1;
        let closing_name_end = cursor + closing_name_end_offset;
        let has_appropriate_boundary = matches!(
            bytes.get(closing_name_end),
            Some(byte) if byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>')
        );

        if has_appropriate_boundary
            && bytes.get(cursor..cursor + 2) == Some(b"</")
            && bytes[cursor + 2..closing_name_end].eq_ignore_ascii_case(name.as_bytes())
        {
            return match scan_html_tag(html, cursor, scan_steps) {
                HtmlTagScan::Complete(tag) => tag.end,
                HtmlTagScan::Invalid | HtmlTagScan::Unclosed => bytes.len(),
            };
        }

        cursor += 1;
    }

    bytes.len()
}

/// Return the first byte after an HTML-tokenizer-recognized comment end. `>` and
/// `->` are abrupt endings immediately after `<!--`; `-->` and `--!>` end a
/// comment from its body. An unclosed comment consumes the remainder.
fn find_html_comment_end(bytes: &[u8], content_start: usize, scan_steps: &mut usize) -> usize {
    if bytes.get(content_start) == Some(&b'>') {
        *scan_steps += 1;
        return content_start + 1;
    }
    if bytes.get(content_start..content_start + 2) == Some(b"->") {
        *scan_steps += 1;
        return content_start + 2;
    }

    let mut cursor = content_start;
    while cursor < bytes.len() {
        *scan_steps += 1;
        if bytes.get(cursor..cursor + 3) == Some(b"-->") {
            return cursor + 3;
        }
        if bytes.get(cursor..cursor + 4) == Some(b"--!>") {
            return cursor + 4;
        }
        cursor += 1;
    }

    bytes.len()
}

/// Consume processing-instruction-like and unknown declaration input in the
/// HTML tokenizer's bogus-comment state. The first `>` ends it; EOF consumes
/// the remainder.
fn find_html_bogus_comment_end(
    bytes: &[u8],
    content_start: usize,
    scan_steps: &mut usize,
) -> usize {
    let mut cursor = content_start;
    while cursor < bytes.len() {
        *scan_steps += 1;
        if bytes[cursor] == b'>' {
            return cursor + 1;
        }
        cursor += 1;
    }

    bytes.len()
}

/// Consume a DOCTYPE according to the tokenizer's declaration-end states. A raw
/// `>` abruptly terminates and emits the DOCTYPE even inside a quoted public or
/// system identifier; a matching quote instead continues the state machine.
/// Malformed states use bogus-DOCTYPE handling, where the first `>` ends the
/// declaration. EOF always consumes the remainder.
fn find_html_doctype_end(bytes: &[u8], content_start: usize, scan_steps: &mut usize) -> usize {
    let mut cursor = content_start;

    match bytes.get(cursor) {
        Some(b'>') => {
            *scan_steps += 1;
            return cursor + 1;
        }
        Some(byte) if is_html_whitespace(*byte) => {}
        Some(_) => return find_html_bogus_comment_end(bytes, cursor, scan_steps),
        None => return bytes.len(),
    }

    skip_html_whitespace(bytes, &mut cursor, scan_steps);
    match bytes.get(cursor) {
        Some(b'>') => {
            *scan_steps += 1;
            return cursor + 1;
        }
        Some(_) => {}
        None => return bytes.len(),
    }

    while bytes
        .get(cursor)
        .is_some_and(|byte| !is_html_whitespace(*byte) && *byte != b'>')
    {
        *scan_steps += 1;
        cursor += 1;
    }

    if bytes.get(cursor) == Some(&b'>') {
        *scan_steps += 1;
        return cursor + 1;
    }
    if cursor == bytes.len() {
        return bytes.len();
    }

    skip_html_whitespace(bytes, &mut cursor, scan_steps);
    if bytes.get(cursor) == Some(&b'>') {
        *scan_steps += 1;
        return cursor + 1;
    }

    let is_public = if starts_with_ascii_case_insensitive_at(bytes, cursor, b"public") {
        *scan_steps += b"public".len();
        cursor += b"public".len();
        true
    } else if starts_with_ascii_case_insensitive_at(bytes, cursor, b"system") {
        *scan_steps += b"system".len();
        cursor += b"system".len();
        false
    } else {
        return find_html_bogus_comment_end(bytes, cursor, scan_steps);
    };

    skip_html_whitespace(bytes, &mut cursor, scan_steps);
    let Some(quote @ (b'\'' | b'"')) = bytes.get(cursor).copied() else {
        return match bytes.get(cursor) {
            Some(b'>') => {
                *scan_steps += 1;
                cursor + 1
            }
            Some(_) => find_html_bogus_comment_end(bytes, cursor, scan_steps),
            None => bytes.len(),
        };
    };

    let mut cursor = match find_quoted_doctype_identifier_end(bytes, cursor, quote, scan_steps) {
        QuotedDoctypeIdentifierEnd::Quote(cursor) => cursor,
        QuotedDoctypeIdentifierEnd::Declaration(end) => return end,
        QuotedDoctypeIdentifierEnd::Eof => return bytes.len(),
    };

    skip_html_whitespace(bytes, &mut cursor, scan_steps);
    if !is_public || bytes.get(cursor) == Some(&b'>') {
        return finish_html_doctype(bytes, cursor, scan_steps);
    }

    let Some(quote @ (b'\'' | b'"')) = bytes.get(cursor).copied() else {
        return match bytes.get(cursor) {
            Some(_) => find_html_bogus_comment_end(bytes, cursor, scan_steps),
            None => bytes.len(),
        };
    };

    let cursor = match find_quoted_doctype_identifier_end(bytes, cursor, quote, scan_steps) {
        QuotedDoctypeIdentifierEnd::Quote(cursor) => cursor,
        QuotedDoctypeIdentifierEnd::Declaration(end) => return end,
        QuotedDoctypeIdentifierEnd::Eof => return bytes.len(),
    };

    finish_html_doctype(bytes, cursor, scan_steps)
}

enum QuotedDoctypeIdentifierEnd {
    Quote(usize),
    Declaration(usize),
    Eof,
}

fn find_quoted_doctype_identifier_end(
    bytes: &[u8],
    mut cursor: usize,
    quote: u8,
    scan_steps: &mut usize,
) -> QuotedDoctypeIdentifierEnd {
    *scan_steps += 1;
    cursor += 1;

    while cursor < bytes.len() {
        *scan_steps += 1;
        let current = bytes[cursor];
        cursor += 1;
        if current == b'>' {
            return QuotedDoctypeIdentifierEnd::Declaration(cursor);
        }
        if current == quote {
            return QuotedDoctypeIdentifierEnd::Quote(cursor);
        }
    }

    QuotedDoctypeIdentifierEnd::Eof
}

fn finish_html_doctype(bytes: &[u8], mut cursor: usize, scan_steps: &mut usize) -> usize {
    skip_html_whitespace(bytes, &mut cursor, scan_steps);
    match bytes.get(cursor) {
        Some(b'>') => {
            *scan_steps += 1;
            cursor + 1
        }
        Some(_) => find_html_bogus_comment_end(bytes, cursor, scan_steps),
        None => bytes.len(),
    }
}

fn skip_html_whitespace(bytes: &[u8], cursor: &mut usize, scan_steps: &mut usize) {
    while bytes
        .get(*cursor)
        .is_some_and(|byte| is_html_whitespace(*byte))
    {
        *scan_steps += 1;
        *cursor += 1;
    }
}

fn is_html_whitespace(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

fn starts_with_at(bytes: &[u8], index: usize, needle: &[u8]) -> bool {
    index + needle.len() <= bytes.len() && &bytes[index..index + needle.len()] == needle
}

fn starts_with_ascii_case_insensitive_at(bytes: &[u8], index: usize, needle: &[u8]) -> bool {
    bytes
        .get(index..index + needle.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::{Html, Selector};
    use std::{
        collections::HashSet,
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
        let replacements = [Some(storage::adoc_path(&PathBuf::from(
            r"..\assets\image.png",
        )))];

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
    async fn canonicalizes_blocked_ipv4_mapped_loopback_media_without_requesting_it() {
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

        assert_eq!(
            rewritten.html,
            r#"<p><img src="http://[::ffff:7f00:1]/private.jpg" alt="Private"></p>"#
        );
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
                storage::adoc_path(&expected_path)
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
            format!(
                r#"<p><img src="{}"></p>"#,
                storage::adoc_path(&expected_path)
            )
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
                storage::adoc_path(&expected_path)
            )
        );
        assert_eq!(rewritten.assets.len(), 1);
        assert!(assets_dir.join(expected_path.file_name().unwrap()).exists());

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/real.jpg");
    }

    #[tokio::test]
    async fn bogus_comments_do_not_expose_fake_images_or_hide_later_real_images() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7, 8, 9]))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let cases = [
            (
                r#"<?probe <img src="/fake-question.jpg">>"#,
                "/real-question.jpg",
            ),
            (
                r#"<!probe <img src="/fake-declaration.jpg">>"#,
                "/real-declaration.jpg",
            ),
        ];

        for (bogus, real_path) in cases {
            let html = format!(r#"{bogus}<img src="{real_path}">"#);
            let rewritten = rewrite_inline_media(
                &html,
                &Url::parse(&server.uri()).unwrap(),
                &assets_dir,
                true,
                None,
            )
            .await
            .unwrap();

            assert!(rewritten.html.starts_with(bogus), "got: {}", rewritten.html);
            assert!(
                !rewritten.html.contains(real_path),
                "got: {}",
                rewritten.html
            );
            assert_eq!(rewritten.assets.len(), 1);
        }

        let requests = server.received_requests().await.unwrap();
        let paths: Vec<_> = requests
            .iter()
            .map(|request| request.url.path().to_owned())
            .collect();
        assert_eq!(paths, ["/real-question.jpg", "/real-declaration.jpg"]);
    }

    #[test]
    fn bogus_comment_scanning_is_linear_and_does_not_restart_inside_content() {
        let bogus = r#"<?probe <img src="/fake.jpg">>"#.repeat(4_096);
        let html = format!(r#"{bogus}<img src="/real.jpg">"#);

        let (targets, scan_steps) = image_targets_with_scan_steps(&html);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].source, "/real.jpg");
        assert!(
            scan_steps <= 4 * html.len(),
            "scanner took {scan_steps} steps for {} input bytes",
            html.len()
        );
    }

    #[test]
    fn bogus_comments_at_eof_consume_the_remainder() {
        for html in [
            r#"<?probe <img src="/fake-question.jpg""#,
            r#"<!probe <img src="/fake-declaration.jpg""#,
        ] {
            let (targets, scan_steps) = image_targets_with_scan_steps(html);

            assert!(targets.is_empty(), "got a target for {html:?}");
            assert!(
                scan_steps <= 2 * html.len(),
                "scanner took {scan_steps} steps for {} input bytes",
                html.len()
            );
        }
    }

    #[test]
    fn greater_than_abruptly_ends_quoted_doctype_identifiers() {
        let cases = [
            (
                concat!(
                    r#"<!DOCTYPE html PUBLIC "identifier><img src='/fake-public.jpg'>">"#,
                    r#"<img src="/real-public.jpg">"#,
                ),
                ["/fake-public.jpg", "/real-public.jpg"],
            ),
            (
                concat!(
                    r#"<!DOCTYPE html SYSTEM 'identifier><img src="/fake-system.jpg">'>"#,
                    r#"<img src='/real-system.jpg'>"#,
                ),
                ["/fake-system.jpg", "/real-system.jpg"],
            ),
        ];

        for (html, expected_sources) in cases {
            let (targets, scan_steps) = image_targets_with_scan_steps(html);
            let sources: Vec<_> = targets
                .iter()
                .map(|target| target.source.as_str())
                .collect();

            assert_eq!(sources, expected_sources, "got targets for {html:?}");
            assert!(
                scan_steps <= 2 * html.len(),
                "scanner took {scan_steps} steps for {} input bytes",
                html.len()
            );
        }
    }

    #[tokio::test]
    async fn images_after_abrupt_quoted_doctype_end_are_rewritten_and_requested() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7, 8, 9]))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let base = Url::parse(&server.uri()).unwrap();
        let fake_url = base.join("/fake.jpg").unwrap();
        let real_url = base.join("/real.jpg").unwrap();
        let fake_target = storage::adoc_path(&relative_asset_path(&filename_for_url(&fake_url)));
        let real_target = storage::adoc_path(&relative_asset_path(&filename_for_url(&real_url)));
        let html = concat!(
            r#"<!DOCTYPE html PUBLIC "identifier><img src='/fake.jpg'>">"#,
            r#"<img src="/real.jpg">"#,
        );

        let rewritten = rewrite_inline_media(html, &base, &assets_dir, true, None)
            .await
            .unwrap();

        assert_eq!(
            rewritten.html,
            format!(
                r#"<!DOCTYPE html PUBLIC "identifier><img src='{fake_target}'>"><img src="{real_target}">"#
            )
        );
        assert_eq!(rewritten.assets.len(), 2);
        assert!(
            assets_dir
                .join(fake_target.rsplit('/').next().unwrap())
                .is_file()
        );
        assert!(
            assets_dir
                .join(real_target.rsplit('/').next().unwrap())
                .is_file()
        );
        let requests = server.received_requests().await.unwrap();
        let paths: Vec<_> = requests
            .iter()
            .map(|request| request.url.path().to_owned())
            .collect();
        assert_eq!(paths, ["/fake.jpg", "/real.jpg"]);
    }

    #[test]
    fn malformed_doctype_name_quote_uses_bogus_terminator() {
        let html = r#"<!DOCTYPE "bogus><img src="/real.jpg">"#;

        let (targets, scan_steps) = image_targets_with_scan_steps(html);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].source, "/real.jpg");
        assert!(
            scan_steps <= 2 * html.len(),
            "scanner took {scan_steps} steps for {} input bytes",
            html.len()
        );
    }

    #[tokio::test]
    async fn opaque_html_bodies_preserve_img_looking_bytes_without_requests() {
        let server = MockServer::start().await;
        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let cases = [
            ("script", r#"<script><img src="/fake.jpg"></script>"#),
            (
                "mixed-case style",
                r#"<StYlE><img src="/fake.jpg"></sTyLe>"#,
            ),
            (
                "textarea close-name lookalike",
                r#"<textarea><img src="/fake.jpg"></textareax><img src="/still-fake.jpg"></textarea>"#,
            ),
            ("self-closing xmp", r#"<xmp/><img src="/fake.jpg"></xmp>"#),
            ("unclosed iframe", r#"<iframe><img src="/fake.jpg">"#),
            ("noembed", r#"<noembed><img src="/fake.jpg"></noembed>"#),
            ("noframes", r#"<noframes><img src="/fake.jpg"></noframes>"#),
            (
                "plaintext",
                r#"<plaintext><img src="/fake.jpg"></plaintext><img src="/still-fake.jpg">"#,
            ),
        ];

        for (name, html) in cases {
            let rewritten = rewrite_inline_media(
                html,
                &Url::parse(&server.uri()).unwrap(),
                &assets_dir,
                true,
                None,
            )
            .await
            .unwrap();

            assert_eq!(rewritten.html, html, "case: {name}");
            assert!(rewritten.assets.is_empty(), "case: {name}");
        }

        assert!(server.received_requests().await.unwrap().is_empty());
        assert!(!assets_dir.exists());
    }

    #[tokio::test]
    async fn malformed_opaque_opening_tag_keeps_img_looking_body_bytes_opaque() {
        let server = MockServer::start().await;
        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let html = r#"<script data=x"><img src="/fake.jpg"></script>"#;

        let rewritten = rewrite_inline_media(
            html,
            &Url::parse(&server.uri()).unwrap(),
            &assets_dir,
            true,
            None,
        )
        .await
        .unwrap();

        assert_eq!(rewritten.html, html);
        assert!(rewritten.assets.is_empty());
        assert!(server.received_requests().await.unwrap().is_empty());
        assert!(!assets_dir.exists());
    }

    #[tokio::test]
    async fn malformed_opaque_close_still_exposes_following_real_image() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/real.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7, 8, 9]))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let opaque = r#"<script><img src="/fake.jpg"></script x=y">"#;
        let html = format!(r#"{opaque}<img src="/real.jpg">"#);

        let rewritten = rewrite_inline_media(
            &html,
            &Url::parse(&server.uri()).unwrap(),
            &assets_dir,
            true,
            None,
        )
        .await
        .unwrap();

        assert!(rewritten.html.starts_with(opaque));
        assert!(rewritten.html.contains(r#"<img src="../assets/"#));
        assert_eq!(rewritten.assets.len(), 1);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/real.jpg");
    }

    #[tokio::test]
    async fn quote_in_attribute_name_does_not_expose_img_looking_value_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/real.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7, 8, 9]))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let attribute_tag = r#"<div bad"name=<img src=/fake.jpg>>"#;
        let html = format!(r#"{attribute_tag}<img src="/real.jpg">"#);

        let rewritten = rewrite_inline_media(
            &html,
            &Url::parse(&server.uri()).unwrap(),
            &assets_dir,
            true,
            None,
        )
        .await
        .unwrap();

        assert!(rewritten.html.starts_with(attribute_tag));
        assert!(rewritten.html.contains(r#"<img src="../assets/"#));
        assert_eq!(rewritten.assets.len(), 1);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/real.jpg");
    }

    #[tokio::test]
    async fn real_images_after_closed_opaque_bodies_are_downloaded_once_each() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7, 8, 9]))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let cases = [
            ("script", r#"<script><img src="/fake.jpg"></script>"#),
            ("style", r#"<style><img src="/fake.jpg"></style>"#),
            ("textarea", r#"<textarea><img src="/fake.jpg"></textarea>"#),
            ("xmp", r#"<xmp><img src="/fake.jpg"></xmp>"#),
            ("iframe", r#"<iframe><img src="/fake.jpg"></iframe>"#),
            ("noembed", r#"<noembed><img src="/fake.jpg"></noembed>"#),
            ("noframes", r#"<noframes><img src="/fake.jpg"></noframes>"#),
        ];

        for (name, opaque) in cases {
            let html = format!(r#"{opaque}<img src="/{name}-real.jpg">"#);
            let rewritten = rewrite_inline_media(
                &html,
                &Url::parse(&server.uri()).unwrap(),
                &assets_dir,
                true,
                None,
            )
            .await
            .unwrap();

            assert!(rewritten.html.starts_with(opaque), "case: {name}");
            assert!(
                rewritten.html.ends_with(r#"">"#)
                    && rewritten.html.contains(r#"<img src="../assets/"#),
                "case: {name}; got: {}",
                rewritten.html
            );
            assert_eq!(rewritten.assets.len(), 1, "case: {name}");
        }

        let requests = server.received_requests().await.unwrap();
        let paths: Vec<_> = requests
            .iter()
            .map(|request| request.url.path().to_owned())
            .collect();
        assert_eq!(
            paths,
            [
                "/script-real.jpg",
                "/style-real.jpg",
                "/textarea-real.jpg",
                "/xmp-real.jpg",
                "/iframe-real.jpg",
                "/noembed-real.jpg",
                "/noframes-real.jpg",
            ]
        );
    }

    #[tokio::test]
    async fn scanner_skips_tag_attributes_and_obeys_comment_terminators() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7, 8, 9]))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let cases = [
            (
                "tag attribute",
                r#"<div data-note='<img src="/fake-attribute.jpg">'></div><img src="/real-attribute.jpg">"#,
                "/fake-attribute.jpg",
                "/real-attribute.jpg",
            ),
            (
                "normal comment end",
                r#"<!-- <img src="/fake-normal.jpg"> --><img src="/real-normal.jpg">"#,
                "/fake-normal.jpg",
                "/real-normal.jpg",
            ),
            (
                "bang comment end",
                r#"<!-- <img src="/fake-bang.jpg"> --!><img src="/real-bang.jpg">"#,
                "/fake-bang.jpg",
                "/real-bang.jpg",
            ),
            (
                "abrupt greater-than comment end",
                r#"<!--><span data-note='<img src="/fake-greater.jpg">'></span><img src="/real-greater.jpg">"#,
                "/fake-greater.jpg",
                "/real-greater.jpg",
            ),
            (
                "abrupt arrow comment end",
                r#"<!---><span data-note='<img src="/fake-arrow.jpg">'></span><img src="/real-arrow.jpg">"#,
                "/fake-arrow.jpg",
                "/real-arrow.jpg",
            ),
        ];

        for (name, html, fake_path, real_path) in cases {
            let rewritten = rewrite_inline_media(
                html,
                &Url::parse(&server.uri()).unwrap(),
                &assets_dir,
                true,
                None,
            )
            .await
            .unwrap();

            assert!(rewritten.html.contains(fake_path), "case: {name}");
            assert!(!rewritten.html.contains(real_path), "case: {name}");
            assert_eq!(rewritten.assets.len(), 1, "case: {name}");
        }

        let requests = server.received_requests().await.unwrap();
        let paths: Vec<_> = requests
            .iter()
            .map(|request| request.url.path().to_owned())
            .collect();
        assert_eq!(
            paths,
            [
                "/real-attribute.jpg",
                "/real-normal.jpg",
                "/real-bang.jpg",
                "/real-greater.jpg",
                "/real-arrow.jpg",
            ]
        );
    }

    #[test]
    fn portable_media_filenames_use_bounded_allowlisted_suffixes() {
        let long_suffix = "a".repeat(300);
        let mut cases: Vec<(String, &str)> = [
            ("https://cdn.example.test/image.jpg", "jpg"),
            ("https://cdn.example.test/image.JPEG", "jpeg"),
            ("https://cdn.example.test/image.PNG", "png"),
            ("https://cdn.example.test/image.GiF", "gif"),
            ("https://cdn.example.test/image.WEBP", "webp"),
            ("https://cdn.example.test/image.SvG", "svg"),
            ("https://cdn.example.test/image.AVIF", "avif"),
            ("https://cdn.example.test/image.BMP", "bmp"),
            ("https://cdn.example.test/image.IcO", "ico"),
            ("https://cdn.example.test/image.jpg:large", "bin"),
            ("https://cdn.example.test/image.j@pg", "bin"),
            ("https://cdn.example.test/image.jpg.", "bin"),
            ("https://cdn.example.test/image.jpg%20", "bin"),
            ("https://cdn.example.test/image%2ejpg", "bin"),
            ("https://cdn.example.test/image.tiff", "bin"),
            ("https://cdn.example.test/image", "bin"),
        ]
        .into_iter()
        .map(|(input, expected_extension)| (input.to_owned(), expected_extension))
        .collect();
        cases.push((
            format!("https://cdn.example.test/image.{long_suffix}"),
            "bin",
        ));

        for (input, expected_extension) in cases {
            let url = Url::parse(&input).unwrap();

            assert_eq!(extension_for_url(&url), expected_extension, "url: {url}");

            let filename = filename_for_url(&url);
            let (stem, extension) = filename.split_once('.').unwrap();
            assert_eq!(extension, expected_extension, "url: {url}");
            assert_eq!(stem.len(), 12, "url: {url}");
            assert!(
                stem.bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
                "url: {url}; filename: {filename}"
            );
            assert!(filename.len() < 64, "url: {url}; filename: {filename}");
            assert!(
                !filename.bytes().any(|byte| matches!(
                    byte,
                    b'<' | b'>' | b':' | b'"' | b'/' | b'\\' | b'|' | b'?' | b'*'
                )),
                "url: {url}; filename: {filename}"
            );
            assert!(
                !filename.ends_with(['.', ' ']),
                "url: {url}; filename: {filename}"
            );
        }
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

    #[tokio::test]
    async fn canonicalizes_relative_sources_without_downloading() {
        let server = MockServer::start().await;
        let html = concat!(
            r#"<img src="/root.jpg" alt="root">"#,
            r#"<img src="next.jpg" alt="next">"#,
            r#"<img src="//cdn.example.test/protocol.jpg" alt="protocol">"#,
        );
        let base = Url::parse(&format!("{}/delivered/nested/1.html", server.uri())).unwrap();
        let rewritten = rewrite_inline_media(html, &base, Path::new("unused"), false, None)
            .await
            .unwrap();

        assert_eq!(
            rewritten.html,
            format!(
                concat!(
                    r#"<img src="{}/root.jpg" alt="root">"#,
                    r#"<img src="{}/delivered/nested/next.jpg" alt="next">"#,
                    r#"<img src="http://cdn.example.test/protocol.jpg" alt="protocol">"#,
                ),
                server.uri(),
                server.uri(),
            )
        );
        assert!(rewritten.assets.is_empty());
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "--no-media must issue no media request"
        );
    }

    #[tokio::test]
    async fn no_media_fallbacks_survive_html_reparse_with_exact_targets() {
        let html = concat!(
            r#"<img src="double.jpg?x=1&amp;copy;=2" alt="double">"#,
            r#"<img src='https://cdn.example.test/x&apos;onerror=alert(1)' alt='single'>"#,
        );
        let rewritten = rewrite_inline_media(
            html,
            &Url::parse("https://cdn.example.test/posts/1.html").unwrap(),
            Path::new("unused"),
            false,
            None,
        )
        .await
        .unwrap();

        let document = Html::parse_fragment(&rewritten.html);
        let selector = Selector::parse("img").unwrap();
        let images: Vec<_> = document.select(&selector).collect();
        assert_eq!(images.len(), 2);
        assert_eq!(
            images[1].attr("src"),
            Some("https://cdn.example.test/x'onerror=alert(1)")
        );
        assert_eq!(
            images[0].attr("src"),
            Some("https://cdn.example.test/posts/double.jpg?x=1&copy;=2")
        );
        assert!(images.iter().all(|image| image.attr("onerror").is_none()));

        let rendered = crate::adoc::html_to_adoc(&rewritten.html);
        assert!(
            rendered.contains(concat!(
                "image::https://cdn.example.test/posts/double.jpg?",
                "x=1&copy;=2[double]"
            )),
            "got: {rendered:?}"
        );
        assert!(
            rendered.contains(concat!(
                "image::https://cdn.example.test/x'onerror=alert(1)",
                "[single]"
            )),
            "got: {rendered:?}"
        );
    }

    #[tokio::test]
    async fn keeps_safe_download_failures_absolute_and_only_success_local() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/delivered/missing.jpg"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/delivered/error.jpg"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/delivered/ok.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let base = Url::parse(&format!("{}/delivered/1.html", server.uri())).unwrap();
        let html = concat!(
            r#"<img src="missing.jpg" alt="missing">"#,
            r#"<img src="error.jpg" alt="error">"#,
            r#"<img src="ok.jpg" alt="ok">"#,
        );

        let rewritten = rewrite_inline_media(html, &base, &assets_dir, true, None)
            .await
            .unwrap();

        assert!(
            rewritten
                .html
                .contains(&format!(r#"src="{}/delivered/missing.jpg""#, server.uri())),
            "got: {}",
            rewritten.html
        );
        assert!(
            rewritten
                .html
                .contains(&format!(r#"src="{}/delivered/error.jpg""#, server.uri())),
            "got: {}",
            rewritten.html
        );
        assert!(
            rewritten.html.contains(r#"src="../assets/"#),
            "got: {}",
            rewritten.html
        );
        assert_eq!(rewritten.assets.len(), 1);
        assert_eq!(rewritten.assets[0].original_url.path(), "/delivered/ok.jpg");
    }

    #[tokio::test]
    async fn failed_download_fallbacks_survive_html_reparse_with_exact_targets() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let base = Url::parse(&format!("{}/delivered/1.html", server.uri())).unwrap();
        let html = concat!(
            r#"<img src="failed.jpg?x=1&amp;copy;=2" alt="double">"#,
            r#"<img src='x&apos;onerror=alert(2)' alt='single'>"#,
        );

        let rewritten = rewrite_inline_media(html, &base, &assets_dir, true, None)
            .await
            .unwrap();

        let document = Html::parse_fragment(&rewritten.html);
        let selector = Selector::parse("img").unwrap();
        let images: Vec<_> = document.select(&selector).collect();
        assert_eq!(images.len(), 2);
        assert_eq!(
            images[0].attr("src"),
            Some(format!("{}/delivered/failed.jpg?x=1&copy;=2", server.uri()).as_str())
        );
        assert_eq!(
            images[1].attr("src"),
            Some(format!("{}/delivered/x'onerror=alert(2)", server.uri()).as_str())
        );
        assert!(images.iter().all(|image| image.attr("onerror").is_none()));

        let rendered = crate::adoc::html_to_adoc(&rewritten.html);
        assert!(
            rendered.contains(&format!(
                "image::{}/delivered/failed.jpg?x=1&copy;=2[double]",
                server.uri()
            )),
            "got: {rendered:?}"
        );
        assert!(
            rendered.contains(&format!(
                "image::{}/delivered/x'onerror=alert(2)[single]",
                server.uri()
            )),
            "got: {rendered:?}"
        );
    }

    #[tokio::test]
    async fn unsafe_image_schemes_are_not_requested_or_rendered_as_active_macros() {
        let server = MockServer::start().await;
        let base = Url::parse(&format!("{}/delivered/1.html", server.uri())).unwrap();
        let html = concat!(
            "<p>",
            r#"<img src="javascript:alert(1)" alt="script"> "#,
            r#"<img src="data:image/png,boom" alt="data"> "#,
            r#"<img src="file:///secret.jpg" alt="file"> "#,
            r#"<img src="mailto:reader@example.test">"#,
            "</p>",
        );
        let rewritten = rewrite_inline_media(html, &base, Path::new("unused"), false, None)
            .await
            .unwrap();
        let rewriter = crate::links::LinkRewriter::new("news", HashSet::new(), base);

        let adoc = crate::adoc::html_to_adoc_with_links(&rewritten.html, Some(&rewriter));

        assert_eq!(adoc, "script data file mailto:reader@example.test\n\n");
        assert!(!adoc.contains("image:"), "got: {adoc:?}");
        assert!(server.received_requests().await.unwrap().is_empty());
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
    fn unclosed_quoted_value_consumes_img_looking_remainder() {
        let html = r#"<img src="a.jpg" alt="oops><img src=b.jpg>"#;
        let targets = image_targets(html);

        assert!(targets.is_empty());
    }

    #[test]
    fn scanner_has_linear_work_for_many_unclosed_tag_starts() {
        let html = "<a ".repeat(4_096);

        let (targets, scan_steps) = image_targets_with_scan_steps(&html);

        assert!(targets.is_empty());
        assert!(
            scan_steps <= 4 * html.len(),
            "scanner took {scan_steps} steps for {} input bytes",
            html.len()
        );
    }
}
