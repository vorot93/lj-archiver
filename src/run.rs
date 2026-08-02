use crate::{
    Result,
    adoc::render_post,
    archive::{Archive, ExistingArchive},
    cli::Cli,
    client::LjClient,
    discover::discover_post_ids,
    error::LjError,
    links::{LinkRewriter, PendingLink, resolve_pending_links, restore_missing_xrefs},
    media::rewrite_inline_media,
    models::{ArchiveTarget, ArchivedPost, Post, PostFailure, RunOptions},
    parser::parse_post_page,
    progress::Progress,
    target::{journals_alias, parse_target, post_ref_from_url, post_url},
};
use futures::{StreamExt, stream};
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::Arc,
};
use url::Url;

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
    let mut candidate_ids = posts.keys().copied().collect::<HashSet<_>>();
    candidate_ids.insert(id);

    progress.single_started(id);
    let client = LjClient::new(options.user_agent.as_deref())?;
    let post = fetch_and_write_post(
        &client,
        options,
        &archive,
        &journal,
        id,
        &url,
        candidate_ids,
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
    fn write_post(&self, archive: &Archive, id: u64, contents: &str) -> Result<PathBuf>;
}

struct AtomicFinalPostWriter;

impl FinalPostWriter for AtomicFinalPostWriter {
    fn write_post(&self, archive: &Archive, id: u64, contents: &str) -> Result<PathBuf> {
        archive.write_post(id, contents)
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
    let client = LjClient::new(options.user_agent.as_deref())?;

    progress.discovery_started();
    let (ids, failed_pages) =
        discover_post_ids(&client, &journal, &options.base_url, progress).await?;
    progress.archiving_started(ids.len());

    let mut candidate_ids = posts.keys().copied().collect::<HashSet<_>>();
    candidate_ids.extend(ids.iter().copied());
    let journal_context = JournalPostContext {
        journal: journal.clone(),
        id_set: candidate_ids,
        trusted_posts,
    };

    let outcomes: Vec<_> = stream::iter(ids)
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
        .buffer_unordered(options.concurrency)
        .collect()
        .await;

    let mut prepared = Vec::new();
    let mut skipped_posts = Vec::new();
    let mut failures = Vec::new();
    let mut skipped = 0usize;
    for outcome in outcomes {
        match outcome {
            JournalPostOutcome::Prepared(post) => prepared.push(post),
            JournalPostOutcome::Skipped(post) => {
                skipped += 1;
                skipped_posts.push(post);
            }
            JournalPostOutcome::Failed(failure) => failures.push(failure),
        }
    }

    let mut finalized = Vec::new();
    let mut archived = 0usize;
    let no_local_ids = HashSet::new();
    for mut post in prepared {
        let first_stage = archive
            .read_pending_post(&post.pending_path)
            .and_then(|pending| {
                let external = resolve_pending_links(&pending, &post.links, &no_local_ids);
                writer.write_post(&archive, post.archived.id, &external)
            });

        match first_stage {
            Ok(path) => {
                post.archived.path = path;
                progress.post_done(post.archived.id, &post.archived.title);
                posts.insert(post.archived.id, post.archived.clone());
                archived += 1;
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

    let final_ids = posts.keys().copied().collect::<HashSet<_>>();

    for post in &finalized {
        match archive.read_pending_post(&post.pending_path) {
            Ok(pending) => {
                let local = resolve_pending_links(&pending, &post.links, &final_ids);
                if let Err(error) = writer.write_post(&archive, post.archived.id, &local) {
                    tracing::warn!(
                        post_id = post.archived.id,
                        error = %error,
                        "could not upgrade finalized post links; retaining external links"
                    );
                }
            }
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

    for post in &skipped_posts {
        match std::fs::read_to_string(&post.path) {
            Ok(existing) => match restore_missing_xrefs(&existing, &journal, &final_ids) {
                Ok(repaired) if repaired != existing => {
                    if let Err(error) = archive.write_post(post.id, &repaired) {
                        tracing::warn!(
                            post_id = post.id,
                            error = %error,
                            "could not repair skipped post links; retaining existing post"
                        );
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        post_id = post.id,
                        error = %error,
                        "could not resolve skipped post links; retaining existing post"
                    );
                }
            },
            Err(error) => {
                tracing::warn!(
                    post_id = post.id,
                    error = %error,
                    "could not read skipped post for link repair; retaining existing post"
                );
            }
        }
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
                let rewriter = LinkRewriter::pending(
                    journal_context.journal,
                    journal_context.id_set,
                    post.url.clone(),
                );
                let rendered = render_post(&post, options.include_comments, Some(&rewriter));
                let pending_path = archive.write_pending_post(id, &rendered)?;

                Result::Ok(PreparedPost {
                    archived: ArchivedPost {
                        id,
                        title: post.title,
                        path: archive.post_path(id),
                    },
                    delivered_url: delivered_url.clone(),
                    pending_path,
                    links: rewriter.pending_links(),
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

fn posts_by_id(existing: Option<&ExistingArchive>) -> BTreeMap<u64, ArchivedPost> {
    existing
        .into_iter()
        .flat_map(|existing| existing.posts.iter().cloned())
        .map(|post| (post.id, post))
        .collect()
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
    candidate_ids: HashSet<u64>,
) -> Result<ArchivedPost> {
    let mut post =
        fetch_requested_post(client, requested_journal, requested_id, requested_url).await?;
    rewrite_post_media(&mut post, options, archive).await?;
    let rewriter = LinkRewriter::new(requested_journal, candidate_ids, post.url.clone());
    let rendered = render_post(&post, options.include_comments, Some(&rewriter));
    let path = archive.write_post(requested_id, &rendered)?;

    Ok(ArchivedPost {
        id: requested_id,
        title: post.title.clone(),
        path,
    })
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
    id_set: HashSet<u64>,
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
    use std::{
        collections::{HashMap, HashSet},
        sync::Mutex,
    };
    use tempfile::tempdir;
    use url::Url;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    #[test]
    fn failure_reason_reports_failed_archive_pages() {
        let reason = failure_reason(2, &[]);
        assert!(
            reason.contains("2 archive page(s) failed to fetch"),
            "unexpected reason: {reason}"
        );
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
            id_set: HashSet::from([1]),
            trusted_posts: Arc::new(BTreeMap::new()),
        };

        archive
            .write_post(1, "= Late Unverified File\n\nMust not trigger a skip.\n")
            .unwrap();
        let options = journal_cli(&server, dir.path(), true).options();
        let (tx, rx) = futures::channel::mpsc::unbounded();
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
                    r#"<img src="picture.jpg" alt="picture"></p>"#,
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
        let (tx, rx) = futures::channel::mpsc::unbounded();
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
        let post = std::fs::read_to_string(post_path).unwrap();
        assert!(
            post.contains("https://news.livejournal.com/2.html[Post two]"),
            "got: {post}"
        );
        assert!(!post.contains("xref:2.adoc"), "got: {post}");
        assert!(!post.contains("lj-archiver-link"), "got: {post:?}");

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
        let (tx, rx) = futures::channel::mpsc::unbounded();
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
        let (tx, rx) = futures::channel::mpsc::unbounded();
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
        let post = std::fs::read_to_string(dir.path().join("out/posts/1.adoc")).unwrap();
        assert!(
            post.contains("https://news.livejournal.com/2.html[Post two]"),
            "got: {post}"
        );
        assert!(!post.contains("xref:2.adoc"), "got: {post}");
        assert!(!post.contains("lj-archiver-link"), "got: {post:?}");
        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert!(index.contains("xref:posts/1.adoc"), "got: {index}");
        assert!(index.contains("xref:posts/2.adoc"), "got: {index}");
        assert!(pending_post_paths(dir.path().join("out/posts")).is_empty());
    }

    #[tokio::test]
    async fn resume_repairs_xrefs_to_missing_failed_posts() {
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
        seed_existing_archive(
            dir.path(),
            &[(1, "Existing", "= Existing\n\nxref:2.adoc[Post two]\n")],
            false,
        );
        let existing = dir.path().join("out/posts/1.adoc");
        let options = journal_cli(&server, dir.path(), true).options();

        let outcome = run_archive("news", &options, &Progress::hidden())
            .await
            .unwrap();

        assert_eq!(outcome.archived, 0);
        assert_eq!(outcome.skipped, 1);
        assert_eq!(outcome.failed, 1);
        let repaired = std::fs::read_to_string(existing).unwrap();
        assert_eq!(
            repaired,
            "= Existing\n\nhttps://news.livejournal.com/2.html[Post two]\n"
        );
        let index = std::fs::read_to_string(dir.path().join("out/index.adoc")).unwrap();
        assert!(
            index.contains("xref:posts/1.adoc[Existing]"),
            "got: {index}"
        );
        assert!(!index.contains("xref:posts/2.adoc"), "got: {index}");
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

    fn asset_dir_entries(path: impl AsRef<std::path::Path>) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }

    struct InjectedFinalPostWriter {
        failing_calls: HashSet<(u64, usize)>,
        calls: Mutex<HashMap<u64, usize>>,
    }

    impl InjectedFinalPostWriter {
        fn failing_on(calls: impl IntoIterator<Item = (u64, usize)>) -> Self {
            Self {
                failing_calls: calls.into_iter().collect(),
                calls: Mutex::new(HashMap::new()),
            }
        }
    }

    impl FinalPostWriter for InjectedFinalPostWriter {
        fn write_post(&self, archive: &Archive, id: u64, contents: &str) -> Result<PathBuf> {
            let call = {
                let mut calls = self.calls.lock().unwrap();
                let call = calls.entry(id).or_default();
                *call += 1;
                *call
            };
            if self.failing_calls.contains(&(id, call)) {
                return Err(LjError::Write {
                    path: archive.post_path(id),
                    source: std::io::Error::other(format!(
                        "injected final post write failure for {id}, call {call}"
                    )),
                });
            }

            archive.write_post(id, contents)
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
