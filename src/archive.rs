use crate::{
    adoc,
    error::{LjError, Result},
    link_metadata::serialize_link_metadata,
    links::{BOOK_ATTRIBUTE, POST_LINK_PREFIX_ATTRIBUTE, ResolvedLinks},
    models::{ArchivedPost, ArchivedTitle},
    storage,
    target::{is_valid_journal_name, journals_alias},
};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    io::{BufRead, BufReader, Read},
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
pub(crate) struct PostPublication {
    pub path: PathBuf,
    pub metadata_warning: Option<LjError>,
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
        storage::inspect_controlled_directory(&self.root, Path::new("assets"), true)?;
        storage::inspect_controlled_directory(&self.root, Path::new(".lj-archiver"), false)?;
        storage::inspect_controlled_directory(&self.root, Path::new(".lj-archiver/links"), true)?;
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
        let index_title = read_controlled_title_line(&index.path)?;
        let journal = index_title
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
        let (relative, _) = self.controlled_post_paths(path)?;
        storage::write_atomic_controlled(&self.root, &relative, contents.as_bytes())?;
        Ok(path.to_path_buf())
    }

    pub(crate) fn publish_post(
        &self,
        destination: &Path,
        resolved: &ResolvedLinks,
    ) -> Result<PostPublication> {
        self.publish_post_with_metadata_writer(destination, resolved, |relative, contents| {
            storage::write_atomic_controlled(&self.root, relative, contents)
        })
    }

    fn publish_post_with_metadata_writer<F>(
        &self,
        destination: &Path,
        resolved: &ResolvedLinks,
        metadata_writer: F,
    ) -> Result<PostPublication>
    where
        F: FnOnce(&Path, &[u8]) -> Result<()>,
    {
        let (post_relative, metadata_relative) = self.controlled_post_paths(destination)?;
        let metadata =
            serialize_link_metadata(&resolved.contents, &resolved.links).map_err(|error| {
                invalid_archive(
                    destination,
                    format!("could not serialize trusted link metadata: {error}"),
                )
            })?;

        storage::prepare_controlled_file(&self.root, &post_relative)?;
        storage::prepare_controlled_file(&self.root, &metadata_relative)?;
        storage::write_atomic_controlled(&self.root, &post_relative, resolved.contents.as_bytes())?;
        let metadata_warning = metadata_writer(&metadata_relative, &metadata).err();

        Ok(PostPublication {
            path: destination.to_path_buf(),
            metadata_warning,
        })
    }

    pub(crate) fn read_link_metadata(&self, post_path: &Path) -> Result<Option<Vec<u8>>> {
        let (_, metadata_relative) = self.controlled_post_paths(post_path)?;
        if !storage::controlled_file_exists(&self.root, &metadata_relative)? {
            return Ok(None);
        }
        let metadata_path = self.root.join(metadata_relative);
        fs::read(&metadata_path)
            .map(Some)
            .map_err(|source| LjError::Read {
                path: metadata_path,
                source,
            })
    }

    pub(crate) fn write_pending_post(&self, id: u64, contents: &str) -> Result<PathBuf> {
        let relative = self.pending_relative_path(id)?;
        let path = self.root.join(&relative);
        storage::write_atomic_controlled(&self.root, &relative, contents.as_bytes())?;
        Ok(path)
    }

    pub(crate) fn read_pending_post(&self, id: u64) -> Result<String> {
        let relative = self.pending_relative_path(id)?;
        storage::controlled_file_exists(&self.root, &relative)?;
        let path = self.root.join(relative);
        fs::read_to_string(&path).map_err(|source| LjError::Read { path, source })
    }

    pub(crate) fn remove_pending_post(&self, id: u64) -> Result<()> {
        let relative = self.pending_relative_path(id)?;
        storage::controlled_file_exists(&self.root, &relative)?;
        let path = self.root.join(relative);
        fs::remove_file(&path).map_err(|source| LjError::Write { path, source })
    }

    fn pending_relative_path(&self, id: u64) -> Result<PathBuf> {
        if id == 0 {
            return Err(invalid_archive(
                &self.root,
                "pending post id must be positive",
            ));
        }

        Ok(PathBuf::from("posts").join(format!("{id}.pending")))
    }

    pub fn write_index(&self, posts: &[ArchivedPost]) -> Result<PathBuf> {
        self.ensure_root()?;
        let mut contents = format!("= {} Archive\n\n", self.journal);
        contents.push_str("|===\n|Post ID |Title\n\n");
        for post in posts {
            let path = self.index_path(&post.path);
            let title = match &post.title {
                ArchivedTitle::Authored(title) => adoc::sanitize_authored_xref_label(title),
                ArchivedTitle::RenderedHeading(title) => adoc::sanitize_rendered_xref_label(title),
            };
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
            "= {} Archive\n:doctype: book\n:toc:\n:{}:\n:imagesdir: posts\n:{}: posts/\n\n",
            self.journal, BOOK_ATTRIBUTE, POST_LINK_PREFIX_ATTRIBUTE
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

    fn controlled_post_paths(&self, path: &Path) -> Result<(PathBuf, PathBuf)> {
        let posts = self.root.join("posts");
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| invalid_archive(path, "post destination is outside the archive root"))?;
        let filename = path.file_name().and_then(OsStr::to_str);
        let numeric_adoc = path.parent() == Some(posts.as_path())
            && filename.and_then(numeric_post_filename_id).is_some();
        if !numeric_adoc {
            return Err(invalid_archive(
                path,
                "post destination must be a numeric .adoc file under posts",
            ));
        }
        let metadata_relative = PathBuf::from(".lj-archiver/links").join(format!(
            "{}.json",
            filename.expect("numeric filename is valid UTF-8")
        ));
        Ok((relative.to_path_buf(), metadata_relative))
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
        let filename = entry
            .path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| invalid_archive(&entry.path, "post id is not valid UTF-8"))?;
        let id = numeric_post_filename_id(filename)
            .ok_or_else(|| invalid_archive(&entry.path, "post id must be a positive integer"))?;
        let title_line = read_controlled_title_line(&entry.path)?;
        let title = title_line
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
            title: ArchivedTitle::rendered_heading(title),
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

const MAX_CONTROLLED_TITLE_LINE_BYTES: u64 = 65_536;

fn read_controlled_title_line(path: &Path) -> Result<String> {
    let file = fs::File::open(path).map_err(|source| LjError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut bytes = Vec::new();
    BufReader::new(file)
        .take(MAX_CONTROLLED_TITLE_LINE_BYTES + 1)
        .read_until(b'\n', &mut bytes)
        .map_err(|source| LjError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > MAX_CONTROLLED_TITLE_LINE_BYTES as usize || !bytes.ends_with(b"\n") {
        return Err(invalid_archive(
            path,
            "controlled title line exceeds 64 KiB or is unterminated",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| invalid_archive(path, "controlled title line is not valid UTF-8"))
}

fn invalid_archive(path: &Path, reason: impl Into<String>) -> LjError {
    LjError::InvalidArchive {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

pub(crate) fn numeric_post_filename_id(filename: &str) -> Option<u64> {
    let digits = filename.strip_suffix(".adoc")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let id = digits.parse::<u64>().ok()?;
    (id > 0).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        links::{ActiveLink, ActiveLinkContext, ResolvedLinks},
        models::{ArchivedPost, ArchivedTitle},
    };
    use sha2::{Digest, Sha256};
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

    fn resolved_post(contents: &str, expression: &str) -> ResolvedLinks {
        let start = contents.find(expression).unwrap();
        ResolvedLinks {
            contents: contents.to_string(),
            links: vec![ActiveLink {
                start,
                end: start + expression.len(),
                target_id: 2,
                context: ActiveLinkContext::Xref,
            }],
        }
    }

    #[test]
    fn publication_maps_numeric_post_to_hidden_link_metadata_path() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        let archive = Archive::new(&root, "news");
        let destination = archive.post_path(166511);
        let resolved = ResolvedLinks {
            contents: "= Title\n".to_string(),
            links: Vec::new(),
        };

        let publication = archive.publish_post(&destination, &resolved).unwrap();

        assert_eq!(publication.path, root.join("posts/166511.adoc"));
        assert!(root.join(".lj-archiver/links/166511.adoc.json").is_file());
    }

    #[test]
    fn publication_writes_exact_post_and_digest_matching_metadata() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        let archive = Archive::new(&root, "news");
        let destination = archive.post_path(166511);
        let contents = "= Title\n\nxref:2.adoc[Two]\n";
        let resolved = resolved_post(contents, "xref:2.adoc");

        let publication = archive.publish_post(&destination, &resolved).unwrap();

        assert!(publication.metadata_warning.is_none());
        assert_eq!(fs::read_to_string(&destination).unwrap(), contents);
        let metadata: serde_json::Value = serde_json::from_slice(
            &fs::read(root.join(".lj-archiver/links/166511.adoc.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            metadata["post_sha256"],
            hex::encode(Sha256::digest(contents.as_bytes()))
        );
        assert_eq!(
            metadata["links"][0]["start"],
            contents.find("xref:").unwrap()
        );
        assert_eq!(
            metadata["links"][0]["end"],
            contents.find("xref:").unwrap() + "xref:2.adoc".len()
        );
    }

    #[test]
    fn publication_with_no_links_still_writes_sidecar() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        let archive = Archive::new(&root, "news");
        let destination = archive.post_path(9);
        let resolved = ResolvedLinks {
            contents: "= No links\n".to_string(),
            links: Vec::new(),
        };

        archive.publish_post(&destination, &resolved).unwrap();

        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join(".lj-archiver/links/9.adoc.json")).unwrap())
                .unwrap();
        assert_eq!(metadata["links"], serde_json::json!([]));
    }

    #[test]
    fn publication_preflights_unsafe_metadata_parent_before_post_write() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        let archive = Archive::new(&root, "news");
        fs::create_dir_all(root.join(".lj-archiver")).unwrap();
        fs::write(root.join(".lj-archiver/links"), "not a directory").unwrap();
        let destination = archive.post_path(9);
        let resolved = ResolvedLinks {
            contents: "= Must not publish\n".to_string(),
            links: Vec::new(),
        };

        assert!(matches!(
            archive.publish_post(&destination, &resolved),
            Err(LjError::InvalidArchive { .. })
        ));
        assert!(!destination.exists());
    }

    #[test]
    fn publication_metadata_write_failure_keeps_post_and_returns_warning() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        let archive = Archive::new(&root, "news");
        let destination = archive.post_path(9);
        let resolved = ResolvedLinks {
            contents: "= Published post\n".to_string(),
            links: Vec::new(),
        };

        let publication = archive
            .publish_post_with_metadata_writer(&destination, &resolved, |relative, _| {
                Err(LjError::Write {
                    path: root.join(relative),
                    source: std::io::Error::other("injected metadata failure"),
                })
            })
            .unwrap();

        assert_eq!(publication.path, destination);
        assert!(publication.metadata_warning.is_some());
        assert_eq!(
            fs::read_to_string(root.join("posts/9.adoc")).unwrap(),
            "= Published post\n"
        );
        assert!(!root.join(".lj-archiver/links/9.adoc.json").exists());
    }

    #[test]
    fn link_metadata_read_returns_none_for_missing_and_rejects_nonregular_sidecar() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        let archive = Archive::new(&root, "news");
        let post_path = archive.post_path(9);

        assert_eq!(archive.read_link_metadata(&post_path).unwrap(), None);

        fs::create_dir_all(root.join(".lj-archiver/links/9.adoc.json")).unwrap();
        assert!(matches!(
            archive.read_link_metadata(&post_path),
            Err(LjError::InvalidArchive { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn link_metadata_read_rejects_symlinked_sidecar() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        let archive = Archive::new(&root, "news");
        let post_path = archive.post_path(9);
        let outside = dir.path().join("outside.json");
        fs::write(&outside, "{}\n").unwrap();
        fs::create_dir_all(root.join(".lj-archiver/links")).unwrap();
        symlink(&outside, root.join(".lj-archiver/links/9.adoc.json")).unwrap();

        assert!(matches!(
            archive.read_link_metadata(&post_path),
            Err(LjError::InvalidArchive { .. })
        ));
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
    fn inspection_rejects_controlled_directory_components_that_are_plain_files() {
        let dir = tempdir().unwrap();
        for relative in ["assets", ".lj-archiver", ".lj-archiver/links"] {
            let root = dir.path().join(relative.replace('/', "-"));
            write_valid_index(&root, "news");
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "not a directory").unwrap();

            let error = Archive::new(&root, "news").inspect_existing().unwrap_err();
            assert_invalid_archive(error, &path, "directory");
        }
    }

    #[cfg(unix)]
    #[test]
    fn inspection_rejects_symlinked_controlled_directory_components() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        for relative in ["assets", ".lj-archiver", ".lj-archiver/links"] {
            let root = dir.path().join(relative.replace('/', "-"));
            let outside = dir
                .path()
                .join(format!("{}-outside", relative.replace('/', "-")));
            write_valid_index(&root, "news");
            fs::create_dir(&outside).unwrap();
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            symlink(&outside, &path).unwrap();

            let error = Archive::new(&root, "news").inspect_existing().unwrap_err();
            assert_invalid_archive(error, &path, "directory");
        }
    }

    #[test]
    fn inspection_accepts_absent_or_empty_real_controlled_directories() {
        let dir = tempdir().unwrap();
        let absent_root = dir.path().join("absent");
        write_valid_index(&absent_root, "news");
        assert!(
            Archive::new(&absent_root, "news")
                .inspect_existing()
                .is_ok()
        );

        let empty_root = dir.path().join("empty");
        write_valid_index(&empty_root, "news");
        fs::create_dir(empty_root.join("assets")).unwrap();
        fs::create_dir_all(empty_root.join(".lj-archiver/links")).unwrap();
        assert!(Archive::new(&empty_root, "news").inspect_existing().is_ok());
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
                    title: ArchivedTitle::rendered_heading("Third title"),
                    path: posts.join("3.adoc"),
                },
                ArchivedPost {
                    id: 20,
                    title: ArchivedTitle::rendered_heading("Twentieth title"),
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
            ("signed", "+1.adoc", "positive"),
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
    fn inspection_marks_heading_source_as_rendered_and_ignores_invalid_body_utf8() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        write_valid_index(&root, "news");
        fs::create_dir(root.join("posts")).unwrap();
        fs::write(root.join("posts/1.adoc"), b"= \\{secret}\n\xffbody").unwrap();

        let existing = Archive::new(&root, "news")
            .inspect_existing()
            .unwrap()
            .unwrap();
        assert_eq!(
            existing.posts[0].title,
            ArchivedTitle::RenderedHeading(r"\{secret}".to_string())
        );
    }

    #[test]
    fn inspection_rejects_a_title_line_over_sixty_four_kib() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        write_valid_index(&root, "news");
        fs::create_dir(root.join("posts")).unwrap();
        let post = root.join("posts/1.adoc");
        fs::write(&post, format!("= {}\n", "x".repeat(65_536))).unwrap();

        let error = Archive::new(&root, "news").inspect_existing().unwrap_err();
        assert_invalid_archive(error, &post, "64 KiB");
    }

    #[test]
    fn retained_title_provenance_keeps_two_index_generations_identical() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        let archive = Archive::new(&root, "news");
        let authored = [
            (1, "image:evil.png[]", "= \\image:evil.png[]\n\nBody\n"),
            (
                2,
                "A | bracketed [title]",
                "= A | bracketed [title]\n\nBody\n",
            ),
            (3, "One, two", "= One, two\n\nBody\n"),
            (4, "{secret}", "= \\{secret}\n\nBody\n"),
            (5, r"First\", "= First&#92;\n\nBody\n"),
            (6, "Adjacent", "= Adjacent\n\nBody\n"),
        ]
        .map(|(id, title, contents)| ArchivedPost {
            id,
            title: ArchivedTitle::authored(title),
            path: archive.write_post(id, contents).unwrap(),
        });
        let first_posts = authored
            .iter()
            .map(|post| fs::read(&post.path).unwrap())
            .collect::<Vec<_>>();
        archive.write_index(&authored).unwrap();
        let first_index = fs::read_to_string(root.join("index.adoc")).unwrap();
        assert_eq!(
            first_index,
            concat!(
                "= news Archive\n\n|===\n|Post ID |Title\n\n",
                "|1 |xref:posts/1.adoc[image:evil.png]\n",
                "|2 |xref:posts/2.adoc[A bracketed title]\n",
                "|3 |xref:posts/3.adoc[One&#44; two]\n",
                "|4 |xref:posts/4.adoc[\\{secret}]\n",
                "|5 |xref:posts/5.adoc[First&#92;]\n",
                "|6 |xref:posts/6.adoc[Adjacent]\n",
                "|===\n",
            )
        );

        let inspected = archive.inspect_existing().unwrap().unwrap();
        archive.write_index(&inspected.posts).unwrap();

        assert_eq!(
            fs::read_to_string(root.join("index.adoc")).unwrap(),
            first_index
        );
        assert_eq!(
            inspected
                .posts
                .iter()
                .map(|post| fs::read(&post.path).unwrap())
                .collect::<Vec<_>>(),
            first_posts
        );
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
                title: ArchivedTitle::authored("Title"),
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
    fn write_post_at_rejects_outside_and_nonnumeric_destinations() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        let archive = Archive::new(&root, "news");
        let outside = dir.path().join("outside.adoc");

        assert!(matches!(
            archive.write_post_at(&outside, "= Outside\n"),
            Err(LjError::InvalidArchive { .. })
        ));
        assert!(!outside.exists());

        let nonnumeric = root.join("posts/draft.adoc");
        assert!(matches!(
            archive.write_post_at(&nonnumeric, "= Draft\n"),
            Err(LjError::InvalidArchive { .. })
        ));
        assert!(!nonnumeric.exists());
    }

    #[test]
    fn post_and_pending_writes_reject_nonregular_destinations() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        let archive = Archive::new(&root, "news");
        fs::create_dir_all(root.join("posts/1.adoc")).unwrap();
        fs::create_dir(root.join("posts/2.pending")).unwrap();

        assert!(matches!(
            archive.write_post(1, "= Post\n"),
            Err(LjError::InvalidArchive { .. })
        ));
        assert!(matches!(
            archive.write_pending_post(2, "pending"),
            Err(LjError::InvalidArchive { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn post_writes_reject_symlinked_posts_directory_without_writing_outside() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        let outside = dir.path().join("outside");
        let archive = Archive::new(&root, "news");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("posts")).unwrap();

        assert!(matches!(
            archive.write_post(1, "= Post\n"),
            Err(LjError::InvalidArchive { .. })
        ));
        assert!(matches!(
            archive.write_pending_post(2, "pending"),
            Err(LjError::InvalidArchive { .. })
        ));
        assert!(!outside.join("1.adoc").exists());
        assert!(!outside.join("2.pending").exists());
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
                title: ArchivedTitle::authored("A | tricky\n[title]"),
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
                title: ArchivedTitle::authored("+++<img src=x onerror=alert(1)>+++"),
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
                title: ArchivedTitle::authored("image:evil.png[] xref:1.adoc[x] {secret}"),
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
                title: ArchivedTitle::authored("Title"),
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
                    title: ArchivedTitle::authored("First"),
                    path: PathBuf::from("posts/166511.adoc"),
                },
                ArchivedPost {
                    id: 166512,
                    title: ArchivedTitle::authored("Second"),
                    path: PathBuf::from("posts/166512.adoc"),
                },
            ])
            .unwrap();

        assert_eq!(path, dir.path().join("archive/book.adoc"));
        let book = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            book,
            "= news Archive\n:doctype: book\n:toc:\n:lj-archiver-book:\n:imagesdir: posts\n:lj-archiver-post-link-prefix: posts/\n\ninclude::index.adoc[leveloffset=+1]\n\ninclude::posts/166511.adoc[leveloffset=+1]\n\ninclude::posts/166512.adoc[leveloffset=+1]\n"
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
            "= news Archive\n:doctype: book\n:toc:\n:lj-archiver-book:\n:imagesdir: posts\n:lj-archiver-post-link-prefix: posts/\n\ninclude::index.adoc[leveloffset=+1]\n"
        );
    }

    #[test]
    fn replaces_existing_index_with_exact_second_contents() {
        let dir = tempdir().unwrap();
        let archive = Archive::new(dir.path().join("archive"), "news");
        archive
            .write_index(&[ArchivedPost {
                id: 166511,
                title: ArchivedTitle::authored("First"),
                path: PathBuf::from("posts/166511.adoc"),
            }])
            .unwrap();
        let path = archive
            .write_index(&[ArchivedPost {
                id: 166512,
                title: ArchivedTitle::authored("Second"),
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
                title: ArchivedTitle::authored("First"),
                path: PathBuf::from("posts/166511.adoc"),
            }])
            .unwrap();
        let path = archive
            .write_book(&[ArchivedPost {
                id: 166512,
                title: ArchivedTitle::authored("Second"),
                path: PathBuf::from(r"posts\166512.adoc"),
            }])
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "= news Archive\n:doctype: book\n:toc:\n:lj-archiver-book:\n:imagesdir: posts\n:lj-archiver-post-link-prefix: posts/\n\ninclude::index.adoc[leveloffset=+1]\n\ninclude::posts/166512.adoc[leveloffset=+1]\n"
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
        assert_eq!(archive.read_pending_post(1).unwrap(), "first pending body");
        assert!(!archive.post_path(1).exists());

        let replacement = archive
            .write_pending_post(1, "second pending body")
            .unwrap();
        assert_eq!(replacement, path);
        assert_eq!(archive.read_pending_post(1).unwrap(), "second pending body");

        archive.remove_pending_post(1).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn pending_helpers_reject_zero_ids_and_nonregular_controlled_leaves() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("archive");
        let archive = Archive::new(&root, "news");

        assert!(matches!(
            archive.write_pending_post(0, "pending"),
            Err(LjError::InvalidArchive { .. })
        ));
        assert!(matches!(
            archive.read_pending_post(0),
            Err(LjError::InvalidArchive { .. })
        ));
        assert!(matches!(
            archive.remove_pending_post(0),
            Err(LjError::InvalidArchive { .. })
        ));

        fs::create_dir_all(root.join("posts/2.pending")).unwrap();
        assert!(matches!(
            archive.read_pending_post(2),
            Err(LjError::InvalidArchive { .. })
        ));
        assert!(matches!(
            archive.remove_pending_post(2),
            Err(LjError::InvalidArchive { .. })
        ));
    }
}
