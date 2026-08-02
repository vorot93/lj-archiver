use crate::{
    adoc,
    error::{LjError, Result},
    models::ArchivedPost,
    storage,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct Archive {
    root: PathBuf,
    journal: String,
}

impl Archive {
    pub fn new(root: impl Into<PathBuf>, journal: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            journal: journal.into(),
        }
    }

    pub fn post_path(&self, id: u64) -> PathBuf {
        self.root.join("posts").join(format!("{id}.adoc"))
    }

    pub fn assets_dir(&self) -> PathBuf {
        self.root.join("assets")
    }

    pub fn post_exists(&self, id: u64) -> bool {
        self.post_path(id).is_file()
    }

    pub fn write_post(&self, id: u64, contents: &str) -> Result<PathBuf> {
        let path = self.post_path(id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| LjError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        storage::write_atomic(&path, contents.as_bytes())?;
        Ok(path)
    }

    pub fn write_pending_post(&self, id: u64, contents: &str) -> Result<PathBuf> {
        let path = self.root.join("posts").join(format!("{id}.pending"));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| LjError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        storage::write_atomic(&path, contents.as_bytes())?;
        Ok(path)
    }

    pub fn read_pending_post(&self, path: &Path) -> Result<String> {
        fs::read_to_string(path).map_err(|source| LjError::Read {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn remove_pending_post(&self, path: &Path) -> Result<()> {
        fs::remove_file(path).map_err(|source| LjError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn write_index(&self, posts: &[ArchivedPost]) -> Result<PathBuf> {
        self.ensure_root()?;
        let mut contents = format!("= {} Archive\n\n", self.journal);
        contents.push_str("|===\n|Post ID |Title\n\n");
        for post in posts {
            let path = self.index_path(&post.path);
            let title = adoc::sanitize_xref_label(&post.title);
            contents.push_str(&format!(
                "|{} |xref:{}[{}]\n",
                post.id,
                storage::adoc_path(&path),
                title
            ));
        }
        contents.push_str("|===\n");
        let path = self.root.join("index.adoc");
        storage::write_atomic(&path, contents.as_bytes())?;
        Ok(path)
    }

    pub fn write_book(&self, posts: &[ArchivedPost]) -> Result<PathBuf> {
        self.ensure_root()?;
        let mut contents = format!(
            "= {} Archive\n:doctype: book\n:toc:\n:imagesdir: posts\n\n",
            self.journal
        );
        contents.push_str("include::index.adoc[leveloffset=+1]\n");
        for post in posts {
            let path = self.index_path(&post.path);
            contents.push_str(&format!(
                "\ninclude::{}[leveloffset=+1]\n",
                storage::adoc_path(&path)
            ));
        }
        let path = self.root.join("book.adoc");
        storage::write_atomic(&path, contents.as_bytes())?;
        Ok(path)
    }

    fn ensure_root(&self) -> Result<()> {
        fs::create_dir_all(&self.root).map_err(|source| LjError::Write {
            path: self.root.clone(),
            source,
        })
    }

    fn index_path(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.root).unwrap_or(path).to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ArchivedPost;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn writes_post_file_and_index() {
        let dir = tempdir().unwrap();
        let archive = Archive::new(dir.path().join("archive"), "news");
        archive.write_post(166511, "= Title\n\nBody\n").unwrap();
        archive
            .write_index(&[ArchivedPost {
                id: 166511,
                title: "Title".to_string(),
                path: PathBuf::from("posts/166511.adoc"),
            }])
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("archive/posts/166511.adoc")).unwrap(),
            "= Title\n\nBody\n"
        );
        let index = std::fs::read_to_string(dir.path().join("archive/index.adoc")).unwrap();
        assert!(index.contains("= news Archive"));
        assert!(index.contains("xref:posts/166511.adoc[Title]"));
    }

    #[test]
    fn detects_existing_posts_for_resume() {
        let dir = tempdir().unwrap();
        let archive = Archive::new(dir.path().join("archive"), "news");
        assert!(!archive.post_exists(166511));
        archive.write_post(166511, "= Title\n").unwrap();
        assert!(archive.post_exists(166511));
    }

    #[test]
    fn sanitizes_index_title_labels() {
        let dir = tempdir().unwrap();
        let archive = Archive::new(dir.path().join("archive"), "news");
        archive
            .write_index(&[ArchivedPost {
                id: 166511,
                title: "A | tricky\n[title]".to_string(),
                path: PathBuf::from("posts/166511.adoc"),
            }])
            .unwrap();

        let index = std::fs::read_to_string(dir.path().join("archive/index.adoc")).unwrap();
        let row = index
            .lines()
            .find(|line| line.starts_with("|166511 "))
            .unwrap();
        assert_eq!(row, "|166511 |xref:posts/166511.adoc[A tricky title]");
    }

    #[test]
    fn neutralizes_active_asciidoc_in_index_titles() {
        let dir = tempdir().unwrap();
        let archive = Archive::new(dir.path().join("archive"), "news");
        archive
            .write_index(&[ArchivedPost {
                id: 1,
                title: "+++<img src=x onerror=alert(1)>+++".to_string(),
                path: PathBuf::from("posts/1.adoc"),
            }])
            .unwrap();

        let index = std::fs::read_to_string(dir.path().join("archive/index.adoc")).unwrap();
        let row = index.lines().find(|line| line.starts_with("|1 ")).unwrap();
        assert_eq!(
            row,
            "|1 |xref:posts/1.adoc[\\+++<img src=x onerror=alert(1)>+++]"
        );
        assert!(!index.contains("[+++<img"));
    }

    #[test]
    fn writes_index_paths_relative_to_archive_root() {
        let dir = tempdir().unwrap();
        let archive = Archive::new(dir.path().join("archive"), "news");
        let path = archive.write_post(166511, "= Title\n").unwrap();
        archive
            .write_index(&[ArchivedPost {
                id: 166511,
                title: "Title".to_string(),
                path,
            }])
            .unwrap();

        let index = std::fs::read_to_string(dir.path().join("archive/index.adoc")).unwrap();
        assert!(index.contains("xref:posts/166511.adoc[Title]"));
        assert!(!index.contains(dir.path().to_str().unwrap()));
    }

    #[test]
    fn writes_book_with_index_and_post_includes() {
        let dir = tempdir().unwrap();
        let archive = Archive::new(dir.path().join("archive"), "news");
        let path = archive
            .write_book(&[
                ArchivedPost {
                    id: 166511,
                    title: "First".to_string(),
                    path: PathBuf::from("posts/166511.adoc"),
                },
                ArchivedPost {
                    id: 166512,
                    title: "Second".to_string(),
                    path: PathBuf::from("posts/166512.adoc"),
                },
            ])
            .unwrap();

        assert_eq!(path, dir.path().join("archive/book.adoc"));
        let book = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            book,
            "= news Archive\n:doctype: book\n:toc:\n:imagesdir: posts\n\ninclude::index.adoc[leveloffset=+1]\n\ninclude::posts/166511.adoc[leveloffset=+1]\n\ninclude::posts/166512.adoc[leveloffset=+1]\n"
        );
    }

    #[test]
    fn writes_book_with_no_posts() {
        let dir = tempdir().unwrap();
        let archive = Archive::new(dir.path().join("archive"), "news");
        let path = archive.write_book(&[]).unwrap();
        let book = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            book,
            "= news Archive\n:doctype: book\n:toc:\n:imagesdir: posts\n\ninclude::index.adoc[leveloffset=+1]\n"
        );
    }

    #[test]
    fn replaces_existing_index_with_exact_second_contents() {
        let dir = tempdir().unwrap();
        let archive = Archive::new(dir.path().join("archive"), "news");
        archive
            .write_index(&[ArchivedPost {
                id: 166511,
                title: "First".to_string(),
                path: PathBuf::from("posts/166511.adoc"),
            }])
            .unwrap();
        let path = archive
            .write_index(&[ArchivedPost {
                id: 166512,
                title: "Second".to_string(),
                path: PathBuf::from(r"posts\166512.adoc"),
            }])
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "= news Archive\n\n|===\n|Post ID |Title\n\n|166512 |xref:posts/166512.adoc[Second]\n|===\n"
        );
    }

    #[test]
    fn replaces_existing_book_with_exact_second_contents() {
        let dir = tempdir().unwrap();
        let archive = Archive::new(dir.path().join("archive"), "news");
        archive
            .write_book(&[ArchivedPost {
                id: 166511,
                title: "First".to_string(),
                path: PathBuf::from("posts/166511.adoc"),
            }])
            .unwrap();
        let path = archive
            .write_book(&[ArchivedPost {
                id: 166512,
                title: "Second".to_string(),
                path: PathBuf::from(r"posts\166512.adoc"),
            }])
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "= news Archive\n:doctype: book\n:toc:\n:imagesdir: posts\n\ninclude::index.adoc[leveloffset=+1]\n\ninclude::posts/166512.adoc[leveloffset=+1]\n"
        );
    }

    #[test]
    fn pending_posts_round_trip_and_cleanup() {
        let dir = tempdir().unwrap();
        let archive = Archive::new(dir.path().join("archive"), "news");

        let path = archive.write_pending_post(1, "first pending body").unwrap();

        assert_eq!(
            path.parent(),
            Some(dir.path().join("archive/posts").as_path())
        );
        assert_ne!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("adoc")
        );
        assert_eq!(
            archive.read_pending_post(&path).unwrap(),
            "first pending body"
        );
        assert!(!archive.post_path(1).exists());

        let replacement = archive
            .write_pending_post(1, "second pending body")
            .unwrap();
        assert_eq!(replacement, path);
        assert_eq!(
            archive.read_pending_post(&path).unwrap(),
            "second pending body"
        );

        archive.remove_pending_post(&path).unwrap();
        assert!(!path.exists());
    }
}
