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
