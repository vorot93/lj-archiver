use lj_archiver::{archive::Archive, models::ArchivedPost};
use std::{fs, process::Command};
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
        title: "Post".to_string(),
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
            title: "First".to_string(),
            path: first_path,
        },
        ArchivedPost {
            id: 2,
            title: "Second".to_string(),
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
