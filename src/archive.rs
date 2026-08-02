use crate::{
    adoc,
    error::{LjError, Result},
    links::POST_LINK_PREFIX_ATTRIBUTE,
    models::ArchivedPost,
    storage,
    target::{is_valid_journal_name, journals_alias},
};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct Archive {
    root: PathBuf,
    journal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingArchive {
    pub journal: String,
    pub posts: Vec<ArchivedPost>,
    pub has_book: bool,
}

#[derive(Debug)]
struct ExistingEntry {
    path: PathBuf,
    file_type: fs::FileType,
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

    pub fn inspect_existing(&self) -> Result<Option<ExistingArchive>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => inspect_entries(entries, &self.root)?,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(invalid_archive(
                    &self.root,
                    format!("could not read directory: {source}"),
                ));
            }
        };
        if entries.is_empty() {
            return Ok(None);
        }

        let index_path = self.root.join("index.adoc");
        let index = find_entry(&entries, "index.adoc")
            .ok_or_else(|| invalid_archive(&index_path, "missing controlled index"))?;
        if !index.file_type.is_file() {
            return Err(invalid_archive(&index.path, "index is not a regular file"));
        }
        let index_contents = read_existing_file(&index.path)?;
        let journal = index_contents
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("= "))
            .and_then(|line| line.strip_suffix(" Archive"))
            .ok_or_else(|| {
                invalid_archive(&index.path, "first line is not a controlled archive title")
            })?;
        if !is_valid_journal_name(journal) {
            return Err(invalid_archive(
                &index.path,
                format!("archive title contains invalid journal name {journal:?}"),
            ));
        }
        if !journals_alias(journal, &self.journal) {
            return Err(invalid_archive(
                &index.path,
                format!(
                    "archive journal {journal:?} does not match requested journal {:?}",
                    self.journal
                ),
            ));
        }

        let posts = match find_entry(&entries, "posts") {
            None => Vec::new(),
            Some(posts_entry) => inspect_posts(posts_entry)?,
        };
        let has_book = match find_entry(&entries, "book.adoc") {
            None => false,
            Some(book) if book.file_type.is_file() => true,
            Some(book) => {
                return Err(invalid_archive(
                    &book.path,
                    "book.adoc is not a regular file",
                ));
            }
        };

        Ok(Some(ExistingArchive {
            journal: journal.to_string(),
            posts,
            has_book,
        }))
    }

    pub fn write_post(&self, id: u64, contents: &str) -> Result<PathBuf> {
        let path = self.post_path(id);
        self.write_post_at(&path, contents)
    }

    pub(crate) fn write_post_at(&self, path: &Path, contents: &str) -> Result<PathBuf> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| LjError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        storage::write_atomic(path, contents.as_bytes())?;
        Ok(path.to_path_buf())
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
            "= {} Archive\n:doctype: book\n:toc:\n:imagesdir: posts\n:{}: posts/\n\n",
            self.journal, POST_LINK_PREFIX_ATTRIBUTE
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

fn inspect_entries(entries: fs::ReadDir, directory: &Path) -> Result<Vec<ExistingEntry>> {
    entries
        .map(|entry| {
            let entry = entry.map_err(|source| {
                invalid_archive(
                    directory,
                    format!("could not read directory entry: {source}"),
                )
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|source| {
                invalid_archive(&path, format!("could not inspect entry: {source}"))
            })?;
            Ok(ExistingEntry { path, file_type })
        })
        .collect()
}

fn find_entry<'a>(entries: &'a [ExistingEntry], name: &str) -> Option<&'a ExistingEntry> {
    entries
        .iter()
        .find(|entry| entry.path.file_name() == Some(OsStr::new(name)))
}

fn inspect_posts(posts_entry: &ExistingEntry) -> Result<Vec<ArchivedPost>> {
    if !posts_entry.file_type.is_dir() {
        return Err(invalid_archive(
            &posts_entry.path,
            "posts path is not a directory",
        ));
    }
    let entries = fs::read_dir(&posts_entry.path).map_err(|source| {
        invalid_archive(
            &posts_entry.path,
            format!("could not read posts directory: {source}"),
        )
    })?;
    let entries = inspect_entries(entries, &posts_entry.path)?;
    let mut posts_by_id = BTreeMap::new();

    for entry in entries {
        if entry.path.extension() != Some(OsStr::new("adoc")) {
            continue;
        }
        if !entry.file_type.is_file() {
            return Err(invalid_archive(
                &entry.path,
                "public post candidate is not a regular file",
            ));
        }
        let stem = entry
            .path
            .file_stem()
            .and_then(OsStr::to_str)
            .ok_or_else(|| invalid_archive(&entry.path, "post id is not valid UTF-8"))?;
        let id = stem
            .parse::<u64>()
            .map_err(|_| invalid_archive(&entry.path, "post id must be a positive integer"))?;
        if id == 0 {
            return Err(invalid_archive(
                &entry.path,
                "post id must be a positive integer",
            ));
        }
        let contents = read_existing_file(&entry.path)?;
        let title = contents
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("= "))
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .ok_or_else(|| {
                invalid_archive(&entry.path, "first line is not a nonempty level-zero title")
            })?;
        let post = ArchivedPost {
            id,
            title: title.to_string(),
            path: entry.path.clone(),
        };
        if posts_by_id.insert(id, post).is_some() {
            return Err(invalid_archive(
                &entry.path,
                format!("duplicate numeric post id {id}"),
            ));
        }
    }

    Ok(posts_by_id.into_values().collect())
}

fn read_existing_file(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .map_err(|source| invalid_archive(path, format!("could not read regular file: {source}")))
}

fn invalid_archive(path: &Path, reason: impl Into<String>) -> LjError {
    LjError::InvalidArchive {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ArchivedPost;
    use std::{fs, path::PathBuf};
    use tempfile::tempdir;

    fn write_valid_index(root: &Path, journal: &str) {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("index.adoc"), format!("= {journal} Archive\n")).unwrap();
    }

    fn assert_invalid_archive(error: LjError, path: &Path, reason_fragment: &str) {
        match error {
            LjError::InvalidArchive {
                path: actual_path,
                reason,
            } => {
                assert_eq!(actual_path, path);
                assert!(
                    reason.contains(reason_fragment),
                    "expected reason {reason:?} to contain {reason_fragment:?}"
                );
            }
            other => panic!("expected InvalidArchive, got {other:?}"),
        }
    }

    #[test]
    fn inspection_returns_none_without_writing_for_missing_or_empty_root() {
        let dir = tempdir().unwrap();
        let missing_root = dir.path().join("missing");
        let missing = Archive::new(&missing_root, "news");
        assert_eq!(missing.inspect_existing().unwrap(), None);
        assert!(!missing_root.exists());

        let empty_root = dir.path().join("empty");
        fs::create_dir(&empty_root).unwrap();
        let empty = Archive::new(&empty_root, "news");
        assert_eq!(empty.inspect_existing().unwrap(), None);
        assert_eq!(fs::read_dir(&empty_root).unwrap().count(), 0);
    }

    #[test]
    fn inspection_rejects_every_nonempty_root_without_index() {
        let dir = tempdir().unwrap();
        for (name, entry_name, is_directory) in [
            ("unrelated", "notes.txt", false),
            ("lone-book", "book.adoc", false),
            ("assets-only", "assets", true),
            ("posts-only", "posts", true),
        ] {
            let root = dir.path().join(name);
            fs::create_dir(&root).unwrap();
            let entry = root.join(entry_name);
            if is_directory {
                fs::create_dir(&entry).unwrap();
            } else {
                fs::write(&entry, "support data").unwrap();
            }
            if name == "posts-only" {
                fs::write(entry.join("1.adoc"), "= Existing\n").unwrap();
            }

            let error = Archive::new(&root, "news").inspect_existing().unwrap_err();
            assert_invalid_archive(error, &root.join("index.adoc"), "missing");
        }
    }

    #[test]
    fn inspection_accepts_matching_index_with_zero_posts() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        write_valid_index(&root, "news");

        assert_eq!(
            Archive::new(&root, "news").inspect_existing().unwrap(),
            Some(ExistingArchive {
                journal: "news".to_string(),
                posts: Vec::new(),
                has_book: false,
            })
        );
    }

    #[test]
    fn inspection_accepts_case_and_hyphen_underscore_journal_alias() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        write_valid_index(&root, "Evo-Lutio");

        let existing = Archive::new(&root, "evo_lutio")
            .inspect_existing()
            .unwrap()
            .unwrap();
        assert_eq!(existing.journal, "Evo-Lutio");
        assert!(existing.posts.is_empty());
        assert!(!existing.has_book);
    }

    #[test]
    fn inspection_rejects_mismatched_index_journal() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        write_valid_index(&root, "other");

        let error = Archive::new(&root, "news").inspect_existing().unwrap_err();
        assert_invalid_archive(error, &root.join("index.adoc"), "does not match");
    }

    #[test]
    fn inspection_rejects_invalid_index_journal_syntax() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        write_valid_index(&root, "bad.name");

        let error = Archive::new(&root, "bad.name")
            .inspect_existing()
            .unwrap_err();
        assert_invalid_archive(error, &root.join("index.adoc"), "invalid journal");
    }

    #[test]
    fn inspection_rejects_malformed_index_title() {
        let dir = tempdir().unwrap();
        for (name, title) in [
            ("wrong-level", "== news Archive\n"),
            ("wrong-suffix", "= news Archives\n"),
            ("missing-title", ""),
        ] {
            let root = dir.path().join(name);
            fs::create_dir(&root).unwrap();
            fs::write(root.join("index.adoc"), title).unwrap();

            let error = Archive::new(&root, "news").inspect_existing().unwrap_err();
            assert_invalid_archive(error, &root.join("index.adoc"), "title");
        }
    }

    #[test]
    fn inspection_rejects_index_that_is_not_a_regular_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        fs::create_dir_all(root.join("index.adoc")).unwrap();

        let error = Archive::new(&root, "news").inspect_existing().unwrap_err();
        assert_invalid_archive(error, &root.join("index.adoc"), "regular file");
    }

    #[test]
    fn inspection_returns_sorted_numeric_posts_with_exact_paths_and_titles() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        write_valid_index(&root, "news");
        let posts = root.join("posts");
        fs::create_dir(&posts).unwrap();
        fs::write(posts.join("20.adoc"), "= Twentieth title\n\nBody\n").unwrap();
        fs::write(posts.join("3.adoc"), "= Third title\n").unwrap();
        fs::write(posts.join("10.pending"), "= Not public\n").unwrap();
        fs::create_dir(posts.join("11.pending")).unwrap();
        fs::write(posts.join("support.txt"), "ignored").unwrap();

        let existing = Archive::new(&root, "news")
            .inspect_existing()
            .unwrap()
            .unwrap();
        assert_eq!(
            existing.posts,
            vec![
                ArchivedPost {
                    id: 3,
                    title: "Third title".to_string(),
                    path: posts.join("3.adoc"),
                },
                ArchivedPost {
                    id: 20,
                    title: "Twentieth title".to_string(),
                    path: posts.join("20.adoc"),
                },
            ]
        );
    }

    #[test]
    fn inspection_rejects_zero_and_nonnumeric_public_post_ids() {
        let dir = tempdir().unwrap();
        for (name, filename, reason) in [
            ("zero", "0.adoc", "positive"),
            ("nonnumeric", "draft.adoc", "positive"),
        ] {
            let root = dir.path().join(name);
            write_valid_index(&root, "news");
            fs::create_dir(root.join("posts")).unwrap();
            let post = root.join("posts").join(filename);
            fs::write(&post, "= Title\n").unwrap();

            let error = Archive::new(&root, "news").inspect_existing().unwrap_err();
            assert_invalid_archive(error, &post, reason);
        }
    }

    #[test]
    fn inspection_rejects_duplicate_numeric_post_ids() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        write_valid_index(&root, "news");
        fs::create_dir(root.join("posts")).unwrap();
        fs::write(root.join("posts/1.adoc"), "= One\n").unwrap();
        fs::write(root.join("posts/01.adoc"), "= Duplicate\n").unwrap();

        let error = Archive::new(&root, "news").inspect_existing().unwrap_err();
        match error {
            LjError::InvalidArchive { path, reason } => {
                assert!(path == root.join("posts/1.adoc") || path == root.join("posts/01.adoc"));
                assert!(reason.contains("duplicate"));
                assert!(reason.contains('1'));
            }
            other => panic!("expected InvalidArchive, got {other:?}"),
        }
    }

    #[test]
    fn inspection_rejects_public_post_candidate_that_is_not_a_regular_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        write_valid_index(&root, "news");
        fs::create_dir_all(root.join("posts/1.adoc")).unwrap();

        let error = Archive::new(&root, "news").inspect_existing().unwrap_err();
        assert_invalid_archive(error, &root.join("posts/1.adoc"), "regular file");
    }

    #[test]
    fn inspection_rejects_missing_malformed_and_empty_post_titles() {
        let dir = tempdir().unwrap();
        for (name, contents) in [
            ("missing", ""),
            ("malformed", "== Nested heading\n"),
            ("empty", "=    \n\nBody\n"),
        ] {
            let root = dir.path().join(name);
            write_valid_index(&root, "news");
            fs::create_dir(root.join("posts")).unwrap();
            let post = root.join("posts/1.adoc");
            fs::write(&post, contents).unwrap();

            let error = Archive::new(&root, "news").inspect_existing().unwrap_err();
            assert_invalid_archive(error, &post, "title");
        }
    }

    #[test]
    fn inspection_rejects_posts_path_that_is_not_a_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        write_valid_index(&root, "news");
        fs::write(root.join("posts"), "not a directory").unwrap();

        let error = Archive::new(&root, "news").inspect_existing().unwrap_err();
        assert_invalid_archive(error, &root.join("posts"), "directory");
    }

    #[test]
    fn inspection_reports_book_presence_and_rejects_non_file_book() {
        let dir = tempdir().unwrap();
        let valid_root = dir.path().join("valid");
        write_valid_index(&valid_root, "news");
        fs::write(valid_root.join("book.adoc"), "= news Archive\n").unwrap();
        let existing = Archive::new(&valid_root, "news")
            .inspect_existing()
            .unwrap()
            .unwrap();
        assert!(existing.has_book);

        let invalid_root = dir.path().join("invalid");
        write_valid_index(&invalid_root, "news");
        fs::create_dir(invalid_root.join("book.adoc")).unwrap();
        let error = Archive::new(&invalid_root, "news")
            .inspect_existing()
            .unwrap_err();
        assert_invalid_archive(error, &invalid_root.join("book.adoc"), "regular file");
    }

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
    fn keeps_generated_index_xref_active_while_neutralizing_user_title_syntax() {
        let dir = tempdir().unwrap();
        let archive = Archive::new(dir.path().join("archive"), "news");
        archive
            .write_index(&[ArchivedPost {
                id: 1,
                title: "image:evil.png[] xref:1.adoc[x] {secret}".to_string(),
                path: PathBuf::from("posts/1.adoc"),
            }])
            .unwrap();

        let index = std::fs::read_to_string(dir.path().join("archive/index.adoc")).unwrap();
        let row = index.lines().find(|line| line.starts_with("|1 ")).unwrap();
        assert_eq!(
            row,
            "|1 |xref:posts/1.adoc[image:evil.png xref:1.adoc x \\{secret}]"
        );
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
            "= news Archive\n:doctype: book\n:toc:\n:imagesdir: posts\n:lj-archiver-post-link-prefix: posts/\n\ninclude::index.adoc[leveloffset=+1]\n\ninclude::posts/166511.adoc[leveloffset=+1]\n\ninclude::posts/166512.adoc[leveloffset=+1]\n"
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
            "= news Archive\n:doctype: book\n:toc:\n:imagesdir: posts\n:lj-archiver-post-link-prefix: posts/\n\ninclude::index.adoc[leveloffset=+1]\n"
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
            "= news Archive\n:doctype: book\n:toc:\n:imagesdir: posts\n:lj-archiver-post-link-prefix: posts/\n\ninclude::index.adoc[leveloffset=+1]\n\ninclude::posts/166512.adoc[leveloffset=+1]\n"
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
