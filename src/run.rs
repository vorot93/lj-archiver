use crate::{
    Result,
    adoc::render_post,
    archive::{Archive, ExistingArchive, PostPublication, numeric_post_filename_id},
    cli::Cli,
    client::LjClient,
    discover::discover_post_ids,
    error::LjError,
    link_metadata::repair_retained_links,
    links::{
        LinkRewriter, PendingLink, PendingLinkError, ResolvedLinks, resolve_pending_links,
        validate_pending_links,
    },
    media::rewrite_inline_media,
    models::{ArchiveTarget, ArchivedPost, Post, PostFailure, RunOptions},
    parser::parse_post_page,
    progress::Progress,
    target::{journals_alias, parse_target, post_ref_from_url, post_url},
};
use futures::{StreamExt, stream};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
};
use url::Url;

const MAX_PENDING_RENDER_ATTEMPTS: usize = 4;

#[derive(Debug)]
struct PendingRenderedPost {
    contents: String,
    links: Vec<PendingLink>,
}

fn random_link_namespace(url: &Url) -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| LjError::LinkIntegrity {
        url: url.to_string(),
        reason: format!("could not obtain pending-link namespace: {error}"),
    })?;
    Ok(hex::encode(bytes))
}

fn render_pending_post_with<F>(
    post: &Post,
    include_comments: bool,
    journal: &str,
    targets: Arc<HashMap<u64, String>>,
    mut next_namespace: F,
) -> Result<PendingRenderedPost>
where
    F: FnMut() -> Result<String>,
{
    for attempt in 1..=MAX_PENDING_RENDER_ATTEMPTS {
        let namespace = next_namespace()?;
        let rewriter = LinkRewriter::pending_with_targets(
            journal,
            Arc::clone(&targets),
            post.url.clone(),
            namespace,
        );
        let contents = render_post(post, include_comments, Some(&rewriter));
        let links = rewriter.pending_links();

        match validate_pending_links(&contents, &links) {
            Ok(()) => return Ok(PendingRenderedPost { contents, links }),
            Err(error @ PendingLinkError::OccurrenceCount { .. })
                if attempt == MAX_PENDING_RENDER_ATTEMPTS =>
            {
                return Err(LjError::LinkIntegrity {
                    url: post.url.to_string(),
                    reason: format!(
                        "could not produce collision-free pending links after \
                         {MAX_PENDING_RENDER_ATTEMPTS} attempts: {error}"
                    ),
                });
            }
            Err(PendingLinkError::OccurrenceCount { .. }) => {}
            Err(error) => {
                return Err(LjError::LinkIntegrity {
                    url: post.url.to_string(),
                    reason: error.to_string(),
                });
            }
        }
    }

    unreachable!("pending render loop either succeeds or returns on its final attempt")
}

/// The result of a completed archive run. `Ok(RunOutcome)` means the run wrote
/// what it could; a journal run with some failed posts/pages still returns
/// `Ok` with the counts here. Only a hard failure (target parse, single-post
/// fetch/parse, discovery navigation) yields `Err`.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub root: PathBuf,
    pub archived: usize,
    pub skipped: usize,
    pub failed: usize,
    pub failed_pages: usize,
    pub failures: Vec<PostFailure>,
    pub journal: Option<String>,
}

pub async fn run(cli: Cli) -> Result<()> {
    crate::progress::init_logging(cli.verbose, cli.quiet);
    let progress = Progress::detect(cli.quiet);
    let options = cli.options();
    let outcome = run_archive(&cli.target, &options, &progress).await?;

    if outcome.failed_pages == 0 && outcome.failures.is_empty() {
        Ok(())
    } else {
        Err(LjError::Discovery {
            journal: outcome.journal.unwrap_or_default(),
            reason: failure_reason(outcome.failed_pages, &outcome.failures),
        })
    }
}

/// Parse `target` and archive it. Reusable by the CLI and the GUI. See
/// [`RunOutcome`] for the partial-failure contract.
pub async fn run_archive(
    target: &str,
    options: &RunOptions,
    progress: &Progress,
) -> Result<RunOutcome> {
    match parse_target(target, options.journal_context.as_deref())? {
        ArchiveTarget::Post { journal, id, url } => {
            archive_post(options, progress, journal, id, url).await
        }
        ArchiveTarget::Journal { journal } => archive_journal(options, progress, journal).await,
    }
}

async fn archive_post(
    options: &RunOptions,
    progress: &Progress,
    journal: String,
    id: u64,
    url: Url,
) -> Result<RunOutcome> {
    let root = output_dir(options, &journal);
    let archive = Archive::new(root.clone(), &journal);
    let existing = archive.inspect_existing()?;
    let generate_book =
        options.generate_book || existing.as_ref().is_some_and(|existing| existing.has_book);
    let mut posts = posts_by_id(existing.as_ref());
    let destination = posts
        .get(&id)
        .map(|post| post.path.clone())
        .unwrap_or_else(|| archive.post_path(id));
    let mut target_filenames = trusted_target_filenames(&posts)?;
    target_filenames
        .entry(id)
        .or_insert_with(|| format!("{id}.adoc"));

    progress.single_started(id);
    let client = LjClient::new(options.user_agent.as_deref())?;
    let post = fetch_and_write_post(
        &client,
        options,
        &archive,
        &journal,
        id,
        &url,
        PostWritePolicy {
            destination,
            target_filenames: Arc::new(target_filenames),
        },
    )
    .await?;
    progress.single_done(post.id, &post.path);
    posts.insert(post.id, post);
    write_manifests(&archive, &posts, generate_book)?;

    Ok(RunOutcome {
        root,
        archived: 1,
        skipped: 0,
        failed: 0,
        failed_pages: 0,
        failures: Vec::new(),
        journal: None,
    })
}

async fn archive_journal(
    options: &RunOptions,
    progress: &Progress,
    journal: String,
) -> Result<RunOutcome> {
    archive_journal_with_writer(options, progress, journal, &AtomicFinalPostWriter).await
}

trait FinalPostWriter {
    fn publish_post(
        &self,
        archive: &Archive,
        id: u64,
        destination: &Path,
        resolved: &ResolvedLinks,
    ) -> Result<PostPublication>;
}

struct AtomicFinalPostWriter;

impl FinalPostWriter for AtomicFinalPostWriter {
    fn publish_post(
        &self,
        archive: &Archive,
        _id: u64,
        destination: &Path,
        resolved: &ResolvedLinks,
    ) -> Result<PostPublication> {
        archive.publish_post(destination, resolved)
    }
}

fn warn_metadata_failure(post_id: u64, publication: &PostPublication) {
    if let Some(error) = &publication.metadata_warning {
        tracing::warn!(
            post_id,
            error = %error,
            "post was published but trusted link metadata could not be written"
        );
    }
}

async fn archive_journal_with_writer<W: FinalPostWriter>(
    options: &RunOptions,
    progress: &Progress,
    journal: String,
    writer: &W,
) -> Result<RunOutcome> {
    let start = std::time::Instant::now();
    let root = output_dir(options, &journal);
    let archive = Archive::new(root.clone(), journal.clone());
    let existing = archive.inspect_existing()?;
    let generate_book =
        options.generate_book || existing.as_ref().is_some_and(|existing| existing.has_book);
    let mut posts = posts_by_id(existing.as_ref());
    let trusted_posts = Arc::new(posts.clone());
    let mut target_filenames = trusted_target_filenames(&posts)?;
    let client = LjClient::new(options.user_agent.as_deref())?;

    progress.discovery_started();
    let (ids, failed_pages) =
        discover_post_ids(&client, &journal, &options.base_url, progress).await?;
    progress.archiving_started(ids.len());

    for id in &ids {
        target_filenames
            .entry(*id)
            .or_insert_with(|| format!("{id}.adoc"));
    }
    let journal_context = JournalPostContext {
        journal: journal.clone(),
        target_filenames: Arc::new(target_filenames),
        trusted_posts,
    };

    let outcomes = stream::iter(ids)
        .map(|id| {
            let archive = archive.clone();
            let client = client.clone();
            let options = options.clone();
            let journal_context = journal_context.clone();
            let progress = progress.clone();

            async move {
                archive_journal_post(client, options, archive, id, journal_context, &progress).await
            }
        })
        .buffer_unordered(options.concurrency);
    futures::pin_mut!(outcomes);

    let mut finalized = Vec::new();
    let mut retained = Vec::new();
    let mut failures = Vec::new();
    let mut skipped = 0usize;
    let mut archived = 0usize;
    let no_local_ids = HashSet::new();
    while let Some(outcome) = outcomes.next().await {
        match outcome {
            JournalPostOutcome::Prepared(mut post) => {
                let first_stage =
                    archive
                        .read_pending_post(&post.pending_path)
                        .and_then(|pending| {
                            let external =
                                resolve_pending_links(&pending, &post.links, &no_local_ids)
                                    .map_err(|error| LjError::LinkIntegrity {
                                        url: post.delivered_url.to_string(),
                                        reason: error.to_string(),
                                    })?;
                            writer.publish_post(
                                &archive,
                                post.archived.id,
                                &post.archived.path,
                                &external,
                            )
                        });

                match first_stage {
                    Ok(publication) => {
                        warn_metadata_failure(post.archived.id, &publication);
                        post.archived.path = publication.path;
                        posts.insert(post.archived.id, post.archived.clone());
                        archived += 1;
                        progress.post_done(post.archived.id, &post.archived.title);
                        finalized.push(post);
                    }
                    Err(error) => {
                        progress.post_failed(post.archived.id);
                        failures.push(PostFailure {
                            id: post.archived.id,
                            url: post.delivered_url,
                            message: error.to_string(),
                        });
                        cleanup_pending_post(&archive, post.archived.id, &post.pending_path);
                    }
                }
            }
            JournalPostOutcome::Skipped(post) => {
                posts.insert(post.id, post.clone());
                retained.push(post);
                skipped += 1;
            }
            JournalPostOutcome::Failed(failure) => failures.push(failure),
        }
    }

    let final_ids = posts.keys().copied().collect::<HashSet<_>>();

    for post in &finalized {
        match archive.read_pending_post(&post.pending_path) {
            Ok(pending) => match resolve_pending_links(&pending, &post.links, &final_ids) {
                Ok(local) => {
                    match writer.publish_post(
                        &archive,
                        post.archived.id,
                        &post.archived.path,
                        &local,
                    ) {
                        Ok(publication) => {
                            warn_metadata_failure(post.archived.id, &publication);
                        }
                        Err(error) => {
                            tracing::warn!(
                                post_id = post.archived.id,
                                error = %error,
                                "could not upgrade finalized post links; retaining external links"
                            );
                        }
                    }
                }
                Err(error) => {
                    let error = LjError::LinkIntegrity {
                        url: post.delivered_url.to_string(),
                        reason: error.to_string(),
                    };
                    tracing::warn!(
                        post_id = post.archived.id,
                        error = %error,
                        "could not resolve finalized post links; retaining external links"
                    );
                }
            },
            Err(error) => {
                tracing::warn!(
                    post_id = post.archived.id,
                    error = %error,
                    "could not reread pending post for local link upgrade; retaining external links"
                );
            }
        }
        cleanup_pending_post(&archive, post.archived.id, &post.pending_path);
    }

    let final_targets = trusted_target_filenames(&posts)?;
    for post in &retained {
        repair_retained_post(&archive, writer, &journal, &final_targets, post);
    }

    progress.archiving_finished();

    failures.sort_by_key(|failure| failure.id);
    write_manifests(&archive, &posts, generate_book)?;

    for failure in &failures {
        eprintln!(
            "failed to archive post {} ({}): {}",
            failure.id, failure.url, failure.message
        );
    }

    progress.summary(archived, skipped, failures.len(), start.elapsed());

    Ok(RunOutcome {
        root,
        archived,
        skipped,
        failed: failures.len(),
        failed_pages,
        failures,
        journal: Some(journal),
    })
}

async fn archive_journal_post(
    client: LjClient,
    options: RunOptions,
    archive: Archive,
    id: u64,
    journal_context: JournalPostContext,
    progress: &Progress,
) -> JournalPostOutcome {
    if options.resume
        && let Some(post) = journal_context.trusted_posts.get(&id).cloned()
    {
        progress.post_skipped(id);
        return JournalPostOutcome::Skipped(post);
    }

    let url = match journal_post_url(&journal_context.journal, id, &options.base_url) {
        Ok(url) => url,
        Err(error) => {
            progress.post_failed(id);
            return JournalPostOutcome::Failed(PostFailure {
                id,
                url: options.base_url,
                message: error.to_string(),
            });
        }
    };

    let prepared = match fetch_requested_post(&client, &journal_context.journal, id, &url).await {
        Ok(mut post) => {
            let delivered_url = post.url.clone();
            async {
                rewrite_post_media(&mut post, &options, &archive).await?;
                let destination = journal_context
                    .trusted_posts
                    .get(&id)
                    .map(|post| post.path.clone())
                    .unwrap_or_else(|| archive.post_path(id));
                let namespace_url = post.url.clone();
                let rendered = render_pending_post_with(
                    &post,
                    options.include_comments,
                    &journal_context.journal,
                    journal_context.target_filenames,
                    || random_link_namespace(&namespace_url),
                );
                let rendered = rendered?;
                let pending_path = archive.write_pending_post(id, &rendered.contents)?;

                Result::Ok(PreparedPost {
                    archived: ArchivedPost {
                        id,
                        title: post.title,
                        path: destination,
                    },
                    delivered_url: delivered_url.clone(),
                    pending_path,
                    links: rendered.links,
                })
            }
            .await
            .map_err(|error| (delivered_url, error))
        }
        Err(error) => Err((
            delivered_url_from_error(&error).unwrap_or_else(|| url.clone()),
            error,
        )),
    };

    match prepared {
        Ok(post) => JournalPostOutcome::Prepared(post),
        Err((failure_url, error)) => {
            progress.post_failed(id);
            JournalPostOutcome::Failed(PostFailure {
                id,
                url: failure_url,
                message: error.to_string(),
            })
        }
    }
}

fn cleanup_pending_post(archive: &Archive, id: u64, path: &std::path::Path) {
    if let Err(error) = archive.remove_pending_post(path) {
        tracing::warn!(
            post_id = id,
            error = %error,
            "could not remove finalized pending post"
        );
    }
}

fn repair_retained_post<W: FinalPostWriter>(
    archive: &Archive,
    writer: &W,
    journal: &str,
    final_targets: &HashMap<u64, String>,
    post: &ArchivedPost,
) {
    let contents = match std::fs::read_to_string(&post.path) {
        Ok(contents) => contents,
        Err(source) => {
            let error = LjError::Read {
                path: post.path.clone(),
                source,
            };
            tracing::warn!(
                post_id = post.id,
                error = %error,
                "could not read retained post for trusted link repair; preserving existing bytes"
            );
            return;
        }
    };
    let metadata = match archive.read_link_metadata(&post.path) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => {
            tracing::warn!(
                post_id = post.id,
                "retained post has no trusted link metadata; preserving existing bytes"
            );
            return;
        }
        Err(error) => {
            tracing::warn!(
                post_id = post.id,
                error = %error,
                "could not read trusted link metadata; preserving retained post bytes"
            );
            return;
        }
    };
    let resolved = match repair_retained_links(&contents, &metadata, journal, final_targets) {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::warn!(
                post_id = post.id,
                error = %error,
                "trusted link metadata could not be validated; preserving retained post bytes"
            );
            return;
        }
    };
    if resolved.contents == contents {
        return;
    }

    match writer.publish_post(archive, post.id, &post.path, &resolved) {
        Ok(publication) => warn_metadata_failure(post.id, &publication),
        Err(error) => {
            tracing::warn!(
                post_id = post.id,
                error = %error,
                "could not publish trusted retained-link repair; retaining existing post"
            );
        }
    }
}

fn posts_by_id(existing: Option<&ExistingArchive>) -> BTreeMap<u64, ArchivedPost> {
    existing
        .into_iter()
        .flat_map(|existing| existing.posts.iter().cloned())
        .map(|post| (post.id, post))
        .collect()
}

fn trusted_target_filenames(posts: &BTreeMap<u64, ArchivedPost>) -> Result<HashMap<u64, String>> {
    posts
        .values()
        .map(|post| trusted_target_filename(post).map(|filename| (post.id, filename)))
        .collect()
}

fn trusted_target_filename(post: &ArchivedPost) -> Result<String> {
    let filename = post
        .path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| invalid_trusted_post_path(post, "filename is not valid UTF-8"))?;
    let parsed_id = numeric_post_filename_id(filename)
        .ok_or_else(|| invalid_trusted_post_path(post, "filename is not positive numeric .adoc"))?;
    if parsed_id != post.id {
        return Err(invalid_trusted_post_path(
            post,
            format!("filename identifies post {parsed_id}, expected {}", post.id),
        ));
    }
    Ok(filename.to_string())
}

fn invalid_trusted_post_path(post: &ArchivedPost, reason: impl Into<String>) -> LjError {
    LjError::InvalidArchive {
        path: post.path.clone(),
        reason: reason.into(),
    }
}

fn write_manifests(
    archive: &Archive,
    posts: &BTreeMap<u64, ArchivedPost>,
    generate_book: bool,
) -> Result<()> {
    let posts = posts.values().cloned().collect::<Vec<_>>();
    archive.write_index(&posts)?;
    if generate_book {
        archive.write_book(&posts)?;
    }
    Ok(())
}

async fn fetch_and_write_post(
    client: &LjClient,
    options: &RunOptions,
    archive: &Archive,
    requested_journal: &str,
    requested_id: u64,
    requested_url: &Url,
    write_policy: PostWritePolicy,
) -> Result<ArchivedPost> {
    let mut post =
        fetch_requested_post(client, requested_journal, requested_id, requested_url).await?;
    rewrite_post_media(&mut post, options, archive).await?;
    let final_ids = write_policy
        .target_filenames
        .keys()
        .copied()
        .collect::<HashSet<_>>();
    let namespace_url = post.url.clone();
    let pending = render_pending_post_with(
        &post,
        options.include_comments,
        requested_journal,
        write_policy.target_filenames,
        || random_link_namespace(&namespace_url),
    );
    let pending = pending?;
    let resolved =
        resolve_pending_links(&pending.contents, &pending.links, &final_ids).map_err(|error| {
            LjError::LinkIntegrity {
                url: post.url.to_string(),
                reason: error.to_string(),
            }
        })?;
    let publication = archive.publish_post(&write_policy.destination, &resolved)?;
    warn_metadata_failure(requested_id, &publication);

    Ok(ArchivedPost {
        id: requested_id,
        title: post.title.clone(),
        path: publication.path,
    })
}

struct PostWritePolicy {
    destination: PathBuf,
    target_filenames: Arc<HashMap<u64, String>>,
}

async fn fetch_requested_post(
    client: &LjClient,
    requested_journal: &str,
    requested_id: u64,
    requested_url: &Url,
) -> Result<Post> {
    let page = client.fetch_page(requested_url).await?;
    let post = parse_post_page(&page.text, &page.final_url)?;
    validate_requested_post_identity(requested_journal, requested_id, &post)?;
    Ok(post)
}

fn validate_requested_post_identity(
    requested_journal: &str,
    requested_id: u64,
    post: &Post,
) -> Result<()> {
    let final_identity = final_post_identity(&post.url);
    let final_matches = final_identity.id.is_none_or(|id| id == requested_id)
        && final_identity
            .journal
            .as_deref()
            .is_none_or(|journal| journals_alias(journal, requested_journal));
    let returned_matches = post.id == requested_id;

    if final_matches && returned_matches {
        return Ok(());
    }

    let final_identity = describe_final_post_identity(&final_identity);
    Err(LjError::Parse {
        url: post.url.to_string(),
        reason: format!(
            "post identity mismatch: requested {requested_journal}/{requested_id}; \
             final URL identity {final_identity}; returned parsed identity {}/{}",
            post.journal, post.id
        ),
    })
}

fn final_post_identity(url: &Url) -> FinalPostIdentity {
    if crate::net::is_loopback_test_server(url) {
        return FinalPostIdentity {
            journal: None,
            id: final_numeric_path_id(url),
        };
    }

    FinalPostIdentity {
        journal: final_livejournal_journal(url),
        id: post_ref_from_url(url).map(|(_, id)| id),
    }
}

fn final_numeric_path_id(url: &Url) -> Option<u64> {
    let segment = url.path_segments()?.rfind(|segment| !segment.is_empty())?;
    let id = segment
        .strip_suffix(".html")
        .unwrap_or(segment)
        .parse()
        .ok()?;
    (id > 0).then_some(id)
}

fn final_livejournal_journal(url: &Url) -> Option<String> {
    let host = crate::net::normalized_domain_host(url)?;
    if host.eq_ignore_ascii_case("users.livejournal.com") {
        return url
            .path_segments()?
            .find(|segment| !segment.is_empty())
            .map(ToOwned::to_owned);
    }

    host.strip_suffix(".livejournal.com")
        .filter(|journal| !journal.is_empty() && !journal.eq_ignore_ascii_case("www"))
        .map(ToOwned::to_owned)
}

fn describe_final_post_identity(identity: &FinalPostIdentity) -> String {
    match (identity.journal.as_deref(), identity.id) {
        (Some(journal), Some(id)) => format!("{journal}/{id}"),
        (Some(journal), None) => journal.to_string(),
        (None, Some(id)) => format!("post ID {id}"),
        (None, None) => "unrecognized".to_string(),
    }
}

fn delivered_url_from_error(error: &LjError) -> Option<Url> {
    match error {
        LjError::Http { url, .. }
        | LjError::HttpStatus { url, .. }
        | LjError::ResponseTooLarge { url, .. }
        | LjError::Parse { url, .. }
        | LjError::AgeGated { url } => Url::parse(url).ok(),
        _ => None,
    }
}

fn output_dir(options: &RunOptions, journal: &str) -> std::path::PathBuf {
    options
        .output_dir
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("archive").join(journal))
}

fn journal_post_url(journal: &str, id: u64, base_url: &Url) -> Result<Url> {
    if crate::net::is_loopback_test_server(base_url) {
        return base_url
            .join(&format!("{journal}/{id}.html"))
            .map_err(|error| LjError::Discovery {
                journal: journal.to_string(),
                reason: error.to_string(),
            });
    }

    post_url(journal, id)
}

fn failure_summary(failures: &[PostFailure]) -> String {
    let ids = failures
        .iter()
        .map(|failure| failure.id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} post(s) failed: {ids}", failures.len())
}

/// Builds the combined discovery-error reason from archive-page failures
/// and/or post failures. Callers must only invoke this when at least one of
/// `failed_pages` or `failures` is non-empty.
fn failure_reason(failed_pages: usize, failures: &[PostFailure]) -> String {
    debug_assert!(failed_pages > 0 || !failures.is_empty());

    let mut reasons = Vec::new();
    if failed_pages > 0 {
        reasons.push(format!(
            "{failed_pages} archive page(s) failed to fetch; archive may be incomplete"
        ));
    }
    if !failures.is_empty() {
        reasons.push(failure_summary(failures));
    }
    reasons.join("; ")
}

struct PreparedPost {
    archived: ArchivedPost,
    delivered_url: Url,
    pending_path: PathBuf,
    links: Vec<PendingLink>,
}

#[derive(Clone)]
struct JournalPostContext {
    journal: String,
    target_filenames: Arc<HashMap<u64, String>>,
    trusted_posts: Arc<BTreeMap<u64, ArchivedPost>>,
}

enum JournalPostOutcome {
    Prepared(PreparedPost),
    Skipped(ArchivedPost),
    Failed(PostFailure),
}

struct FinalPostIdentity {
    journal: Option<String>,
    id: Option<u64>,
}

async fn rewrite_post_media(
    post: &mut Post,
    options: &RunOptions,
    archive: &Archive,
) -> Result<()> {
    let assets_dir = archive.assets_dir();

    let rewritten = rewrite_inline_media(
        &post.body_html,
        &post.url,
        &assets_dir,
        options.download_media,
        options.user_agent.as_deref(),
    )
    .await?;
    post.body_html = rewritten.html;

    if options.include_comments {
        for comment in &mut post.comments {
            let rewritten = rewrite_inline_media(
                &comment.body_html,
                &post.url,
                &assets_dir,
                options.download_media,
                options.user_agent.as_deref(),
            )
            .await?;
            comment.body_html = rewritten.html;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{archive::Archive, cli::Cli, progress::ProgressEvent};
    use sha2::{Digest, Sha256};
    use std::{
        collections::{HashMap, HashSet},
        sync::{Condvar, Mutex},
    };
    use tempfile::tempdir;
    use url::Url;
    use wiremock::{
        Mock, MockServer, Request, Respond, ResponseTemplate,
        matchers::{method, path},
    };

    const COLLIDING_NAMESPACE: &str = "00112233445566778899aabbccddeeff";
    const SAFE_NAMESPACE: &str = "ffeeddccbbaa99887766554433221100";
    const PENDING_SEPARATOR: char = '\u{1f}';

    #[test]
    fn failure_reason_reports_failed_archive_pages() {
        let reason = failure_reason(2, &[]);
        assert!(
            reason.contains("2 archive page(s) failed to fetch"),
            "unexpected reason: {reason}"
        );
    }

    #[test]
    fn pending_render_retries_collision_then_succeeds() {
        let namespaces = [COLLIDING_NAMESPACE, SAFE_NAMESPACE];
        let mut calls = 0;
        let rendered = render_pending_post_with(
            &post_containing_collision(COLLIDING_NAMESPACE),
            false,
            "news",
            Arc::new(HashMap::from([(7, "7.adoc".to_string())])),
            || {
                let value = namespaces[calls];
                calls += 1;
                Ok(value.to_string())
            },
        )
        .unwrap();

        assert_eq!(calls, 2);
        assert_eq!(rendered.links.len(), 1);
        assert!(
            rendered.contents.contains(SAFE_NAMESPACE),
            "got: {:?}",
            rendered.contents
        );
    }

    #[test]
    fn pending_render_retries_shared_boundary_collision_without_generated_residual() {
        let namespaces = [COLLIDING_NAMESPACE, SAFE_NAMESPACE];
        let mut calls = 0;
        let rendered = render_pending_post_with(
            &post_containing_shared_boundary_prefix(COLLIDING_NAMESPACE),
            false,
            "news",
            Arc::new(HashMap::from([(7, "7.adoc".to_string())])),
            || {
                let value = namespaces[calls];
                calls += 1;
                Ok(value.to_string())
            },
        )
        .unwrap();

        assert_eq!(calls, 2);
        validate_pending_links(&rendered.contents, &rendered.links).unwrap();
        let safe_token = rendered.links[0].token.clone();
        let safe_body = &safe_token
            [PENDING_SEPARATOR.len_utf8()..safe_token.len() - PENDING_SEPARATOR.len_utf8()];
        let resolved =
            resolve_pending_links(&rendered.contents, &rendered.links, &HashSet::from([7]))
                .unwrap()
                .contents;

        assert!(resolved.contains("xref:7.adoc[Seven]"), "got: {resolved:?}");
        assert!(!resolved.contains(&safe_token), "got: {resolved:?}");
        assert!(
            !resolved.contains(&format!("{safe_body}{PENDING_SEPARATOR}")),
            "got: {resolved:?}"
        );
    }

    #[test]
    fn pending_render_exhausts_after_four_colliding_namespaces() {
        let post = post_containing_collision(COLLIDING_NAMESPACE);
        let mut calls = 0;
        let error = render_pending_post_with(
            &post,
            false,
            "news",
            Arc::new(HashMap::from([(7, "7.adoc".to_string())])),
            || {
                calls += 1;
                Ok(COLLIDING_NAMESPACE.to_string())
            },
        )
        .unwrap_err();

        assert_eq!(calls, 4);
        match error {
            LjError::LinkIntegrity { url, reason } => {
                assert_eq!(url, post.url.as_str());
                assert!(reason.contains("after 4 attempts"), "{reason}");
                assert!(reason.contains("occurrence count was 2"), "{reason}");
            }
            other => panic!("expected link integrity error, got {other:?}"),
        }
    }

    #[test]
    fn pending_render_preserves_namespace_source_failure_url_context() {
        let post = parsed_post("https://news.livejournal.com/9.html", "news", 9);
        let mut calls = 0;
        let error =
            render_pending_post_with(&post, false, "news", Arc::new(HashMap::new()), || {
                calls += 1;
                Err(LjError::LinkIntegrity {
                    url: post.url.to_string(),
                    reason: "could not obtain pending-link namespace: unavailable".to_string(),
                })
            })
            .unwrap_err();

        assert_eq!(calls, 1);
        match error {
            LjError::LinkIntegrity { url, reason } => {
                assert_eq!(url, post.url.as_str());
                assert_eq!(
                    reason,
                    "could not obtain pending-link namespace: unavailable"
                );
            }
            other => panic!("expected link integrity error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn archives_single_post_from_local_server() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/166511.html"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("../tests/fixtures/post_basic.html")),
            )
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let cli = Cli {
            target: format!("{}/news/166511.html", server.uri()),
            output_dir: Some(dir.path().join("out")),
            no_comments: false,
            no_media: true,
            concurrency: 2,
            resume: false,
            book: false,
            journal: None,
            verbose: false,
            quiet: false,
            user_agent: None,
            base_url: Url::parse(&server.uri()).unwrap(),
        };

        run(cli).await.unwrap();

        let post = std::fs::read_to_string(dir.path().join("out/posts/166511.adoc")).unwrap();
        assert!(post.contains("= Post Title"));
        assert!(post.contains("Hello *world*."));
    }

    #[tokio::test]
    async fn single_post_same_id_update_preserves_alias_path_manifests_and_next_run() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                "<p>Updated alias body.</p>",
                "",
            )))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let output = dir.path().join("out");
        let baseline = "= Existing Alias\n\nOld bytes.\n";
        seed_existing_archive_at(
            dir.path(),
            &[(1, "01.adoc", "Existing Alias", baseline)],
            true,
        );
        let options = test_cli(&server, dir.path(), "1", false, true).options();
        let target = format!("{}/news/1.html", server.uri());
        let expected_post = format!(
            concat!(
                "= Post Title\n\n",
                ":lj-id: 1\n",
                ":lj-url: {}\n",
                ":lj-author: news\n",
                "ifndef::lj-archiver-post-link-prefix",
                "[:lj-archiver-post-link-prefix:]\n\n",
                "Updated alias body.\n",
            ),
            target
        );

        let first = run_archive(&target, &options, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(first.archived, 1);
        assert_eq!(
            std::fs::read_to_string(output.join("posts/01.adoc")).unwrap(),
            expected_post
        );
        assert!(!output.join("posts/1.adoc").exists());
        assert_eq!(
            std::fs::read_to_string(output.join("index.adoc")).unwrap(),
            concat!(
                "= news Archive\n\n",
                "|===\n|Post ID |Title\n\n",
                "|1 |xref:posts/01.adoc[Post Title]\n",
                "|===\n",
            )
        );
        assert_eq!(
            std::fs::read_to_string(output.join("book.adoc")).unwrap(),
            concat!(
                "= news Archive\n",
                ":doctype: book\n",
                ":toc:\n",
                ":imagesdir: posts\n",
                ":lj-archiver-post-link-prefix: posts/\n\n",
                "include::index.adoc[leveloffset=+1]\n",
                "\ninclude::posts/01.adoc[leveloffset=+1]\n",
            )
        );
        let inspected = Archive::new(&output, "news")
            .inspect_existing()
            .unwrap()
            .unwrap();
        assert_eq!(inspected.posts.len(), 1);
        assert_eq!(inspected.posts[0].id, 1);
        assert_eq!(inspected.posts[0].path, output.join("posts/01.adoc"));

        let second = run_archive(&target, &options, &Progress::hidden())
            .await
            .unwrap();
        assert_eq!(second.archived, 1);
        assert_eq!(
            std::fs::read_to_string(output.join("posts/01.adoc")).unwrap(),
            expected_post
        );
        assert!(!output.join("posts/1.adoc").exists());
    }

    #[tokio::test]
    async fn resume_does_not_skip_a_post_absent_from_the_inspected_snapshot() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                "<p>Fetched after inspection.</p>",
                "",
            )))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let archive = Archive::new(dir.path().join("out"), "news");
        let journal_context = JournalPostContext {
            journal: "news".to_string(),
            target_filenames: Arc::new(HashMap::from([(1, "1.adoc".to_string())])),
            trusted_posts: Arc::new(BTreeMap::new()),
        };

        archive
            .write_post(1, "= Late Unverified File\n\nMust not trigger a skip.\n")
            .unwrap();
        let options = journal_cli(&server, dir.path(), true).options();
        let (tx, rx) = futures::channel::mpsc::channel(128);
        let progress = Progress::channel(tx);

        let outcome = archive_journal_post(
            LjClient::new(None).unwrap(),
            options,
            archive.clone(),
            1,
            journal_context,
            &progress,
        )
        .await;

        let pending_path = match outcome {
            JournalPostOutcome::Prepared(post) => {
                assert_eq!(post.archived.id, 1);
                assert_eq!(post.archived.title, "Post Title");
                post.pending_path
            }
            JournalPostOutcome::Skipped(post) => {
                panic!("late unverified post {} was incorrectly skipped", post.id)
            }
            JournalPostOutcome::Failed(failure) => {
                panic!(
                    "post {} unexpectedly failed: {}",
                    failure.id, failure.message
                )
            }
        };
        drop(progress);
        let events: Vec<_> = rx.collect().await;

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|request| request.url.path())
                .collect::<Vec<_>>(),
            vec!["/news/1.html"]
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProgressEvent::PostSkipped { id: 1 })),
            "{events:?}"
        );
        archive.remove_pending_post(&pending_path).unwrap();
    }

    #[tokio::test]
    async fn journal_rejects_returned_identity_collision_and_uses_final_failure_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"<a href="/news/1.html">One</a><a href="/news/2.html">Two</a>"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "/delivered/1.html?source=returned-2"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/delivered/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "2",
                "<p>Returned as post two.</p>",
                "",
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/2.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "2",
                "<p>Actual post two.</p>",
                "",
            )))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let mut options = journal_cli(&server, dir.path(), false).options();
        options.generate_book = true;

        let outcome = run_archive("news", &options, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(outcome.archived, 1);
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.failures.len(), 1);
        let failure = &outcome.failures[0];
        assert_eq!(failure.id, 1);
        assert_eq!(
            failure.url.as_str(),
            format!("{}/delivered/1.html?source=returned-2", server.uri())
        );
        assert!(failure.message.contains("requested news/1"), "{failure:?}");
        assert!(
            failure.message.contains("returned parsed identity news/2"),
            "{failure:?}"
        );

        let mut post_files = std::fs::read_dir(dir.path().join("out/posts"))
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                (path.extension().and_then(|extension| extension.to_str()) == Some("adoc"))
                    .then_some(path.file_name().unwrap().to_owned())
            })
            .collect::<Vec<_>>();
        post_files.sort();
        assert_eq!(post_files, [std::ffi::OsString::from("2.adoc")]);
        assert!(!dir.path().join("out/posts/1.adoc").exists());
        let post = std::fs::read_to_string(dir.path().join("out/posts/2.adoc")).unwrap();
        assert_eq!(post.matches(":lj-id: 2").count(), 1, "{post}");

        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert_eq!(index.matches("|2 |xref:posts/2.adoc").count(), 1, "{index}");
        assert!(!index.contains("|1 |xref:posts/1.adoc"), "{index}");
        let book = std::fs::read_to_string(dir.path().join("out/book.adoc")).unwrap();
        assert_eq!(book.matches("include::posts/2.adoc").count(), 1, "{book}");
        assert!(!book.contains("include::posts/1.adoc"), "{book}");
        assert!(pending_post_paths(dir.path().join("out/posts")).is_empty());
    }

    #[tokio::test]
    async fn journal_status_failure_uses_delivered_final_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/news/1.html">One</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "/delivered/failure?source=status"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/delivered/failure"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let options = journal_cli(&server, dir.path(), false).options();

        let outcome = run_archive("news", &options, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(outcome.archived, 0);
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.failures.len(), 1);
        let failure = &outcome.failures[0];
        assert_eq!(failure.id, 1);
        assert_eq!(
            failure.url.as_str(),
            format!("{}/delivered/failure?source=status", server.uri())
        );
        assert!(failure.message.contains("404"), "{failure:?}");
        assert!(!dir.path().join("out/posts/1.adoc").exists());
    }

    #[tokio::test]
    async fn single_post_rejects_returned_identity_without_writing_archive_files() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "2",
                "<p>Wrong post.</p>",
                "",
            )))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let mut options = test_cli(&server, dir.path(), "1", false, true).options();
        options.generate_book = true;
        let requested_url = format!("{}/news/1.html", server.uri());

        let error = run_archive(&requested_url, &options, &Progress::hidden())
            .await
            .unwrap_err();

        match error {
            LjError::Parse { url, reason } => {
                assert_eq!(url, requested_url);
                assert!(reason.contains("requested news/1"), "{reason}");
                assert!(reason.contains("final URL identity post ID 1"), "{reason}");
                assert!(
                    reason.contains("returned parsed identity news/2"),
                    "{reason}"
                );
            }
            other => panic!("expected parse error, got {other:?}"),
        }

        for relative in [
            "posts/1.adoc",
            "posts/2.adoc",
            "index.adoc",
            "book.adoc",
            "posts/1.pending",
            "posts/2.pending",
        ] {
            assert!(
                !dir.path().join("out").join(relative).exists(),
                "{relative}"
            );
        }
    }

    #[tokio::test]
    async fn single_post_uses_delivered_final_url_after_redirect() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "/delivered/1.html?source=redirect"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/delivered/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                "<p>Delivered body.</p>",
                "",
            )))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let options = test_cli(&server, dir.path(), "1", false, true).options();
        let requested_url = format!("{}/news/1.html", server.uri());
        let final_url = format!("{}/delivered/1.html?source=redirect", server.uri());

        run_archive(&requested_url, &options, &Progress::hidden())
            .await
            .unwrap();

        let post = std::fs::read_to_string(dir.path().join("out/posts/1.adoc")).unwrap();
        assert!(post.contains(&format!(":lj-url: {final_url}")), "{post}");
        assert!(
            !post.contains(&format!(":lj-url: {requested_url}")),
            "{post}"
        );
    }

    #[tokio::test]
    async fn single_post_resolves_links_and_no_media_images_from_delivered_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "/delivered/nested/1.html"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/delivered/nested/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                concat!(
                    r#"<p><a href="next.html">next</a> "#,
                    r#"<a href="/profile">profile</a> "#,
                    r#"<a href="javascript:alert(1)">unsafe</a> "#,
                    r#"<img src="picture.jpg" alt="picture"> "#,
                    r#"<a href="https://news.livejournal.com/1.html">self</a> "#,
                    r#"<a href="https://news.livejournal.com/1.html">"#,
                    r#"<img src="linked.jpg" alt="linked"></a></p>"#,
                ),
                "",
            )))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let options = test_cli(&server, dir.path(), "1", false, true).options();

        run_archive(
            &format!("{}/news/1.html", server.uri()),
            &options,
            &Progress::hidden(),
        )
        .await
        .unwrap();

        let post = std::fs::read_to_string(dir.path().join("out/posts/1.adoc")).unwrap();
        assert!(
            post.contains(&format!(
                "{}/delivered/nested/next.html[next]",
                server.uri()
            )),
            "got: {post}"
        );
        assert!(
            post.contains(&format!("{}/profile[profile]", server.uri())),
            "got: {post}"
        );
        assert!(!post.contains("javascript:"), "got: {post}");
        assert!(
            post.contains(&format!(
                "image::{}/delivered/nested/picture.jpg[picture]",
                server.uri()
            )),
            "got: {post}"
        );
        assert!(post.contains("xref:1.adoc[self]"), "got: {post}");
        assert!(
            post.contains("link={lj-archiver-post-link-prefix}1.adoc"),
            "got: {post}"
        );
        let post_path = dir.path().join("out/posts/1.adoc");
        let metadata = read_matching_link_metadata(&post_path);
        assert_eq!(metadata["links"].as_array().unwrap().len(), 2);
        assert_eq!(
            metadata_expressions(&post, &metadata),
            ["xref:1.adoc", "{lj-archiver-post-link-prefix}1.adoc"]
        );
        assert_eq!(metadata["links"][0]["context"], "xref");
        assert_eq!(metadata["links"][1]["context"], "image_link");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn journal_post_resolves_links_from_each_delivered_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/news/1.html">One</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "/delivered/nested/1.html"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/delivered/nested/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                r#"<p><a href="next.html">next</a></p>"#,
                "",
            )))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let options = journal_cli(&server, dir.path(), false).options();

        run_archive("news", &options, &Progress::hidden())
            .await
            .unwrap();

        let post = std::fs::read_to_string(dir.path().join("out/posts/1.adoc")).unwrap();
        assert!(
            post.contains(&format!(
                "{}/delivered/nested/next.html[next]",
                server.uri()
            )),
            "got: {post}"
        );
        assert!(
            !post.contains("https://news.livejournal.com/next.html"),
            "got: {post}"
        );
    }

    #[tokio::test]
    async fn single_post_rejects_wrong_numeric_final_url_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "/delivered/2?source=wrong-id"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/delivered/2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                "<p>Correct article under a wrong final path.</p>",
                "",
            )))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let options = test_cli(&server, dir.path(), "1", false, true).options();
        let requested_url = format!("{}/news/1.html", server.uri());
        let final_url = format!("{}/delivered/2?source=wrong-id", server.uri());

        let error = run_archive(&requested_url, &options, &Progress::hidden())
            .await
            .unwrap_err();

        match error {
            LjError::Parse { url, reason } => {
                assert_eq!(url, final_url);
                assert!(reason.contains("requested news/1"), "{reason}");
                assert!(reason.contains("final URL identity post ID 2"), "{reason}");
                assert!(
                    reason.contains("returned parsed identity news/1"),
                    "{reason}"
                );
            }
            other => panic!("expected parse error, got {other:?}"),
        }
        assert!(!dir.path().join("out/posts/1.adoc").exists());
        assert!(!dir.path().join("out/index.adoc").exists());
    }

    #[test]
    fn final_livejournal_journal_mismatch_is_rejected_on_non_post_path() {
        let post = parsed_post(
            "https://other.livejournal.com/landing?source=redirect",
            "canonical_metadata",
            1,
        );

        let error = validate_requested_post_identity("news", 1, &post).unwrap_err();

        match error {
            LjError::Parse { url, reason } => {
                assert_eq!(url, "https://other.livejournal.com/landing?source=redirect");
                assert!(reason.contains("requested news/1"), "{reason}");
                assert!(reason.contains("final URL identity other"), "{reason}");
                assert!(
                    reason.contains("returned parsed identity canonical_metadata/1"),
                    "{reason}"
                );
            }
            other => panic!("expected parse error, got {other:?}"),
        }
    }

    #[test]
    fn dns_trailing_dot_final_livejournal_journal_mismatch_is_rejected() {
        let post = parsed_post(
            "https://other.livejournal.com./landing?source=redirect",
            "canonical_metadata",
            1,
        );

        let error = validate_requested_post_identity("news", 1, &post).unwrap_err();

        match error {
            LjError::Parse { url, reason } => {
                assert_eq!(
                    url,
                    "https://other.livejournal.com./landing?source=redirect"
                );
                assert!(reason.contains("requested news/1"), "{reason}");
                assert!(reason.contains("final URL identity other"), "{reason}");
            }
            other => panic!("expected parse error, got {other:?}"),
        }
    }

    #[test]
    fn dns_trailing_dot_final_livejournal_post_id_mismatch_is_rejected() {
        let post = parsed_post(
            "https://news.livejournal.com./2.html?source=redirect",
            "news",
            1,
        );

        let error = validate_requested_post_identity("news", 1, &post).unwrap_err();

        match error {
            LjError::Parse { reason, .. } => {
                assert!(reason.contains("requested news/1"), "{reason}");
                assert!(reason.contains("final URL identity news/2"), "{reason}");
            }
            other => panic!("expected parse error, got {other:?}"),
        }
    }

    #[test]
    fn dns_trailing_dot_valid_final_livejournal_alias_is_accepted() {
        for url in [
            "https://evo-lutio.livejournal.com./240331.html",
            "https://users.livejournal.com./evo_lutio/240331.html",
        ] {
            let post = parsed_post(url, "canonical_metadata", 240331);

            validate_requested_post_identity("evo_lutio", 240331, &post).unwrap();
        }
    }

    #[test]
    fn final_users_livejournal_journal_alias_is_accepted_on_non_post_path() {
        let post = parsed_post(
            "https://users.livejournal.com/evo-lutio/landing?source=redirect",
            "canonical_metadata",
            240331,
        );

        validate_requested_post_identity("evo_lutio", 240331, &post).unwrap();
    }

    #[tokio::test]
    async fn archives_journal_and_skips_existing_posts_with_resume() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("../tests/fixtures/journal_calendar.html")),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("../tests/fixtures/journal_month.html")),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/166511.html"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("../tests/fixtures/post_basic.html")),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/166512.html"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("../tests/fixtures/post_with_comments.html")),
            )
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let cli = Cli {
            target: "news".to_string(),
            output_dir: Some(dir.path().join("out")),
            no_comments: false,
            no_media: true,
            concurrency: 2,
            resume: true,
            book: false,
            journal: None,
            verbose: false,
            quiet: false,
            user_agent: None,
            base_url: Url::parse(&server.uri()).unwrap(),
        };

        run(cli).await.unwrap();

        assert!(dir.path().join("out/posts/166511.adoc").exists());
        assert!(dir.path().join("out/posts/166512.adoc").exists());
        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert!(index.contains("xref:posts/166511.adoc"));
        assert!(index.contains("xref:posts/166512.adoc"));
    }

    #[tokio::test]
    async fn resume_skips_existing_journal_posts_without_fetching_them() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<a href="/2024/12/">December 2024</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a href="/news/166511.html">Existing</a><a href="/news/166512.html">New</a>"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/166512.html"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("../tests/fixtures/post_with_comments.html")),
            )
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        seed_existing_archive(
            dir.path(),
            &[(166511, "Existing", "= Existing\n\nBody\n")],
            false,
        );
        let existing_post = dir.path().join("out/posts/166511.adoc");
        let cli = journal_cli(&server, dir.path(), true);

        run(cli).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(&existing_post).unwrap(),
            "= Existing\n\nBody\n"
        );
        assert!(dir.path().join("out/posts/166512.adoc").exists());
        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert!(index.contains("xref:posts/166511.adoc[Existing]"));
        assert!(index.contains("xref:posts/166512.adoc"));

        let requests = server.received_requests().await.unwrap();
        let paths: Vec<_> = requests
            .iter()
            .map(|request| request.url.path().to_string())
            .collect();
        assert!(!paths.contains(&"/news/166511.html".to_string()));
        assert!(paths.contains(&"/news/166512.html".to_string()));
    }

    #[tokio::test]
    async fn journal_archiving_continues_after_post_failure_and_summarizes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<a href="/2024/12/">December 2024</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a href="/news/166511.html">Good</a><a href="/news/166512.html">Bad</a>"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/166511.html"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("../tests/fixtures/post_basic.html")),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/166512.html"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let cli = journal_cli(&server, dir.path(), false);

        let error = run(cli).await.unwrap_err();

        match error {
            LjError::Discovery { journal, reason } => {
                assert_eq!(journal, "news");
                assert!(reason.contains("1 post(s) failed"));
                assert!(reason.contains("166512"));
            }
            other => panic!("expected discovery error, got {other:?}"),
        }
        assert!(dir.path().join("out/posts/166511.adoc").exists());
        assert!(!dir.path().join("out/posts/166512.adoc").exists());
        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert!(index.contains("xref:posts/166511.adoc"));
        assert!(!index.contains("xref:posts/166512.adoc"));
    }

    #[tokio::test]
    async fn run_archive_returns_outcome_for_single_post() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/166511.html"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("../tests/fixtures/post_basic.html")),
            )
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let options = RunOptions {
            output_dir: Some(dir.path().join("out")),
            include_comments: true,
            download_media: false,
            concurrency: 2,
            resume: false,
            generate_book: false,
            journal_context: None,
            user_agent: None,
            base_url: Url::parse(&server.uri()).unwrap(),
        };
        let progress = Progress::hidden();

        let outcome = run_archive(
            &format!("{}/news/166511.html", server.uri()),
            &options,
            &progress,
        )
        .await
        .unwrap();

        assert_eq!(outcome.archived, 1);
        assert_eq!(outcome.failed, 0);
        assert!(outcome.failures.is_empty());
        assert_eq!(outcome.root, dir.path().join("out"));
    }

    #[tokio::test]
    async fn run_archive_is_ok_with_failed_count_on_partial_journal_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a href="/news/166511.html">Good</a><a href="/news/166512.html">Bad</a>"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/166511.html"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("../tests/fixtures/post_basic.html")),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/166512.html"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let options = RunOptions {
            output_dir: Some(dir.path().join("out")),
            include_comments: true,
            download_media: true,
            concurrency: 2,
            resume: false,
            generate_book: false,
            journal_context: None,
            user_agent: None,
            base_url: Url::parse(&server.uri()).unwrap(),
        };

        let outcome = run_archive("news", &options, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(outcome.failed, 1);
        assert!(outcome.failures.iter().any(|f| f.id == 166512));
        assert_eq!(outcome.journal.as_deref(), Some("news"));
        assert!(dir.path().join("out/posts/166511.adoc").exists());
    }

    #[tokio::test]
    async fn partial_rediscovery_retains_baseline_manifests_counts_and_local_links() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"<a href="/2024/12/">Old</a><a href="/2025/01/">Current</a>"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2025/01/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/news/2.html">Two</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/2.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "2",
                r#"<p><a href="https://news.livejournal.com/1.html">Post one</a></p>"#,
                "",
            )))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let baseline = "= Existing One\n\n:lj-id: 1\n\nOriginal body.\n";
        seed_existing_archive(dir.path(), &[(1, "Existing One", baseline)], true);
        let options = journal_cli(&server, dir.path(), false).options();
        let (tx, rx) = futures::channel::mpsc::channel(128);
        let progress = Progress::channel(tx);

        let outcome = run_archive("news", &options, &progress).await.unwrap();
        drop(progress);
        let events: Vec<_> = rx.collect().await;

        assert_eq!(outcome.archived, 1);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.failed, 0);
        assert_eq!(outcome.failed_pages, 1);
        assert!(outcome.failures.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/posts/1.adoc")).unwrap(),
            baseline
        );
        let post_two = std::fs::read_to_string(dir.path().join("out/posts/2.adoc")).unwrap();
        assert!(post_two.contains("xref:1.adoc[Post one]"), "{post_two}");
        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert_eq!(index.matches("|1 |xref:posts/1.adoc").count(), 1, "{index}");
        assert_eq!(index.matches("|2 |xref:posts/2.adoc").count(), 1, "{index}");
        assert!(index.find("|1 |").unwrap() < index.find("|2 |").unwrap());
        let book = std::fs::read_to_string(dir.path().join("out/book.adoc")).unwrap();
        assert_eq!(book.matches("include::posts/1.adoc").count(), 1, "{book}");
        assert_eq!(book.matches("include::posts/2.adoc").count(), 1, "{book}");
        assert!(
            book.find("include::posts/1.adoc").unwrap()
                < book.find("include::posts/2.adoc").unwrap()
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ProgressEvent::Summary {
                    archived: 1,
                    skipped: 0,
                    failed: 0,
                    ..
                }
            )
        }));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProgressEvent::PostDone { id: 2, .. }))
                .count(),
            1
        );
        assert!(pending_post_paths(dir.path().join("out/posts")).is_empty());
    }

    #[tokio::test]
    async fn partial_rediscovery_retains_omitted_alias_and_targets_it_from_text_and_image_links() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"<a href="/2024/12/">Old</a><a href="/2025/01/">Current</a>"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2025/01/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/news/2.html">Two</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/2.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "2",
                concat!(
                    r#"<p><a href="https://news.livejournal.com/1.html">Post one</a> "#,
                    r#"<a href="https://news.livejournal.com/1.html">"#,
                    r#"<img src="https://cdn.example.test/photo.jpg" alt="photo"></a></p>"#,
                ),
                "",
            )))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let baseline = "= Existing Alias\n\nOriginal alias body.\n";
        seed_existing_archive_at(
            dir.path(),
            &[(1, "01.adoc", "Existing Alias", baseline)],
            true,
        );
        let options = journal_cli(&server, dir.path(), false).options();

        let outcome = run_archive("news", &options, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(outcome.archived, 1);
        assert_eq!(outcome.failed_pages, 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/posts/01.adoc")).unwrap(),
            baseline
        );
        assert!(!dir.path().join("out/posts/1.adoc").exists());
        let post_two = std::fs::read_to_string(dir.path().join("out/posts/2.adoc")).unwrap();
        assert_eq!(
            post_two.matches("xref:01.adoc[Post one]").count(),
            1,
            "{post_two}"
        );
        assert_eq!(
            post_two
                .matches("link={lj-archiver-post-link-prefix}01.adoc")
                .count(),
            1,
            "{post_two}"
        );
        assert!(!post_two.contains("xref:1.adoc"), "{post_two}");
        assert!(!post_two.contains("post-link-prefix}1.adoc"), "{post_two}");
        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert_eq!(
            index.matches("|1 |xref:posts/01.adoc").count(),
            1,
            "{index}"
        );
        assert_eq!(index.matches("|2 |xref:posts/2.adoc").count(), 1, "{index}");
        let book = std::fs::read_to_string(dir.path().join("out/book.adoc")).unwrap();
        assert_eq!(book.matches("include::posts/01.adoc").count(), 1, "{book}");
        assert_eq!(book.matches("include::posts/2.adoc").count(), 1, "{book}");
    }

    #[tokio::test]
    async fn single_post_rejects_unverified_output_before_requests_or_writes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                "<p>Should not be fetched.</p>",
                "",
            )))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("unverified.txt"), "unchanged").unwrap();
        let options = test_cli(&server, dir.path(), "1", false, true).options();

        let error = run_archive(
            &format!("{}/news/1.html", server.uri()),
            &options,
            &Progress::hidden(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, LjError::InvalidArchive { .. }), "{error:?}");
        assert!(server.received_requests().await.unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(output.join("unverified.txt")).unwrap(),
            "unchanged"
        );
        assert!(!output.join("posts").exists());
        assert!(!output.join("index.adoc").exists());
        assert!(!output.join("book.adoc").exists());
    }

    #[tokio::test]
    async fn journal_rejects_mismatched_output_before_requests_or_writes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(ResponseTemplate::new(200).set_body_string("unused"))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let output = dir.path().join("out");
        std::fs::create_dir(&output).unwrap();
        let mismatched_index = "= other Archive\n\n|===\n|Post ID |Title\n\n|===\n";
        std::fs::write(output.join("index.adoc"), mismatched_index).unwrap();
        let options = journal_cli(&server, dir.path(), false).options();

        let error = run_archive("news", &options, &Progress::hidden())
            .await
            .unwrap_err();

        assert!(matches!(error, LjError::InvalidArchive { .. }), "{error:?}");
        assert!(server.received_requests().await.unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(output.join("index.adoc")).unwrap(),
            mismatched_index
        );
        assert!(!output.join("posts").exists());
        assert!(!output.join("book.adoc").exists());
    }

    #[test]
    fn builds_users_livejournal_post_url_for_underscore_journal() {
        let base_url = Url::parse("https://livejournal.com").unwrap();
        let url = journal_post_url("example_user", 166511, &base_url).unwrap();
        assert_eq!(
            url.as_str(),
            "https://users.livejournal.com/example_user/166511.html"
        );
    }

    #[tokio::test]
    async fn archives_single_post_media_relative_to_post_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/166511.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "166511",
                r#"<p>Post image <img src="pic.jpg" alt="Picture"></p>"#,
                "",
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/pic.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let cli = test_cli(&server, dir.path(), "166511", false, false);

        run(cli).await.unwrap();

        let post = std::fs::read_to_string(dir.path().join("out/posts/166511.adoc")).unwrap();
        assert!(post.contains("../assets/"));
        assert!(
            !post.contains("../assets/166511/"),
            "asset ref must have no per-post segment: {post}"
        );
        assert!(
            !dir.path().join("out/assets/166511").exists(),
            "assets are flat, no per-post subdir"
        );
        assert!(asset_dir_entries(dir.path().join("out/assets")).len() == 1);
    }

    #[tokio::test]
    async fn skips_comment_media_rewriting_when_comments_are_disabled() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/166512.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "166512",
                "<p>Post body.</p>",
                r#"
                <div id="comments">
                  <div class="comment" id="t1">
                    <span class="comment-author">reader</span>
                    <div class="comment-content">
                      <p>Comment image <img src="comment.jpg" alt="Comment picture"></p>
                    </div>
                  </div>
                </div>
                "#,
            )))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let cli = test_cli(&server, dir.path(), "166512", true, false);

        run(cli).await.unwrap();

        let post = std::fs::read_to_string(dir.path().join("out/posts/166512.adoc")).unwrap();
        assert!(!post.contains("== Comments"));
        assert!(!post.contains("reader"));
        assert!(!dir.path().join("out/assets/166512").exists());

        let requests = server.received_requests().await.unwrap();
        let paths: Vec<_> = requests.iter().map(|request| request.url.path()).collect();
        assert_eq!(paths, vec!["/news/166512.html"]);
    }

    #[tokio::test]
    async fn rewrites_in_journal_links_to_xref_on_bulk_export() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a href="/news/166511.html">a</a><a href="/news/166512.html">b</a>"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/166511.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "166511",
                r#"<p>Link to <a href="https://news.livejournal.com/166512.html">the other</a>.</p>"#,
                "",
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/166512.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "166512",
                "<p>Other body.</p>",
                "",
            )))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let cli = journal_cli(&server, dir.path(), false);
        run(cli).await.unwrap();

        let post = std::fs::read_to_string(dir.path().join("out/posts/166511.adoc")).unwrap();
        assert!(post.contains("xref:166512.adoc[the other]"), "got: {post}");
        assert!(
            !post.contains("news.livejournal.com/166512.html"),
            "got: {post}"
        );
    }

    #[tokio::test]
    async fn failed_bulk_link_target_stays_external() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"<a href="/news/1.html">One</a><a href="/news/2.html">Two</a>"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                r#"<p><a href="https://news.livejournal.com/2.html">Post two</a></p>"#,
                "",
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/2.html"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let options = journal_cli(&server, dir.path(), false).options();

        let outcome = run_archive("news", &options, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(outcome.archived, 1);
        assert_eq!(outcome.failed, 1);
        let post_path = dir.path().join("out/posts/1.adoc");
        assert!(post_path.exists());
        assert!(!dir.path().join("out/posts/2.adoc").exists());
        let post = std::fs::read_to_string(&post_path).unwrap();
        assert!(
            post.contains("https://news.livejournal.com/2.html[Post two]"),
            "got: {post}"
        );
        assert!(!post.contains("xref:2.adoc"), "got: {post}");
        assert!(!post.contains("lj-archiver-link"), "got: {post:?}");
        let metadata = read_matching_link_metadata(&post_path);
        assert_eq!(
            metadata_expressions(&post, &metadata),
            ["https://news.livejournal.com/2.html"]
        );

        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert!(index.contains("xref:posts/1.adoc"), "got: {index}");
        assert!(!index.contains("xref:posts/2.adoc"), "got: {index}");
        let pending_files: Vec<_> = std::fs::read_dir(dir.path().join("out/posts"))
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                (path.extension().and_then(|extension| extension.to_str()) == Some("pending"))
                    .then_some(path)
            })
            .collect();
        assert!(pending_files.is_empty(), "got: {pending_files:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn successful_post_is_public_with_external_links_before_delayed_peer_finishes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"<a href="/news/1.html">One</a><a href="/news/2.html">Two</a>"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                r#"<p><a href="https://news.livejournal.com/2.html">Post two</a></p>"#,
                "",
            )))
            .mount(&server)
            .await;
        let peer_gate =
            GatedResponse::closed(ResponseTemplate::new(200).set_body_string(post_html(
                "2",
                "<p>Delayed post two.</p>",
                "",
            )));
        Mock::given(method("GET"))
            .and(path("/news/2.html"))
            .respond_with(peer_gate.clone())
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let options = journal_cli(&server, dir.path(), false).options();
        let post_one_path = dir.path().join("out/posts/1.adoc");
        let (tx, mut rx) = futures::channel::mpsc::channel(128);
        let progress = Progress::channel(tx);
        let _release_peer_on_drop = peer_gate.release_on_drop();
        let mut archive_task =
            tokio::spawn(async move { run_archive("news", &options, &progress).await });
        let mut events = Vec::new();

        let early_done = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(event) = rx.next().await {
                let post_one_done = matches!(event, ProgressEvent::PostDone { id: 1, .. });
                events.push(event);
                if post_one_done {
                    return Ok(());
                }
            }
            Err("progress channel closed before post 1 completed")
        })
        .await;
        let early_done = match early_done {
            Ok(result) => result.map_err(str::to_owned),
            Err(error) => Err(format!("deadlock guard elapsed: {error}")),
        };
        if let Err(error) = early_done {
            peer_gate.release();
            archive_task.abort();
            let _ = archive_task.await;
            panic!("post 1 was not finalized before delayed post 2: {error}");
        }

        assert!(!peer_gate.is_released(), "peer gate released prematurely");
        assert!(
            !archive_task.is_finished(),
            "archive unexpectedly finished before delayed post 2"
        );
        assert!(post_one_path.exists(), "post 1 was not publicly written");
        let external = std::fs::read_to_string(&post_one_path).unwrap();
        assert!(
            external.contains("https://news.livejournal.com/2.html[Post two]"),
            "got: {external}"
        );
        assert!(!external.contains("xref:2.adoc"), "got: {external}");
        assert!(!external.contains("\u{1f}"), "got: {external:?}");
        assert!(!external.contains("lj-archiver-link"), "got: {external:?}");
        let external_metadata = read_matching_link_metadata(&post_one_path);
        assert_eq!(
            metadata_expressions(&external, &external_metadata),
            ["https://news.livejournal.com/2.html"]
        );
        let external_digest = external_metadata["post_sha256"].clone();

        peer_gate.release();
        let outcome =
            match tokio::time::timeout(std::time::Duration::from_secs(10), &mut archive_task).await
            {
                Ok(joined) => joined.unwrap().unwrap(),
                Err(error) => {
                    archive_task.abort();
                    let _ = archive_task.await;
                    panic!("archive did not finish after releasing delayed post 2: {error}");
                }
            };
        events.extend(rx.collect::<Vec<_>>().await);

        assert_eq!(outcome.archived, 2);
        assert_eq!(outcome.failed, 0);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProgressEvent::PostDone { id: 1, .. }))
                .count(),
            1
        );
        let local = std::fs::read_to_string(&post_one_path).unwrap();
        assert!(local.contains("xref:2.adoc[Post two]"), "got: {local}");
        let local_metadata = read_matching_link_metadata(&post_one_path);
        assert_eq!(
            metadata_expressions(&local, &local_metadata),
            ["xref:2.adoc"]
        );
        assert_ne!(local_metadata["post_sha256"], external_digest);
        assert!(pending_post_paths(dir.path().join("out/posts")).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn first_stage_failure_is_reported_before_delayed_peer_and_retains_baseline() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"<a href="/news/1.html">One</a><a href="/news/2.html">Two</a>"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                "<p>Replacement that must fail first-stage finalization.</p>",
                "",
            )))
            .mount(&server)
            .await;
        let peer_gate =
            GatedResponse::closed(ResponseTemplate::new(200).set_body_string(post_html(
                "2",
                "<p>Delayed post two.</p>",
                "",
            )));
        Mock::given(method("GET"))
            .and(path("/news/2.html"))
            .respond_with(peer_gate.clone())
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let baseline = "= Baseline One\n\n:lj-id: 1\n\nKeep me.\n";
        seed_existing_archive(dir.path(), &[(1, "Baseline One", baseline)], true);
        let options = journal_cli(&server, dir.path(), false).options();
        let expected_failure_url = format!("{}/news/1.html", server.uri());
        let writer = InjectedFinalPostWriter::failing_on([(1, 1)]);
        let task_writer = writer.clone();
        let (tx, mut rx) = futures::channel::mpsc::channel(128);
        let progress = Progress::channel(tx);
        let _release_peer_on_drop = peer_gate.release_on_drop();
        let mut archive_task = tokio::spawn(async move {
            archive_journal_with_writer(&options, &progress, "news".to_string(), &task_writer).await
        });
        let mut events = Vec::new();

        let early_failure = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(event) = rx.next().await {
                let post_one_failed = matches!(event, ProgressEvent::PostFailed { id: 1 });
                events.push(event);
                if post_one_failed {
                    return Ok(());
                }
            }
            Err("progress channel closed before post 1 failed")
        })
        .await;
        let early_failure = match early_failure {
            Ok(result) => result.map_err(str::to_owned),
            Err(error) => Err(format!("deadlock guard elapsed: {error}")),
        };
        if let Err(error) = early_failure {
            peer_gate.release();
            archive_task.abort();
            let _ = archive_task.await;
            panic!("post 1 failure was not reported before delayed post 2: {error}");
        }

        assert!(!peer_gate.is_released(), "peer gate released prematurely");
        assert!(
            !archive_task.is_finished(),
            "archive unexpectedly finished before delayed post 2"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/posts/1.adoc")).unwrap(),
            baseline
        );

        peer_gate.release();
        let outcome =
            match tokio::time::timeout(std::time::Duration::from_secs(10), &mut archive_task).await
            {
                Ok(joined) => joined.unwrap().unwrap(),
                Err(error) => {
                    archive_task.abort();
                    let _ = archive_task.await;
                    panic!("archive did not finish after releasing delayed post 2: {error}");
                }
            };
        events.extend(rx.collect::<Vec<_>>().await);

        assert_eq!(outcome.archived, 1);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].id, 1);
        assert_eq!(outcome.failures[0].url.as_str(), expected_failure_url);
        assert_eq!(writer.calls_for(1), 1);
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        ProgressEvent::PostDone { id: 1, .. }
                            | ProgressEvent::PostSkipped { id: 1 }
                            | ProgressEvent::PostFailed { id: 1 }
                    )
                })
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProgressEvent::PostFailed { id: 1 }))
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/posts/1.adoc")).unwrap(),
            baseline
        );
        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert_eq!(index.matches("|1 |xref:posts/1.adoc").count(), 1, "{index}");
        assert_eq!(index.matches("|2 |xref:posts/2.adoc").count(), 1, "{index}");
        let book = std::fs::read_to_string(dir.path().join("out/book.adoc")).unwrap();
        assert_eq!(book.matches("include::posts/1.adoc").count(), 1, "{book}");
        assert_eq!(book.matches("include::posts/2.adoc").count(), 1, "{book}");
        assert!(pending_post_paths(dir.path().join("out/posts")).is_empty());
    }

    #[tokio::test]
    async fn first_stage_write_failures_are_sorted_excluded_and_cleaned_up() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a href="/news/1.html">One</a><a href="/news/2.html">Two</a><a href="/news/3.html">Three</a>"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                "<p>Post one.</p>",
                "",
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/2.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "2",
                "<p>Post two.</p>",
                "",
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/3.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "3",
                r#"<p><a href="https://news.livejournal.com/1.html">Post one</a> and <a href="https://news.livejournal.com/2.html">Post two</a></p>"#,
                "",
            )))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let mut options = journal_cli(&server, dir.path(), false).options();
        options.generate_book = true;
        let writer = InjectedFinalPostWriter::failing_on([(1, 1), (2, 1)]);
        let (tx, rx) = futures::channel::mpsc::channel(128);
        let progress = Progress::channel(tx);

        let outcome = archive_journal_with_writer(&options, &progress, "news".to_string(), &writer)
            .await
            .unwrap();
        drop(progress);
        let events: Vec<_> = rx.collect().await;

        assert_eq!(outcome.archived, 1);
        assert_eq!(outcome.failed, 2);
        assert_eq!(
            outcome
                .failures
                .iter()
                .map(|failure| failure.id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(!dir.path().join("out/posts/1.adoc").exists());
        assert!(!dir.path().join("out/posts/2.adoc").exists());
        let post = std::fs::read_to_string(dir.path().join("out/posts/3.adoc")).unwrap();
        assert!(
            post.contains("https://news.livejournal.com/1.html[Post one]"),
            "got: {post}"
        );
        assert!(
            post.contains("https://news.livejournal.com/2.html[Post two]"),
            "got: {post}"
        );
        assert!(!post.contains("xref:1.adoc"), "got: {post}");
        assert!(!post.contains("xref:2.adoc"), "got: {post}");
        assert!(!post.contains("lj-archiver-link"), "got: {post:?}");

        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert!(index.contains("xref:posts/3.adoc"), "got: {index}");
        assert!(!index.contains("xref:posts/1.adoc"), "got: {index}");
        assert!(!index.contains("xref:posts/2.adoc"), "got: {index}");
        let book = std::fs::read_to_string(dir.path().join("out/book.adoc")).unwrap();
        assert!(book.contains("include::posts/3.adoc"), "got: {book}");
        assert!(!book.contains("include::posts/1.adoc"), "got: {book}");
        assert!(!book.contains("include::posts/2.adoc"), "got: {book}");

        let mut failed_progress = events
            .iter()
            .filter_map(|event| match event {
                ProgressEvent::PostFailed { id } => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();
        failed_progress.sort_unstable();
        assert_eq!(failed_progress, vec![1, 2]);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ProgressEvent::PostDone {
                    id: 3,
                    title
                } if title == "Post Title"
            )
        }));
        assert!(pending_post_paths(dir.path().join("out/posts")).is_empty());
    }

    #[tokio::test]
    async fn failed_refetch_retains_the_validated_baseline_entry_and_file() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"<a href="/news/1.html">One</a><a href="/news/2.html">Two</a>"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                "<p>Replacement that must not become public.</p>",
                "",
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/2.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "2",
                r#"<p><a href="https://news.livejournal.com/1.html">Post one</a></p>"#,
                "",
            )))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let baseline = "= Baseline One\n\n:lj-id: 1\n\nKeep me.\n";
        seed_existing_archive(dir.path(), &[(1, "Baseline One", baseline)], true);
        let options = journal_cli(&server, dir.path(), false).options();
        let writer = InjectedFinalPostWriter::failing_on([(1, 1)]);
        let (tx, rx) = futures::channel::mpsc::channel(128);
        let progress = Progress::channel(tx);

        let outcome = archive_journal_with_writer(&options, &progress, "news".to_string(), &writer)
            .await
            .unwrap();
        drop(progress);
        let events: Vec<_> = rx.collect().await;

        assert_eq!(outcome.archived, 1);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].id, 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/posts/1.adoc")).unwrap(),
            baseline
        );
        let post_two = std::fs::read_to_string(dir.path().join("out/posts/2.adoc")).unwrap();
        assert!(post_two.contains("xref:1.adoc[Post one]"), "{post_two}");
        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert_eq!(index.matches("|1 |xref:posts/1.adoc").count(), 1, "{index}");
        assert_eq!(index.matches("|2 |xref:posts/2.adoc").count(), 1, "{index}");
        let book = std::fs::read_to_string(dir.path().join("out/book.adoc")).unwrap();
        assert_eq!(book.matches("include::posts/1.adoc").count(), 1, "{book}");
        assert_eq!(book.matches("include::posts/2.adoc").count(), 1, "{book}");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProgressEvent::PostFailed { id: 1 }))
                .count(),
            1
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ProgressEvent::Summary {
                    archived: 1,
                    skipped: 0,
                    failed: 1,
                    ..
                }
            )
        }));
        assert!(pending_post_paths(dir.path().join("out/posts")).is_empty());
    }

    #[tokio::test]
    async fn journal_same_id_refetch_replaces_only_the_trusted_alias_path() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/news/1.html">One</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                "<p>Journal alias replacement.</p>",
                "",
            )))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        seed_existing_archive_at(
            dir.path(),
            &[(1, "01.adoc", "Existing Alias", "= Existing Alias\n\nOld.\n")],
            true,
        );
        let options = journal_cli(&server, dir.path(), false).options();

        let outcome = run_archive("news", &options, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(outcome.archived, 1);
        assert_eq!(outcome.failed, 0);
        let alias = dir.path().join("out/posts/01.adoc");
        let expected = format!(
            concat!(
                "= Post Title\n\n",
                ":lj-id: 1\n",
                ":lj-url: {}/news/1.html\n",
                ":lj-author: news\n",
                "ifndef::lj-archiver-post-link-prefix",
                "[:lj-archiver-post-link-prefix:]\n\n",
                "Journal alias replacement.\n",
            ),
            server.uri()
        );
        assert_eq!(std::fs::read_to_string(alias).unwrap(), expected);
        assert!(!dir.path().join("out/posts/1.adoc").exists());
        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert_eq!(
            index.matches("|1 |xref:posts/01.adoc").count(),
            1,
            "{index}"
        );
        assert_eq!(index.matches("xref:posts/1.adoc").count(), 0, "{index}");
        let book = std::fs::read_to_string(dir.path().join("out/book.adoc")).unwrap();
        assert_eq!(book.matches("include::posts/01.adoc").count(), 1, "{book}");
        assert_eq!(book.matches("include::posts/1.adoc").count(), 0, "{book}");
    }

    #[tokio::test]
    async fn failed_final_write_retains_alias_and_manifests_only_its_actual_path() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/news/1.html">One</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                "<p>Replacement must fail.</p>",
                "",
            )))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let baseline = "= Existing Alias\n\nxref:2.adoc[Post two]\n";
        seed_existing_archive_at(
            dir.path(),
            &[(1, "01.adoc", "Existing Alias", baseline)],
            true,
        );
        let options = journal_cli(&server, dir.path(), false).options();
        let writer = InjectedFinalPostWriter::failing_on([(1, 1)]);

        let outcome =
            archive_journal_with_writer(&options, &Progress::hidden(), "news".to_string(), &writer)
                .await
                .unwrap();

        assert_eq!(outcome.archived, 0);
        assert_eq!(outcome.failed, 1);
        assert_eq!(writer.calls_for(1), 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/posts/01.adoc")).unwrap(),
            baseline
        );
        assert!(!dir.path().join("out/posts/1.adoc").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap(),
            concat!(
                "= news Archive\n\n",
                "|===\n|Post ID |Title\n\n",
                "|1 |xref:posts/01.adoc[Existing Alias]\n",
                "|===\n",
            )
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/book.adoc")).unwrap(),
            concat!(
                "= news Archive\n",
                ":doctype: book\n",
                ":toc:\n",
                ":imagesdir: posts\n",
                ":lj-archiver-post-link-prefix: posts/\n\n",
                "include::index.adoc[leveloffset=+1]\n",
                "\ninclude::posts/01.adoc[leveloffset=+1]\n",
            )
        );
    }

    #[tokio::test]
    async fn failed_refetch_retains_only_alias_and_keeps_actual_manifest_targets() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/news/1.html">One</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let baseline = "= Existing Alias\n\nPublic baseline bytes.\n";
        seed_existing_archive_at(
            dir.path(),
            &[(1, "01.adoc", "Existing Alias", baseline)],
            true,
        );
        let options = journal_cli(&server, dir.path(), false).options();

        let outcome = run_archive("news", &options, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(outcome.archived, 0);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.failed, 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/posts/01.adoc")).unwrap(),
            baseline
        );
        assert!(!dir.path().join("out/posts/1.adoc").exists());
        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert_eq!(
            index.matches("|1 |xref:posts/01.adoc").count(),
            1,
            "{index}"
        );
        assert_eq!(index.matches("xref:posts/1.adoc").count(), 0, "{index}");
        let book = std::fs::read_to_string(dir.path().join("out/book.adoc")).unwrap();
        assert_eq!(book.matches("include::posts/01.adoc").count(), 1, "{book}");
        assert_eq!(book.matches("include::posts/1.adoc").count(), 0, "{book}");
    }

    #[tokio::test]
    async fn upgrade_write_failure_retains_external_fallback_and_cleans_up() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"<a href="/news/1.html">One</a><a href="/news/2.html">Two</a>"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                r#"<p><a href="https://news.livejournal.com/2.html">Post two</a></p>"#,
                "",
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/2.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "2",
                "<p>Post two.</p>",
                "",
            )))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let options = journal_cli(&server, dir.path(), false).options();
        let writer = InjectedFinalPostWriter::failing_on([(1, 2)]);

        let outcome =
            archive_journal_with_writer(&options, &Progress::hidden(), "news".to_string(), &writer)
                .await
                .unwrap();

        assert_eq!(outcome.archived, 2);
        assert_eq!(outcome.failed, 0);
        assert!(outcome.failures.is_empty());
        let post_path = dir.path().join("out/posts/1.adoc");
        let post = std::fs::read_to_string(&post_path).unwrap();
        assert!(
            post.contains("https://news.livejournal.com/2.html[Post two]"),
            "got: {post}"
        );
        assert!(!post.contains("xref:2.adoc"), "got: {post}");
        assert!(!post.contains("lj-archiver-link"), "got: {post:?}");
        let metadata = read_matching_link_metadata(&post_path);
        assert_eq!(
            metadata_expressions(&post, &metadata),
            ["https://news.livejournal.com/2.html"]
        );
        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert!(index.contains("xref:posts/1.adoc"), "got: {index}");
        assert!(index.contains("xref:posts/2.adoc"), "got: {index}");
        assert!(pending_post_paths(dir.path().join("out/posts")).is_empty());
    }

    #[tokio::test]
    async fn metadata_warning_keeps_successful_journal_publication_counts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/news/1.html">One</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/1.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(post_html(
                "1",
                "<p>Published despite metadata warning.</p>",
                "",
            )))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let options = journal_cli(&server, dir.path(), false).options();
        let writer = InjectedFinalPostWriter::warning_on([(1, 1)]);

        let outcome =
            archive_journal_with_writer(&options, &Progress::hidden(), "news".to_string(), &writer)
                .await
                .unwrap();

        assert_eq!(outcome.archived, 1);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.failed, 0);
        assert!(outcome.failures.is_empty());
        assert_eq!(writer.calls_for(1), 2);
        read_matching_link_metadata(&dir.path().join("out/posts/1.adoc"));
    }

    #[tokio::test]
    async fn resume_valid_metadata_externalizes_link_to_failed_post() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"<a href="/news/1.html">One</a><a href="/news/2.html">Two</a>"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/2.html"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let baseline = "= Existing\n\nxref:2.adoc[Post two]\n";
        seed_existing_archive(dir.path(), &[(1, "Existing", baseline)], false);
        let existing = dir.path().join("out/posts/1.adoc");
        write_link_metadata_fixture(
            &existing,
            baseline,
            vec![stored_link(baseline, "xref:2.adoc", 2, "xref")],
        );
        let options = journal_cli(&server, dir.path(), true).options();

        let outcome = run_archive("news", &options, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(outcome.archived, 0);
        assert_eq!(outcome.skipped, 1);
        assert_eq!(outcome.failed, 1);
        let repaired = std::fs::read_to_string(&existing).unwrap();
        assert!(
            repaired.contains("https://news.livejournal.com/2.html[Post two]"),
            "got: {repaired}"
        );
        assert!(!repaired.contains("xref:2.adoc"), "got: {repaired}");
        let metadata = read_matching_link_metadata(&existing);
        assert_eq!(
            metadata_expressions(&repaired, &metadata),
            ["https://news.livejournal.com/2.html"]
        );
        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert!(
            index.contains("xref:posts/1.adoc[Existing]"),
            "got: {index}"
        );
        assert!(!index.contains("xref:posts/2.adoc"), "got: {index}");
    }

    #[tokio::test]
    async fn resume_valid_metadata_retargets_exact_aliases_and_refreshes_shifted_ranges() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a href="/news/1.html">One</a><a href="/news/3.html">Three</a>"#,
            ))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let baseline = concat!(
            "= Existing Alias\n\n",
            "xref:3.adoc[Peer]\n",
            "image:../assets/p.jpg[,link={lj-archiver-post-link-prefix}3.adoc]\n",
            "xref:1.adoc[Self]\n",
        );
        seed_existing_archive_at(
            dir.path(),
            &[
                (1, "01.adoc", "Existing Alias", baseline),
                (3, "03.adoc", "Peer Alias", "= Peer Alias\n\nBody.\n"),
            ],
            false,
        );
        let post_path = dir.path().join("out/posts/01.adoc");
        write_link_metadata_fixture(
            &post_path,
            baseline,
            vec![
                stored_link(baseline, "xref:3.adoc", 3, "xref"),
                stored_link(
                    baseline,
                    "{lj-archiver-post-link-prefix}3.adoc",
                    3,
                    "image_link",
                ),
                stored_link(baseline, "xref:1.adoc", 1, "xref"),
            ],
        );
        let options = journal_cli(&server, dir.path(), true).options();

        let outcome = run_archive("news", &options, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(outcome.archived, 0);
        assert_eq!(outcome.skipped, 2);
        assert_eq!(outcome.failed, 0);
        let expected = concat!(
            "= Existing Alias\n\n",
            "xref:03.adoc[Peer]\n",
            "image:../assets/p.jpg[,link={lj-archiver-post-link-prefix}03.adoc]\n",
            "xref:01.adoc[Self]\n",
        );
        assert_eq!(std::fs::read_to_string(&post_path).unwrap(), expected);
        let metadata = read_matching_link_metadata(&post_path);
        assert_eq!(
            metadata_expressions(expected, &metadata),
            [
                "xref:03.adoc",
                "{lj-archiver-post-link-prefix}03.adoc",
                "xref:01.adoc"
            ]
        );
        assert_eq!(
            metadata["links"][1]["start"],
            expected
                .find("{lj-archiver-post-link-prefix}03.adoc")
                .unwrap()
        );
        assert_eq!(
            metadata["links"][2]["start"],
            expected.find("xref:01.adoc").unwrap()
        );
    }

    #[tokio::test]
    async fn resume_without_metadata_preserves_alias_post_bytes_and_manifests() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"<a href="/news/1.html">One</a><a href="/news/2.html">Two</a>"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/news/2.html"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let dir = tempdir().unwrap();
        let baseline = concat!(
            "= Existing Alias\n\n",
            "xref:1.adoc[Self] xref:2.adoc[Failed peer]\n",
            "image:../assets/p.jpg[,link={lj-archiver-post-link-prefix}1.adoc]\n",
            "image:../assets/q.jpg[,link={lj-archiver-post-link-prefix}2.adoc]\n",
            "`xref:2.adoc[Inline code evidence]`\n",
            "....\n",
            "xref:2.adoc[Listing evidence]\n",
            "....\n",
            "++++\n",
            "<unsupported>xref:2.adoc[Passthrough evidence]</unsupported>\n",
            "++++\n",
        );
        seed_existing_archive_at(
            dir.path(),
            &[(1, "01.adoc", "Existing Alias", baseline)],
            true,
        );
        let options = journal_cli(&server, dir.path(), true).options();

        let outcome = run_archive("news", &options, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(outcome.archived, 0);
        assert_eq!(outcome.skipped, 1);
        assert_eq!(outcome.failed, 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/posts/01.adoc")).unwrap(),
            baseline
        );
        assert!(!dir.path().join("out/posts/1.adoc").exists());
        let inspected = Archive::new(dir.path().join("out"), "news")
            .inspect_existing()
            .unwrap()
            .unwrap();
        assert_eq!(inspected.posts.len(), 1);
        assert_eq!(
            inspected.posts[0].path,
            dir.path().join("out/posts/01.adoc")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap(),
            concat!(
                "= news Archive\n\n",
                "|===\n|Post ID |Title\n\n",
                "|1 |xref:posts/01.adoc[Existing Alias]\n",
                "|===\n",
            )
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/book.adoc")).unwrap(),
            concat!(
                "= news Archive\n",
                ":doctype: book\n",
                ":toc:\n",
                ":imagesdir: posts\n",
                ":lj-archiver-post-link-prefix: posts/\n\n",
                "include::index.adoc[leveloffset=+1]\n",
                "\ninclude::posts/01.adoc[leveloffset=+1]\n",
            )
        );
    }

    #[tokio::test]
    async fn resume_invalid_metadata_variants_preserve_exact_post_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/news/calendar"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2024/12/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<a href="/news/1.html">One</a>"#),
            )
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        for name in ["malformed", "stale", "invalid-range", "invalid-syntax"] {
            let case_dir = dir.path().join(name);
            std::fs::create_dir(&case_dir).unwrap();
            let contents = match name {
                "stale" => "= Existing\n\nxref:1.adoc[Self]\nDeliberate user edit.\n",
                "invalid-syntax" => "= Existing\n\nxref:99.adoc[Wrong target syntax]\n",
                _ => "= Existing\n\nxref:1.adoc[Self]\n",
            };
            seed_existing_archive(&case_dir, &[(1, "Existing", contents)], false);
            let post_path = case_dir.join("out/posts/1.adoc");

            match name {
                "malformed" => {
                    let metadata_path = link_metadata_path(&post_path);
                    std::fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
                    std::fs::write(metadata_path, b"{not-json\n").unwrap();
                }
                "stale" => write_link_metadata_fixture(
                    &post_path,
                    "= Existing\n\nxref:1.adoc[Self]\n",
                    vec![stored_link(contents, "xref:1.adoc", 1, "xref")],
                ),
                "invalid-range" => write_link_metadata_fixture(
                    &post_path,
                    contents,
                    vec![serde_json::json!({
                        "start": 0,
                        "end": contents.len() + 1,
                        "target_id": 1,
                        "context": "xref",
                    })],
                ),
                "invalid-syntax" => write_link_metadata_fixture(
                    &post_path,
                    contents,
                    vec![stored_link(contents, "xref:99.adoc", 1, "xref")],
                ),
                _ => unreachable!(),
            }
            let options = journal_cli(&server, &case_dir, true).options();

            let outcome = run_archive("news", &options, &Progress::hidden())
                .await
                .unwrap();

            assert_eq!(outcome.archived, 0, "case {name}");
            assert_eq!(outcome.skipped, 1, "case {name}");
            assert_eq!(outcome.failed, 0, "case {name}");
            assert_eq!(
                std::fs::read_to_string(post_path).unwrap(),
                contents,
                "case {name}"
            );
        }
    }

    fn test_cli(
        server: &MockServer,
        temp_dir: &std::path::Path,
        post_id: &str,
        no_comments: bool,
        no_media: bool,
    ) -> Cli {
        Cli {
            target: format!("{}/news/{post_id}.html", server.uri()),
            output_dir: Some(temp_dir.join("out")),
            no_comments,
            no_media,
            concurrency: 2,
            resume: false,
            book: false,
            journal: None,
            verbose: false,
            quiet: false,
            user_agent: None,
            base_url: Url::parse(&server.uri()).unwrap(),
        }
    }

    fn journal_cli(server: &MockServer, temp_dir: &std::path::Path, resume: bool) -> Cli {
        Cli {
            target: "news".to_string(),
            output_dir: Some(temp_dir.join("out")),
            no_comments: false,
            no_media: true,
            concurrency: 2,
            resume,
            book: false,
            journal: None,
            verbose: false,
            quiet: false,
            user_agent: None,
            base_url: Url::parse(&server.uri()).unwrap(),
        }
    }

    fn post_html(id: &str, body_html: &str, comments_html: &str) -> String {
        format!(
            r#"<!doctype html>
<html>
  <head><title>Post Title</title></head>
  <body>
    <article class="lj-post" data-journal="news" data-itemid="{id}">
      <h1 class="entry-title">Post Title</h1>
      <a class="entry-author" href="https://news.livejournal.com/">news</a>
      <div class="entry-content">{body_html}</div>
    </article>
    {comments_html}
  </body>
</html>"#
        )
    }

    fn parsed_post(url: &str, journal: &str, id: u64) -> Post {
        Post {
            journal: journal.to_string(),
            id,
            url: Url::parse(url).unwrap(),
            title: "Title".to_string(),
            author: journal.to_string(),
            poster: None,
            published_at: None,
            tags: Vec::new(),
            body_html: "<p>Body.</p>".to_string(),
            comments: Vec::new(),
        }
    }

    fn post_containing_collision(namespace: &str) -> Post {
        let mut post = parsed_post("https://news.livejournal.com/1.html", "news", 1);
        post.body_html = format!(
            concat!(
                "<p>authored \u{1f}lj-archiver-link:{}:7:0\u{1f} ",
                r#"<a href="https://news.livejournal.com/7.html">Seven</a></p>"#,
            ),
            namespace
        );
        post
    }

    fn post_containing_shared_boundary_prefix(namespace: &str) -> Post {
        let mut post = parsed_post("https://news.livejournal.com/1.html", "news", 1);
        post.body_html = format!(
            concat!(
                "<p>\u{1f}lj-archiver-link:{}:7:0",
                r#"<a href="https://news.livejournal.com/7.html">Seven</a></p>"#,
            ),
            namespace
        );
        post
    }

    fn seed_existing_archive(
        temp_dir: &std::path::Path,
        posts: &[(u64, &str, &str)],
        generate_book: bool,
    ) -> Archive {
        let archive = Archive::new(temp_dir.join("out"), "news");
        let archived = posts
            .iter()
            .map(|&(id, title, contents)| ArchivedPost {
                id,
                title: title.to_string(),
                path: archive.write_post(id, contents).unwrap(),
            })
            .collect::<Vec<_>>();
        archive.write_index(&archived).unwrap();
        if generate_book {
            archive.write_book(&archived).unwrap();
        }
        archive
    }

    fn seed_existing_archive_at(
        temp_dir: &std::path::Path,
        posts: &[(u64, &str, &str, &str)],
        generate_book: bool,
    ) -> Archive {
        let archive = Archive::new(temp_dir.join("out"), "news");
        let archived = posts
            .iter()
            .map(|&(id, filename, title, contents)| {
                let path = temp_dir.join("out/posts").join(filename);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, contents).unwrap();
                ArchivedPost {
                    id,
                    title: title.to_string(),
                    path,
                }
            })
            .collect::<Vec<_>>();
        archive.write_index(&archived).unwrap();
        if generate_book {
            archive.write_book(&archived).unwrap();
        }
        archive
    }

    fn link_metadata_path(post_path: &Path) -> PathBuf {
        let root = post_path.parent().unwrap().parent().unwrap();
        let filename = post_path.file_name().unwrap().to_str().unwrap();
        root.join(".lj-archiver/links")
            .join(format!("{filename}.json"))
    }

    fn stored_link(
        contents: &str,
        expression: &str,
        target_id: u64,
        context: &str,
    ) -> serde_json::Value {
        let start = contents.find(expression).unwrap();
        serde_json::json!({
            "start": start,
            "end": start + expression.len(),
            "target_id": target_id,
            "context": context,
        })
    }

    fn write_link_metadata_fixture(
        post_path: &Path,
        digest_contents: &str,
        links: Vec<serde_json::Value>,
    ) {
        let path = link_metadata_path(post_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut encoded = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "post_sha256": hex::encode(Sha256::digest(digest_contents.as_bytes())),
            "links": links,
        }))
        .unwrap();
        encoded.push(b'\n');
        std::fs::write(path, encoded).unwrap();
    }

    fn read_matching_link_metadata(post_path: &Path) -> serde_json::Value {
        let contents = std::fs::read_to_string(post_path).unwrap();
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(link_metadata_path(post_path)).unwrap()).unwrap();
        assert_eq!(metadata["version"], 1);
        assert_eq!(
            metadata["post_sha256"],
            hex::encode(Sha256::digest(contents.as_bytes()))
        );
        metadata
    }

    fn metadata_expressions<'a>(
        contents: &'a str,
        metadata: &'a serde_json::Value,
    ) -> Vec<&'a str> {
        metadata["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|link| {
                let start = link["start"].as_u64().unwrap() as usize;
                let end = link["end"].as_u64().unwrap() as usize;
                &contents[start..end]
            })
            .collect()
    }

    fn asset_dir_entries(path: impl AsRef<std::path::Path>) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }

    #[derive(Clone)]
    struct GatedResponse {
        response: ResponseTemplate,
        state: Arc<(Mutex<bool>, Condvar)>,
    }

    impl GatedResponse {
        fn closed(response: ResponseTemplate) -> Self {
            Self {
                response,
                state: Arc::new((Mutex::new(false), Condvar::new())),
            }
        }

        fn is_released(&self) -> bool {
            *self
                .state
                .0
                .lock()
                .expect("gated response mutex poisoned while checking release state")
        }

        fn release(&self) {
            let (released, wake) = &*self.state;
            let mut released = released
                .lock()
                .expect("gated response mutex poisoned while releasing response");
            *released = true;
            wake.notify_all();
        }

        fn release_on_drop(&self) -> GateReleaseGuard {
            GateReleaseGuard(self.clone())
        }
    }

    impl Respond for GatedResponse {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            let (released, wake) = &*self.state;
            let mut released = released
                .lock()
                .expect("gated response mutex poisoned while waiting for release");
            while !*released {
                released = wake
                    .wait(released)
                    .expect("gated response mutex poisoned while blocked on release");
            }
            self.response.clone()
        }
    }

    struct GateReleaseGuard(GatedResponse);

    impl Drop for GateReleaseGuard {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    #[derive(Clone)]
    struct InjectedFinalPostWriter {
        failing_calls: HashSet<(u64, usize)>,
        metadata_warning_calls: HashSet<(u64, usize)>,
        calls: Arc<Mutex<HashMap<u64, usize>>>,
    }

    impl InjectedFinalPostWriter {
        fn failing_on(calls: impl IntoIterator<Item = (u64, usize)>) -> Self {
            Self::with_injections(calls, [])
        }

        fn warning_on(calls: impl IntoIterator<Item = (u64, usize)>) -> Self {
            Self::with_injections([], calls)
        }

        fn with_injections(
            failing_calls: impl IntoIterator<Item = (u64, usize)>,
            metadata_warning_calls: impl IntoIterator<Item = (u64, usize)>,
        ) -> Self {
            Self {
                failing_calls: failing_calls.into_iter().collect(),
                metadata_warning_calls: metadata_warning_calls.into_iter().collect(),
                calls: Arc::new(Mutex::new(HashMap::new())),
            }
        }

        fn calls_for(&self, id: u64) -> usize {
            self.calls.lock().unwrap().get(&id).copied().unwrap_or(0)
        }
    }

    impl FinalPostWriter for InjectedFinalPostWriter {
        fn publish_post(
            &self,
            archive: &Archive,
            id: u64,
            destination: &Path,
            resolved: &ResolvedLinks,
        ) -> Result<PostPublication> {
            let call = {
                let mut calls = self.calls.lock().unwrap();
                let call = calls.entry(id).or_default();
                *call += 1;
                *call
            };
            if self.failing_calls.contains(&(id, call)) {
                return Err(LjError::Write {
                    path: destination.to_path_buf(),
                    source: std::io::Error::other(format!(
                        "injected final post write failure for {id}, call {call}"
                    )),
                });
            }

            let mut publication = archive.publish_post(destination, resolved)?;
            if self.metadata_warning_calls.contains(&(id, call)) {
                publication.metadata_warning = Some(LjError::Write {
                    path: link_metadata_path(destination),
                    source: std::io::Error::other(format!(
                        "injected metadata warning for {id}, call {call}"
                    )),
                });
            }
            Ok(publication)
        }
    }

    fn pending_post_paths(path: impl AsRef<std::path::Path>) -> Vec<PathBuf> {
        std::fs::read_dir(path)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                (path.extension().and_then(|extension| extension.to_str()) == Some("pending"))
                    .then_some(path)
            })
            .collect()
    }
}
