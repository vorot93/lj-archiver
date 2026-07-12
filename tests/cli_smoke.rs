use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn cli_archives_single_post() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/news/166511.html"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/post_basic.html")),
        )
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("lj-archiver").unwrap();
    cmd.arg("--base-url")
        .arg(server.uri())
        .arg("--output-dir")
        .arg(dir.path().join("out"))
        .arg("--no-media")
        .arg(format!("{}/news/166511.html", server.uri()))
        .assert()
        .success();

    let post = std::fs::read_to_string(dir.path().join("out/posts/166511.adoc")).unwrap();
    assert!(post.contains("= Post Title"));
}

#[tokio::test]
async fn cli_archives_poster_and_date_from_site_entry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/news/166511.html"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(include_str!("fixtures/post_with_metadata.html")),
        )
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("lj-archiver").unwrap();
    cmd.arg("--base-url")
        .arg(server.uri())
        .arg("--output-dir")
        .arg(dir.path().join("out"))
        .arg("--no-media")
        .arg(format!("{}/news/166511.html", server.uri()))
        .assert()
        .success();

    let post = std::fs::read_to_string(dir.path().join("out/posts/166511.adoc")).unwrap();
    assert!(post.contains(":lj-poster: livejournal"), "got: {post}");
    assert!(
        post.contains(":lj-date: 2025-12-17T16:01:00Z"),
        "got: {post}"
    );
}

#[tokio::test]
async fn cli_journal_archive_exits_nonzero_when_a_month_page_fails() {
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
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<a href="/news/166511.html">Good post</a>"#),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/2025/01/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/news/166511.html"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/post_basic.html")),
        )
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("lj-archiver").unwrap();
    cmd.arg("--base-url")
        .arg(server.uri())
        .arg("--output-dir")
        .arg(dir.path().join("out"))
        .arg("--no-media")
        .arg("news")
        .assert()
        .failure();

    // The successfully-discovered post is still archived even though the
    // command as a whole must report failure via its exit status.
    let post = std::fs::read_to_string(dir.path().join("out/posts/166511.adoc")).unwrap();
    assert!(post.contains("= Post Title"));
}

#[test]
fn cli_reports_invalid_bare_id() {
    let mut cmd = Command::cargo_bin("lj-archiver").unwrap();
    cmd.arg("@166511")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--journal"));
}

#[tokio::test]
async fn cli_archives_correct_output_for_images_mentions_and_author() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/news/166511.html"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(include_str!("fixtures/post_output_correctness.html")),
        )
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("lj-archiver").unwrap();
    cmd.arg("--base-url")
        .arg(server.uri())
        .arg("--output-dir")
        .arg(dir.path().join("out"))
        .arg("--no-media")
        .arg(format!("{}/news/166511.html", server.uri()))
        .assert()
        .success();

    let post = std::fs::read_to_string(dir.path().join("out/posts/166511.adoc")).unwrap();

    // Fix 3: author is the canonical Site.entry.journal, not the request path "news".
    assert!(post.contains(":lj-author: canonical_lj"), "author: {post}");
    // Fix 1b: user mention is the clean linked username; userhead glyph dropped.
    assert!(
        post.contains("https://canonical-lj.livejournal.com/[*canonical_lj*]"),
        "user mention: {post}"
    );
    assert!(
        !post.contains("userinfo_v8.svg"),
        "userhead not dropped: {post}"
    );
    // Fix 1: linked image is an inline image macro with link=, never a block
    // macro embedded in link brackets.
    assert!(
        post.contains("[pic,link=https://ic.pics.livejournal.com/canonical_lj/1/original.jpg]"),
        "linked image: {post}"
    );
    assert!(
        !post.contains("[image::"),
        "block image macro in brackets: {post}"
    );
    // Fix 2: no dangling trailing hard break before the stripped widget.
    assert!(post.contains("Trailing text."), "body text: {post}");
    assert!(!post.contains("Trailing text. +"), "dangling +: {post}");
    assert!(!post.contains("+\n+"), "double hard break: {post}");
}

#[tokio::test]
async fn cli_journal_shows_progress_on_stderr() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/news/calendar"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"<a href="/2024/12/">Dec</a>"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/2024/12/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<a href="/news/166511.html">a</a><a href="/news/166512.html">b</a>"#,
        ))
        .mount(&server)
        .await;
    // Distinct fixtures whose data-itemid matches each URL (the parser resolves
    // the id from data-itemid first), so the two posts do not collide.
    Mock::given(method("GET"))
        .and(path("/news/166511.html"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/post_basic.html")),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/news/166512.html"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(include_str!("fixtures/post_with_comments.html")),
        )
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    Command::cargo_bin("lj-archiver")
        .unwrap()
        .arg("--base-url")
        .arg(server.uri())
        .arg("--output-dir")
        .arg(dir.path().join("out"))
        .arg("--no-media")
        .arg("news")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("discovered 2 posts"))
        .stderr(predicate::str::contains("archived 166511"))
        .stderr(predicate::str::contains("done: 2 archived"));
}

#[tokio::test]
async fn cli_quiet_suppresses_all_progress() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/news/166511.html"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/post_basic.html")),
        )
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    Command::cargo_bin("lj-archiver")
        .unwrap()
        .arg("--base-url")
        .arg(server.uri())
        .arg("--output-dir")
        .arg(dir.path().join("out"))
        .arg("--no-media")
        .arg("--quiet")
        .arg(format!("{}/news/166511.html", server.uri()))
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
}

#[tokio::test]
async fn cli_book_flag_writes_book_adoc() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/news/166511.html"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/post_basic.html")),
        )
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    Command::cargo_bin("lj-archiver")
        .unwrap()
        .arg("--base-url")
        .arg(server.uri())
        .arg("--output-dir")
        .arg(dir.path().join("out"))
        .arg("--no-media")
        .arg("--book")
        .arg(format!("{}/news/166511.html", server.uri()))
        .assert()
        .success();

    let book = std::fs::read_to_string(dir.path().join("out/book.adoc")).unwrap();
    assert_eq!(
        book,
        "= news Archive\n:doctype: book\n:toc:\n\ninclude::index.adoc[leveloffset=+1]\n\ninclude::posts/166511.adoc[leveloffset=+1]\n"
    );
}

#[tokio::test]
async fn cli_without_book_flag_writes_no_book_adoc() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/news/166511.html"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/post_basic.html")),
        )
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    Command::cargo_bin("lj-archiver")
        .unwrap()
        .arg("--base-url")
        .arg(server.uri())
        .arg("--output-dir")
        .arg(dir.path().join("out"))
        .arg("--no-media")
        .arg(format!("{}/news/166511.html", server.uri()))
        .assert()
        .success();

    assert!(!dir.path().join("out/book.adoc").exists());
}
