use lj_archiver::{
    archive::Archive,
    models::{ArchivedPost, ArchivedTitle},
};
use std::{fs, path::Path, process::Command};
use tempfile::tempdir;

const DIAGNOSTIC_MAX_CHARS: usize = 2_000;

fn bounded_diagnostic(input: &[u8]) -> String {
    let text = String::from_utf8_lossy(input);
    let mut chars = text.chars();
    let excerpt: String = chars.by_ref().take(DIAGNOSTIC_MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{excerpt}\n...[truncated]")
    } else {
        excerpt
    }
}

fn first_image_src(html: &str) -> Option<&str> {
    html.match_indices("<img").find_map(|(start, _)| {
        let (tag, _) = html[start..].split_once('>')?;
        tag.split_ascii_whitespace()
            .find_map(|attribute| attribute.strip_prefix("src=\""))
            .and_then(|value| value.strip_suffix('"'))
    })
}

#[test]
#[ignore = "requires asciidoctor; exercised explicitly by CI"]
fn book_media_resolves_to_archive_root_asset() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("archive");
    let archive = Archive::new(&root, "news");
    fs::create_dir_all(archive.assets_dir()).unwrap();
    fs::write(
        archive.assets_dir().join("sample.svg"),
        r#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#,
    )
    .unwrap();
    let post_path = archive
        .write_post(1, "= Post\n\nimage::../assets/sample.svg[]\n")
        .unwrap();
    let posts = [ArchivedPost {
        id: 1,
        title: ArchivedTitle::authored("Post"),
        path: post_path,
    }];
    archive.write_index(&posts).unwrap();
    archive.write_book(&posts).unwrap();

    let output = Command::new("asciidoctor")
        .arg("book.adoc")
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "asciidoctor failed:\nstdout excerpt: {}\nstderr excerpt: {}",
        bounded_diagnostic(&output.stdout),
        bounded_diagnostic(&output.stderr)
    );

    let html = fs::read_to_string(root.join("book.html")).unwrap();
    let emitted_src = first_image_src(&html).unwrap_or_else(|| {
        panic!(
            "generated HTML contained no <img src>; HTML excerpt:\n{}",
            bounded_diagnostic(html.as_bytes())
        )
    });
    assert!(
        emitted_src == "assets/sample.svg",
        "generated HTML emitted an unexpected <img src>: expected {:?}, actual excerpt: {}",
        "assets/sample.svg",
        bounded_diagnostic(emitted_src.as_bytes())
    );
    let emitted_asset_path = root.join(emitted_src);
    assert!(
        emitted_asset_path.is_file(),
        "generated HTML references a missing archive asset: src excerpt: {}, resolved path: {}",
        bounded_diagnostic(emitted_src.as_bytes()),
        emitted_asset_path.display()
    );
}

#[test]
#[ignore = "requires asciidoctor; exercised explicitly by CI"]
fn linked_post_image_href_resolves_in_standalone_and_book_contexts() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("archive");
    let archive = Archive::new(&root, "news");
    let source = concat!(
        "= First\n\n",
        "ifndef::lj-archiver-post-link-prefix",
        "[:lj-archiver-post-link-prefix:]\n\n",
        "image:https://cdn.example.test/photo.jpg[photo,",
        "link={lj-archiver-post-link-prefix}2.adoc]\n",
    );
    let first_path = archive.write_post(1, source).unwrap();
    let second_path = archive.write_post(2, "= Second\n").unwrap();
    let posts = [
        ArchivedPost {
            id: 1,
            title: ArchivedTitle::authored("First"),
            path: first_path,
        },
        ArchivedPost {
            id: 2,
            title: ArchivedTitle::authored("Second"),
            path: second_path,
        },
    ];
    archive.write_index(&posts).unwrap();
    archive.write_book(&posts).unwrap();

    assert_eq!(
        fs::read_to_string(root.join("posts/1.adoc")).unwrap(),
        source
    );

    for input in ["posts/1.adoc", "book.adoc"] {
        let output = Command::new("asciidoctor")
            .arg(input)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "asciidoctor failed for {input}:\nstdout excerpt: {}\nstderr excerpt: {}",
            bounded_diagnostic(&output.stdout),
            bounded_diagnostic(&output.stderr)
        );
    }

    let standalone = fs::read_to_string(root.join("posts/1.html")).unwrap();
    assert!(
        standalone.contains("href=\"2.adoc\""),
        "standalone post used the wrong linked-image href:\n{}",
        bounded_diagnostic(standalone.as_bytes())
    );
    assert!(!standalone.contains("href=\"xref:2.adoc\""));

    let book = fs::read_to_string(root.join("book.html")).unwrap();
    assert!(
        book.contains("href=\"posts/2.adoc\""),
        "included post used the wrong linked-image href:\n{}",
        bounded_diagnostic(book.as_bytes())
    );
    assert!(!book.contains("href=\"2.adoc\""));
    assert!(!book.contains("href=\"xref:2.adoc\""));
    assert!(root.join("posts/2.adoc").is_file());
}

#[test]
#[ignore = "requires asciidoctor; exercised explicitly by CI"]
fn raw_media_resolves_in_both_standalone_and_book_contexts() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("archive");
    let archive = Archive::new(&root, "news");
    fs::create_dir_all(archive.assets_dir()).unwrap();
    fs::write(
        archive.assets_dir().join("sample.svg"),
        r#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#,
    )
    .unwrap();

    // Byte-for-byte the renderer's own output for a downloaded asset inside a
    // reviewed table; see `adoc::tests::raw_passthrough_localizes_downloaded_media_per_context`.
    let body = concat!(
        "= Post\n\n",
        "ifndef::lj-archiver-book[]\n",
        "++++\n",
        r#"<table><tr><td><img src="../assets/sample.svg" alt="a"></td></tr></table>"#,
        "\n++++\nendif::[]\n",
        "ifdef::lj-archiver-book[]\n",
        "++++\n",
        r#"<table><tr><td><img src="assets/sample.svg" alt="a"></td></tr></table>"#,
        "\n++++\nendif::[]\n",
    );
    let post_path = archive.write_post(1, body).unwrap();
    let posts = [ArchivedPost {
        id: 1,
        title: ArchivedTitle::authored("Post"),
        path: post_path,
    }];
    archive.write_index(&posts).unwrap();
    archive.write_book(&posts).unwrap();

    // Standalone: converted next to posts/, so ../assets/ reaches the asset.
    convert(&root, "posts/1.adoc");
    let standalone_html = fs::read_to_string(root.join("posts/1.html")).unwrap();
    assert_eq!(
        first_image_src(&standalone_html),
        Some("../assets/sample.svg"),
        "standalone conversion emitted the wrong raw <img src>:\n{}",
        bounded_diagnostic(standalone_html.as_bytes())
    );
    assert_eq!(
        standalone_html.matches("<img").count(),
        1,
        "exactly one conditional variant may survive:\n{}",
        bounded_diagnostic(standalone_html.as_bytes())
    );
    assert!(root.join("posts/../assets/sample.svg").is_file());

    // Book: converted at the archive root, so the asset is a direct child.
    convert(&root, "book.adoc");
    let book_html = fs::read_to_string(root.join("book.html")).unwrap();
    assert_eq!(
        first_image_src(&book_html),
        Some("assets/sample.svg"),
        "book conversion emitted the wrong raw <img src>:\n{}",
        bounded_diagnostic(book_html.as_bytes())
    );
    assert_eq!(
        book_html.matches("<img").count(),
        1,
        "exactly one conditional variant may survive:\n{}",
        bounded_diagnostic(book_html.as_bytes())
    );
    assert!(root.join("assets/sample.svg").is_file());
}

/// A directive line inside a conditional raw block must stay inert in both
/// reading contexts, and the escape must not disturb variant selection. The
/// include target is created inside the archive root, so a regression splices
/// recognizable content into the reader's converted archive.
#[test]
#[ignore = "requires asciidoctor; exercised explicitly by CI"]
fn raw_passthrough_include_directive_reads_no_file_in_either_context() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("archive");
    let archive = Archive::new(&root, "news");

    // Byte-for-byte the renderer's own output for a reviewed table carrying an
    // authored `include::` line beside a context-dependent link; see
    // `adoc::tests::conditional_raw_passthrough_escapes_include_lines_in_both_variants`.
    let body = concat!(
        "= Post\n\n",
        "ifndef::lj-archiver-book[]\n",
        "++++\n",
        "<table><tr><td>\n\\include::secret.txt[]\n",
        r#"<a href="2.html">post</a>"#,
        "\n</td></tr></table>\n",
        "++++\nendif::[]\n",
        "ifdef::lj-archiver-book[]\n",
        "++++\n",
        "<table><tr><td>\n\\include::secret.txt[]\n",
        r#"<a href="posts/2.html">post</a>"#,
        "\n</td></tr></table>\n",
        "++++\nendif::[]\n",
    );
    let post_path = archive.write_post(1, body).unwrap();
    let posts = [ArchivedPost {
        id: 1,
        title: ArchivedTitle::authored("Post"),
        path: post_path,
    }];
    archive.write_index(&posts).unwrap();
    archive.write_book(&posts).unwrap();
    for directory in [&root, &root.join("posts")] {
        fs::write(directory.join("secret.txt"), "LEAKED-FILE-CONTENTS\n").unwrap();
    }

    for (input, output, expected_href) in [
        ("posts/1.adoc", "posts/1.html", "2.html"),
        ("book.adoc", "book.html", "posts/2.html"),
    ] {
        convert(&root, input);
        let html = fs::read_to_string(root.join(output)).unwrap();

        assert!(
            !html.contains("LEAKED-FILE-CONTENTS"),
            "the preprocessor read a local file converting {input}:\n{}",
            bounded_diagnostic(html.as_bytes())
        );
        assert_eq!(
            html.matches("include::secret.txt[]").count(),
            1,
            "exactly one variant may survive, with the directive inert:\n{}",
            bounded_diagnostic(html.as_bytes())
        );
        assert!(
            !html.contains("\\include::"),
            "the protective backslash was visible converting {input}:\n{}",
            bounded_diagnostic(html.as_bytes())
        );
        assert!(
            html.contains(&format!(r#"href="{expected_href}""#)),
            "{input} selected the wrong variant:\n{}",
            bounded_diagnostic(html.as_bytes())
        );
    }
}

/// Converts `input` inside `root`, asserting Asciidoctor said nothing at all. A
/// preprocessor complaint about a generated conditional is only ever reported as
/// an `ERROR` line — the process still exits zero — so an empty stderr, not the
/// exit status, is the assertion that matters.
fn convert(root: &Path, input: &str) {
    let output = Command::new("asciidoctor")
        .arg(input)
        .current_dir(root)
        .output()
        .unwrap();
    let diagnostics = bounded_diagnostic(&output.stderr);
    assert!(
        output.status.success(),
        "asciidoctor failed for {input}:\nstdout excerpt: {}\nstderr excerpt: {diagnostics}",
        bounded_diagnostic(&output.stdout),
    );
    assert!(
        diagnostics.is_empty(),
        "asciidoctor diagnosed {input}: {diagnostics}"
    );
}

#[test]
#[ignore = "requires asciidoctor; exercised explicitly by CI"]
fn plain_text_cannot_create_asciidoc_macros_or_expand_attributes() {
    let dir = tempdir().unwrap();
    let adoc = format!(
        ":experimental:\n:secret: EXPANDED\n\n{}",
        lj_archiver::adoc::html_to_adoc(concat!(
            "<p>image:evil.png[] xref:1.adoc[x] {secret} ",
            "footnote:[note] kbd:[Ctrl+C] btn:[OK]</p>"
        ))
    );
    fs::write(dir.path().join("plain-text.adoc"), adoc).unwrap();

    let output = Command::new("asciidoctor")
        .arg("--embedded")
        .arg("plain-text.adoc")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "asciidoctor failed:\nstdout excerpt: {}\nstderr excerpt: {}",
        bounded_diagnostic(&output.stdout),
        bounded_diagnostic(&output.stderr)
    );

    let html = fs::read_to_string(dir.path().join("plain-text.html")).unwrap();
    assert!(
        html.contains("image:evil.png[]"),
        "literal image macro was not visible; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        html.contains("xref:1.adoc[x]"),
        "literal xref macro was not visible; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        html.contains("{secret}"),
        "literal attribute reference was not visible; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        html.contains("footnote:[note]"),
        "literal footnote macro was not visible; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        html.contains("kbd:[Ctrl+C]"),
        "literal kbd macro was not visible; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        html.contains("btn:[OK]"),
        "literal btn macro was not visible; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        !html.contains(r"\image:evil.png[]"),
        "protective image backslash was visible; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        !html.contains(r"\xref:1.adoc[x]"),
        "protective xref backslash was visible; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        !html.contains(r"\{secret}"),
        "protective attribute-reference backslash was visible; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        !html.contains(r"\footnote:[note]"),
        "protective footnote backslash was visible; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        !html.contains(r"\kbd:[Ctrl+C]"),
        "protective kbd backslash was visible; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        !html.contains(r"\btn:[OK]"),
        "protective btn backslash was visible; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        !html.match_indices("<img").any(|(start, _)| html[start..]
            .split_once('>')
            .is_some_and(|(tag, _)| tag.contains("evil.png"))),
        "plain text created an evil.png image; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        !html
            .match_indices("<a")
            .any(
                |(start, _)| html[start..].split_once('>').is_some_and(|(tag, _)| {
                    tag.contains("href=\"1.adoc") || tag.contains("href=\"1.html")
                })
            ),
        "plain text created a link to 1.adoc; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        !html.contains(">x</a>"),
        "plain text created an xref label; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        !html.contains("EXPANDED"),
        "plain text expanded an AsciiDoc attribute; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        !html.contains("class=\"footnote\""),
        "plain text created active footnote markup; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        !html.contains("<kbd"),
        "plain text created kbd markup; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
    assert!(
        !html.contains("class=\"button\""),
        "plain text created button markup; HTML excerpt:\n{}",
        bounded_diagnostic(html.as_bytes())
    );
}
