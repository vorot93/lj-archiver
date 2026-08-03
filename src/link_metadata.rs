use crate::{
    adoc::sanitize_macro_target,
    links::{ActiveLink, ActiveLinkContext, POST_LINK_PREFIX_REFERENCE, ResolvedLinks},
    target::{journals_alias, post_ref_from_url, post_url},
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use url::Url;

const LINK_METADATA_VERSION: u32 = 1;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PostLinkMetadata {
    version: u32,
    post_sha256: String,
    links: Vec<StoredActiveLink>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredActiveLink {
    start: usize,
    end: usize,
    target_id: u64,
    context: StoredActiveLinkContext,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredActiveLinkContext {
    Xref,
    ImageLink,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LinkMetadataError {
    #[error("could not decode link metadata: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("could not encode link metadata: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("invalid link metadata: {0}")]
    Invalid(String),
}

pub(crate) fn serialize_link_metadata(
    contents: &str,
    links: &[ActiveLink],
) -> std::result::Result<Vec<u8>, LinkMetadataError> {
    let mut links = links.to_vec();
    links.sort_by_key(|link| (link.start, link.end));
    let stored_links = links
        .into_iter()
        .map(StoredActiveLink::from)
        .collect::<Vec<_>>();
    validate_links(contents, &stored_links, None)?;
    let metadata = PostLinkMetadata {
        version: LINK_METADATA_VERSION,
        post_sha256: sha256(contents),
        links: stored_links,
    };
    let mut encoded = serde_json::to_vec(&metadata).map_err(LinkMetadataError::Encode)?;
    encoded.push(b'\n');
    Ok(encoded)
}

pub(crate) fn repair_retained_links(
    contents: &str,
    encoded_metadata: &[u8],
    journal: &str,
    final_targets: &HashMap<u64, String>,
) -> std::result::Result<ResolvedLinks, LinkMetadataError> {
    let metadata: PostLinkMetadata =
        serde_json::from_slice(encoded_metadata).map_err(LinkMetadataError::Decode)?;
    if metadata.version != LINK_METADATA_VERSION {
        return Err(LinkMetadataError::Invalid(format!(
            "unsupported version {}",
            metadata.version
        )));
    }
    if metadata.post_sha256.len() != 64
        || !metadata
            .post_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(LinkMetadataError::Invalid(
            "post_sha256 must be 64 lowercase hexadecimal characters".to_string(),
        ));
    }
    if metadata.post_sha256 != sha256(contents) {
        return Err(LinkMetadataError::Invalid(
            "post_sha256 does not match post contents".to_string(),
        ));
    }
    validate_links(contents, &metadata.links, Some(journal))?;

    let mut repaired = String::with_capacity(contents.len());
    let mut refreshed_links = Vec::with_capacity(metadata.links.len());
    let mut cursor = 0;
    for link in metadata.links {
        repaired.push_str(&contents[cursor..link.start]);
        let replacement = desired_expression(journal, &link, final_targets)?;
        let start = repaired.len();
        repaired.push_str(&replacement);
        let end = repaired.len();
        refreshed_links.push(ActiveLink {
            start,
            end,
            target_id: link.target_id,
            context: match link.context {
                StoredActiveLinkContext::Xref => ActiveLinkContext::Xref,
                StoredActiveLinkContext::ImageLink => ActiveLinkContext::ImageLink,
            },
        });
        cursor = link.end;
    }
    repaired.push_str(&contents[cursor..]);

    Ok(ResolvedLinks {
        contents: repaired,
        links: refreshed_links,
    })
}

fn sha256(contents: &str) -> String {
    hex::encode(Sha256::digest(contents.as_bytes()))
}

fn validate_links(
    contents: &str,
    links: &[StoredActiveLink],
    journal: Option<&str>,
) -> std::result::Result<(), LinkMetadataError> {
    let mut previous: Option<&StoredActiveLink> = None;
    for link in links {
        if link.start >= link.end {
            return invalid("link ranges must be nonempty and forward");
        }
        if link.end > contents.len() {
            return invalid("link range is outside post contents");
        }
        if !contents.is_char_boundary(link.start) || !contents.is_char_boundary(link.end) {
            return invalid("link range is not on UTF-8 boundaries");
        }
        if link.target_id == 0 {
            return invalid("link target IDs must be positive");
        }
        if let Some(previous) = previous {
            if (link.start, link.end) <= (previous.start, previous.end) {
                return invalid("link ranges must be strictly sorted");
            }
            if link.start < previous.end {
                return invalid("link ranges must not overlap");
            }
        }

        let expression = &contents[link.start..link.end];
        match link.context {
            StoredActiveLinkContext::Xref => {
                if !contents[link.end..].starts_with('[') {
                    return invalid("xref expression is not followed immediately by a label");
                }
                validate_expression(expression, link.target_id, journal, true)?;
            }
            StoredActiveLinkContext::ImageLink => {
                if !contents[..link.start].ends_with("link=") {
                    return invalid("image link expression is not immediately after link=");
                }
                if !contents[link.end..].starts_with(']') {
                    return invalid("image link expression is not followed by its terminator");
                }
                validate_expression(expression, link.target_id, journal, false)?;
            }
        }
        previous = Some(link);
    }
    Ok(())
}

fn validate_expression(
    expression: &str,
    target_id: u64,
    journal: Option<&str>,
    xref: bool,
) -> std::result::Result<(), LinkMetadataError> {
    let local_prefix = if xref {
        "xref:"
    } else {
        POST_LINK_PREFIX_REFERENCE
    };
    if let Some(filename) = expression.strip_prefix(local_prefix) {
        let actual_id = numeric_adoc_id(filename).ok_or_else(|| {
            invalid_error("local link target is not a positive numeric .adoc file")
        })?;
        if actual_id != target_id {
            return invalid("local link target ID does not match metadata");
        }
        return Ok(());
    }

    validate_external_expression(expression, target_id, journal)
}

fn validate_external_expression(
    expression: &str,
    target_id: u64,
    requested_journal: Option<&str>,
) -> std::result::Result<(), LinkMetadataError> {
    let url = Url::parse(expression)
        .map_err(|_| invalid_error("external link target is not a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
        return invalid("external link target must be hosted HTTP(S)");
    }

    let (actual_journal, actual_id) = if let Some(post_ref) = post_ref_from_url(&url) {
        post_ref
    } else if crate::net::is_loopback_test_server(&url) {
        loopback_post_ref(&url)
            .ok_or_else(|| invalid_error("loopback link target is not a journal post URL"))?
    } else {
        return invalid("external link target is not a LiveJournal post URL");
    };
    if actual_id != target_id {
        return invalid("external link target ID does not match metadata");
    }
    if requested_journal.is_some_and(|journal| !journals_alias(&actual_journal, journal)) {
        return invalid("external link target journal does not match requested journal");
    }
    Ok(())
}

fn loopback_post_ref(url: &Url) -> Option<(String, u64)> {
    let mut segments = url.path_segments()?;
    let journal = segments.next()?;
    let filename = segments.next()?;
    if journal.is_empty() || segments.next().is_some() {
        return None;
    }
    let id = filename.strip_suffix(".html")?.parse::<u64>().ok()?;
    (id > 0).then(|| (journal.to_string(), id))
}

fn numeric_adoc_id(filename: &str) -> Option<u64> {
    let digits = filename.strip_suffix(".adoc")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let id = digits.parse::<u64>().ok()?;
    (id > 0).then_some(id)
}

fn desired_expression(
    journal: &str,
    link: &StoredActiveLink,
    final_targets: &HashMap<u64, String>,
) -> std::result::Result<String, LinkMetadataError> {
    if let Some(filename) = final_targets.get(&link.target_id) {
        return Ok(match link.context {
            StoredActiveLinkContext::Xref => format!("xref:{filename}"),
            StoredActiveLinkContext::ImageLink => {
                format!("{POST_LINK_PREFIX_REFERENCE}{filename}")
            }
        });
    }

    let mut target = sanitize_macro_target(
        post_url(journal, link.target_id)
            .map_err(|error| invalid_error(error.to_string()))?
            .as_str(),
    );
    if matches!(link.context, StoredActiveLinkContext::ImageLink) {
        target = target.replace(',', "%2C");
    }
    Ok(target)
}

fn invalid<T>(message: impl Into<String>) -> std::result::Result<T, LinkMetadataError> {
    Err(invalid_error(message))
}

fn invalid_error(message: impl Into<String>) -> LinkMetadataError {
    LinkMetadataError::Invalid(message.into())
}

impl From<ActiveLink> for StoredActiveLink {
    fn from(link: ActiveLink) -> Self {
        Self {
            start: link.start,
            end: link.end,
            target_id: link.target_id,
            context: match link.context {
                ActiveLinkContext::Xref => StoredActiveLinkContext::Xref,
                ActiveLinkContext::ImageLink => StoredActiveLinkContext::ImageLink,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::links::{ActiveLink, ActiveLinkContext, ResolvedLinks};
    use std::collections::HashMap;

    const CONTENTS: &str = "xref:2.adoc[Two]\n";
    const CONTENTS_SHA256: &str =
        "5da1c4915c8f5d45e04377c3d6937153c0e6a4dc90644b1bd3cfe50b00102d22";

    fn valid_metadata() -> Vec<u8> {
        format!(
            concat!(
                "{{\"version\":1,\"post_sha256\":\"{}\",",
                "\"links\":[{{\"start\":0,\"end\":11,\"target_id\":2,",
                "\"context\":\"xref\"}}]}}\n"
            ),
            CONTENTS_SHA256
        )
        .into_bytes()
    }

    fn unchecked_metadata(contents: &str, links: &[ActiveLink]) -> Vec<u8> {
        let metadata = PostLinkMetadata {
            version: LINK_METADATA_VERSION,
            post_sha256: sha256(contents),
            links: links.iter().cloned().map(StoredActiveLink::from).collect(),
        };
        let mut encoded = serde_json::to_vec(&metadata).unwrap();
        encoded.push(b'\n');
        encoded
    }

    fn recorded_link(
        contents: &str,
        expression: &str,
        target_id: u64,
        context: ActiveLinkContext,
    ) -> ActiveLink {
        let start = contents.find(expression).unwrap();
        ActiveLink {
            start,
            end: start + expression.len(),
            target_id,
            context,
        }
    }

    #[test]
    fn serialization_is_compact_deterministic_json_with_one_trailing_newline() {
        let contents = "xref:2.adoc[Two]\n";
        let links = vec![ActiveLink {
            start: 0,
            end: "xref:2.adoc".len(),
            target_id: 2,
            context: ActiveLinkContext::Xref,
        }];
        let encoded = serialize_link_metadata(contents, &links).unwrap();
        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            concat!(
                "{\"version\":1,\"post_sha256\":\"",
                "5da1c4915c8f5d45e04377c3d6937153c0e6a4dc90644b1bd3cfe50b00102d22",
                "\",\"links\":[{\"start\":0,\"end\":11,\"target_id\":2,",
                "\"context\":\"xref\"}]}\n"
            )
        );
    }

    #[test]
    fn serialization_accepts_an_empty_link_array() {
        let encoded = serialize_link_metadata("no links\n", &[]).unwrap();

        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            concat!(
                "{\"version\":1,\"post_sha256\":\"",
                "ac94c2eca3a56581db227e484e6cc1524f84290b675a9d94268e7bc3cb4821c4",
                "\",\"links\":[]}\n"
            )
        );
    }

    #[test]
    fn serialization_rejects_zero_length_ranges() {
        let link = ActiveLink {
            start: 0,
            end: 0,
            target_id: 2,
            context: ActiveLinkContext::Xref,
        };

        assert!(serialize_link_metadata(CONTENTS, &[link]).is_err());
    }

    #[test]
    fn serialization_rejects_reversed_ranges() {
        let link = ActiveLink {
            start: 11,
            end: 0,
            target_id: 2,
            context: ActiveLinkContext::Xref,
        };

        assert!(serialize_link_metadata(CONTENTS, &[link]).is_err());
    }

    #[test]
    fn serialization_rejects_overlapping_ranges() {
        let links = [
            ActiveLink {
                start: 0,
                end: 11,
                target_id: 2,
                context: ActiveLinkContext::Xref,
            },
            ActiveLink {
                start: 5,
                end: 12,
                target_id: 2,
                context: ActiveLinkContext::Xref,
            },
        ];

        assert!(serialize_link_metadata(CONTENTS, &links).is_err());
    }

    #[test]
    fn serialization_normalizes_unsorted_input_ranges() {
        let contents = "xref:2.adoc[Two] xref:3.adoc[Three]\n";
        let first = recorded_link(contents, "xref:2.adoc", 2, ActiveLinkContext::Xref);
        let second = recorded_link(contents, "xref:3.adoc", 3, ActiveLinkContext::Xref);

        let encoded = serialize_link_metadata(contents, &[second, first]).unwrap();
        let repaired = repair_retained_links(
            contents,
            &encoded,
            "news",
            &HashMap::from([(2, "2.adoc".to_string()), (3, "3.adoc".to_string())]),
        )
        .unwrap();

        assert_eq!(repaired.links[0].start, 0);
        assert_eq!(repaired.links[1].start, "xref:2.adoc[Two] ".len());
    }

    #[test]
    fn serialization_rejects_out_of_bounds_ranges() {
        let link = ActiveLink {
            start: 0,
            end: CONTENTS.len() + 1,
            target_id: 2,
            context: ActiveLinkContext::Xref,
        };

        assert!(serialize_link_metadata(CONTENTS, &[link]).is_err());
    }

    #[test]
    fn serialization_rejects_non_utf8_boundary_ranges() {
        let contents = "xref:é.adoc[label]";
        let invalid = ActiveLink {
            start: "xref:".len() + 1,
            end: "xref:é.adoc".len(),
            target_id: 2,
            context: ActiveLinkContext::Xref,
        };
        assert!(serialize_link_metadata(contents, &[invalid]).is_err());
    }

    #[test]
    fn serialization_rejects_zero_target_ids() {
        let link = ActiveLink {
            start: 0,
            end: 11,
            target_id: 0,
            context: ActiveLinkContext::Xref,
        };

        assert!(serialize_link_metadata(CONTENTS, &[link]).is_err());
    }

    #[test]
    fn serialization_rejects_duplicate_source_locations() {
        let link = ActiveLink {
            start: 0,
            end: 11,
            target_id: 2,
            context: ActiveLinkContext::Xref,
        };

        assert!(serialize_link_metadata(CONTENTS, &[link.clone(), link]).is_err());
    }

    #[test]
    fn serialization_rejects_xrefs_without_an_immediate_label_suffix() {
        let contents = "xref:2.adoc";
        let link = recorded_link(contents, contents, 2, ActiveLinkContext::Xref);

        assert!(serialize_link_metadata(contents, &[link]).is_err());
    }

    #[test]
    fn serialization_rejects_image_targets_without_immediate_link_sentinel() {
        let target = "{lj-archiver-post-link-prefix}2.adoc";
        let contents = format!("image:x.jpg[,{target}]");
        let link = recorded_link(&contents, target, 2, ActiveLinkContext::ImageLink);

        assert!(serialize_link_metadata(&contents, &[link]).is_err());
    }

    #[test]
    fn serialization_rejects_image_spans_that_include_the_generated_terminator() {
        let target = "{lj-archiver-post-link-prefix}2.adoc";
        let contents = format!("image:x.jpg[,link={target}]");
        let mut link = recorded_link(&contents, target, 2, ActiveLinkContext::ImageLink);
        link.end += 1;

        assert!(serialize_link_metadata(&contents, &[link]).is_err());
    }

    #[test]
    fn decoding_rejects_unsorted_ranges_even_when_their_syntax_is_valid() {
        let contents = "xref:2.adoc[Two] xref:3.adoc[Three]\n";
        let first = recorded_link(contents, "xref:2.adoc", 2, ActiveLinkContext::Xref);
        let second = recorded_link(contents, "xref:3.adoc", 3, ActiveLinkContext::Xref);
        let encoded = unchecked_metadata(contents, &[second, first]);

        assert!(repair_retained_links(contents, &encoded, "news", &HashMap::new()).is_err());
    }

    #[test]
    fn repair_rejects_local_filenames_whose_numeric_id_does_not_match_metadata() {
        let contents = "xref:3.adoc[Three]\n";
        let link = recorded_link(contents, "xref:3.adoc", 2, ActiveLinkContext::Xref);
        let encoded = unchecked_metadata(contents, &[link]);

        assert!(repair_retained_links(contents, &encoded, "news", &HashMap::new()).is_err());
    }

    #[test]
    fn repair_rejects_foreign_journal_urls() {
        let expression = "https://other.livejournal.com/2.html";
        let contents = format!("{expression}[Two]\n");
        let link = recorded_link(&contents, expression, 2, ActiveLinkContext::Xref);
        let encoded = unchecked_metadata(&contents, &[link]);

        assert!(repair_retained_links(&contents, &encoded, "news", &HashMap::new()).is_err());
    }

    #[test]
    fn repair_rejects_unsupported_external_url_schemes() {
        let expression = "ftp://news.livejournal.com/2.html";
        let contents = format!("{expression}[Two]\n");
        let link = recorded_link(&contents, expression, 2, ActiveLinkContext::Xref);
        let encoded = unchecked_metadata(&contents, &[link]);

        assert!(repair_retained_links(&contents, &encoded, "news", &HashMap::new()).is_err());
    }

    #[test]
    fn repair_accepts_valid_loopback_test_server_post_urls_for_the_requested_journal() {
        let expression = "http://127.0.0.1:1234/news/2.html";
        let contents = format!("{expression}[Two]\n");
        let link = recorded_link(&contents, expression, 2, ActiveLinkContext::Xref);
        let encoded = unchecked_metadata(&contents, &[link]);

        assert_eq!(
            repair_retained_links(
                &contents,
                &encoded,
                "news",
                &HashMap::from([(2, "02.adoc".to_string())]),
            )
            .unwrap(),
            ResolvedLinks {
                contents: "xref:02.adoc[Two]\n".to_string(),
                links: vec![ActiveLink {
                    start: 0,
                    end: "xref:02.adoc".len(),
                    target_id: 2,
                    context: ActiveLinkContext::Xref,
                }],
            }
        );
    }

    #[test]
    fn repair_changes_only_recorded_active_spans_and_preserves_inert_copies() {
        let contents = concat!(
            "xref:2.adoc[active]\n",
            "`xref:2.adoc[inline code]`\n",
            "[source,text,subs=specialcharacters]\n",
            "----\n",
            "xref:2.adoc[listing]\n",
            "----\n",
        );
        let link = recorded_link(contents, "xref:2.adoc", 2, ActiveLinkContext::Xref);
        let encoded = serialize_link_metadata(contents, &[link]).unwrap();
        let expected = concat!(
            "https://news.livejournal.com/2.html[active]\n",
            "`xref:2.adoc[inline code]`\n",
            "[source,text,subs=specialcharacters]\n",
            "----\n",
            "xref:2.adoc[listing]\n",
            "----\n",
        );

        assert_eq!(
            repair_retained_links(contents, &encoded, "news", &HashMap::new()).unwrap(),
            ResolvedLinks {
                contents: expected.to_string(),
                links: vec![ActiveLink {
                    start: 0,
                    end: "https://news.livejournal.com/2.html".len(),
                    target_id: 2,
                    context: ActiveLinkContext::Xref,
                }],
            }
        );
    }

    #[test]
    fn repair_externalizes_recorded_image_link_targets() {
        let target = "{lj-archiver-post-link-prefix}2.adoc";
        let contents = format!("image:x.jpg[,link={target}]\n");
        let link = recorded_link(&contents, target, 2, ActiveLinkContext::ImageLink);
        let encoded = serialize_link_metadata(&contents, &[link]).unwrap();
        let expected = "image:x.jpg[,link=https://news.livejournal.com/2.html]\n";
        let expected_start = "image:x.jpg[,link=".len();

        assert_eq!(
            repair_retained_links(&contents, &encoded, "news", &HashMap::new()).unwrap(),
            ResolvedLinks {
                contents: expected.to_string(),
                links: vec![ActiveLink {
                    start: expected_start,
                    end: expected_start + "https://news.livejournal.com/2.html".len(),
                    target_id: 2,
                    context: ActiveLinkContext::ImageLink,
                }],
            }
        );
    }

    #[test]
    fn repair_retargets_local_links_to_exact_alias_filenames() {
        let contents = "xref:2.adoc[Two]\n";
        let link = recorded_link(contents, "xref:2.adoc", 2, ActiveLinkContext::Xref);
        let encoded = serialize_link_metadata(contents, &[link]).unwrap();

        assert_eq!(
            repair_retained_links(
                contents,
                &encoded,
                "news",
                &HashMap::from([(2, "0002.adoc".to_string())]),
            )
            .unwrap()
            .contents,
            "xref:0002.adoc[Two]\n"
        );
    }

    #[test]
    fn repair_upgrades_same_journal_external_urls_to_local_aliases() {
        let expression = "https://users.livejournal.com/news/2.html";
        let contents = format!("{expression}[Two]\n");
        let link = recorded_link(&contents, expression, 2, ActiveLinkContext::Xref);
        let encoded = serialize_link_metadata(&contents, &[link]).unwrap();

        assert_eq!(
            repair_retained_links(
                &contents,
                &encoded,
                "news",
                &HashMap::from([(2, "02.adoc".to_string())]),
            )
            .unwrap()
            .contents,
            "xref:02.adoc[Two]\n"
        );
    }

    #[test]
    fn repair_refreshes_later_ranges_after_length_changing_replacements() {
        let contents = "xref:1.adoc[One] xref:2.adoc[Two]\n";
        let first = recorded_link(contents, "xref:1.adoc", 1, ActiveLinkContext::Xref);
        let second = recorded_link(contents, "xref:2.adoc", 2, ActiveLinkContext::Xref);
        let encoded = serialize_link_metadata(contents, &[first, second]).unwrap();
        let expected = "https://news.livejournal.com/1.html[One] xref:0002.adoc[Two]\n";
        let second_start = expected.find("xref:0002.adoc").unwrap();

        assert_eq!(
            repair_retained_links(
                contents,
                &encoded,
                "news",
                &HashMap::from([(2, "0002.adoc".to_string())]),
            )
            .unwrap(),
            ResolvedLinks {
                contents: expected.to_string(),
                links: vec![
                    ActiveLink {
                        start: 0,
                        end: "https://news.livejournal.com/1.html".len(),
                        target_id: 1,
                        context: ActiveLinkContext::Xref,
                    },
                    ActiveLink {
                        start: second_start,
                        end: second_start + "xref:0002.adoc".len(),
                        target_id: 2,
                        context: ActiveLinkContext::Xref,
                    },
                ],
            }
        );
    }

    #[test]
    fn decoding_rejects_unknown_top_level_fields() {
        let encoded = format!(
            concat!(
                "{{\"version\":1,\"post_sha256\":\"{}\",\"links\":[],",
                "\"unexpected\":true}}\n"
            ),
            CONTENTS_SHA256
        );

        let error = repair_retained_links(CONTENTS, encoded.as_bytes(), "news", &HashMap::new())
            .unwrap_err();
        assert!(matches!(error, LinkMetadataError::Decode(_)));
    }

    #[test]
    fn decoding_rejects_unknown_link_fields() {
        let encoded = format!(
            concat!(
                "{{\"version\":1,\"post_sha256\":\"{}\",",
                "\"links\":[{{\"start\":0,\"end\":11,\"target_id\":2,",
                "\"context\":\"xref\",\"unexpected\":true}}]}}\n"
            ),
            CONTENTS_SHA256
        );

        let error = repair_retained_links(CONTENTS, encoded.as_bytes(), "news", &HashMap::new())
            .unwrap_err();
        assert!(matches!(error, LinkMetadataError::Decode(_)));
    }

    #[test]
    fn decoding_rejects_unknown_contexts() {
        let encoded = String::from_utf8(valid_metadata())
            .unwrap()
            .replace("\"xref\"", "\"button\"");

        let error = repair_retained_links(CONTENTS, encoded.as_bytes(), "news", &HashMap::new())
            .unwrap_err();
        assert!(matches!(error, LinkMetadataError::Decode(_)));
    }

    #[test]
    fn decoding_rejects_unsupported_versions() {
        let encoded = String::from_utf8(valid_metadata()).unwrap().replacen(
            "\"version\":1",
            "\"version\":2",
            1,
        );

        let error = repair_retained_links(CONTENTS, encoded.as_bytes(), "news", &HashMap::new())
            .unwrap_err();
        assert!(matches!(error, LinkMetadataError::Invalid(_)));
    }

    #[test]
    fn decoding_rejects_uppercase_digests() {
        let encoded = String::from_utf8(valid_metadata())
            .unwrap()
            .replace(CONTENTS_SHA256, &CONTENTS_SHA256.to_ascii_uppercase());

        let error = repair_retained_links(CONTENTS, encoded.as_bytes(), "news", &HashMap::new())
            .unwrap_err();
        assert!(matches!(error, LinkMetadataError::Invalid(_)));
    }

    #[test]
    fn decoding_rejects_short_digests() {
        let encoded = String::from_utf8(valid_metadata())
            .unwrap()
            .replace(CONTENTS_SHA256, &CONTENTS_SHA256[..63]);

        let error = repair_retained_links(CONTENTS, encoded.as_bytes(), "news", &HashMap::new())
            .unwrap_err();
        assert!(matches!(error, LinkMetadataError::Invalid(_)));
    }

    #[test]
    fn decoding_rejects_stale_digests_without_returning_modified_contents() {
        let encoded = String::from_utf8(valid_metadata())
            .unwrap()
            .replace(CONTENTS_SHA256, &"0".repeat(64));

        let error = repair_retained_links(CONTENTS, encoded.as_bytes(), "news", &HashMap::new())
            .unwrap_err();
        assert!(matches!(error, LinkMetadataError::Invalid(_)));
    }

    #[test]
    fn decoding_accepts_valid_version_one_metadata() {
        assert_eq!(
            repair_retained_links(
                CONTENTS,
                &valid_metadata(),
                "news",
                &HashMap::from([(2, "2.adoc".to_string()),])
            )
            .unwrap(),
            ResolvedLinks {
                contents: CONTENTS.to_string(),
                links: vec![ActiveLink {
                    start: 0,
                    end: 11,
                    target_id: 2,
                    context: ActiveLinkContext::Xref,
                }],
            }
        );
    }
}
