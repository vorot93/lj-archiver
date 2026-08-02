use lj_archiver::{archive::Archive, models::ArchivedPost};
use std::{fs, process::Command};
use tempfile::tempdir;

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
        "asciidoctor failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let html = fs::read_to_string(root.join("book.html")).unwrap();
    assert!(
        html.contains(r#"src="assets/sample.svg""#),
        "generated HTML did not use the archive-root asset: {html}"
    );
    assert!(root.join("assets/sample.svg").is_file());
}
