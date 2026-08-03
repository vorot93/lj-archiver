use crate::{LjError, Result};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExistingControlledFile {
    Missing,
    Regular,
}

pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| LjError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    let temporary_path = temporary.path().to_path_buf();
    temporary
        .write_all(contents)
        .and_then(|()| temporary.flush())
        .map_err(|source| LjError::Write {
            path: temporary_path,
            source,
        })?;
    temporary
        .into_temp_path()
        .persist(path)
        .map_err(|error| LjError::Write {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

pub(crate) fn inspect_controlled_directory(
    root: &Path,
    relative: &Path,
    require_regular_leaf_entries: bool,
) -> Result<()> {
    let components = controlled_components(root, relative)?;
    let Some(canonical_root) = canonical_root(root, false)? else {
        return Ok(());
    };
    let mut directory = root.to_path_buf();

    for component in components {
        directory.push(component);
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(invalid_controlled_path(
                    &directory,
                    format!("could not inspect controlled directory: {source}"),
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            return Err(invalid_controlled_path(
                &directory,
                "controlled directory component is not a real directory",
            ));
        }
    }

    let canonical_directory = fs::canonicalize(&directory).map_err(|source| {
        invalid_controlled_path(
            &directory,
            format!("could not canonicalize controlled directory: {source}"),
        )
    })?;
    ensure_within_root(&canonical_root, &canonical_directory, &directory)?;

    if require_regular_leaf_entries {
        let entries = fs::read_dir(&directory).map_err(|source| {
            invalid_controlled_path(
                &directory,
                format!("could not read controlled directory: {source}"),
            )
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| {
                invalid_controlled_path(
                    &directory,
                    format!("could not read controlled directory entry: {source}"),
                )
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|source| {
                invalid_controlled_path(
                    &path,
                    format!("could not inspect controlled directory entry: {source}"),
                )
            })?;
            if !file_type.is_file() {
                return Err(invalid_controlled_path(
                    &path,
                    "controlled directory entry is not a regular file",
                ));
            }
        }
    }

    Ok(())
}

pub(crate) fn prepare_controlled_file(
    root: &Path,
    relative: &Path,
) -> Result<ExistingControlledFile> {
    let components = controlled_components(root, relative)?;
    let (leaf, parents) = components
        .split_last()
        .ok_or_else(|| invalid_controlled_path(relative, "controlled file path is empty"))?;
    let canonical_root = canonical_root(root, true)?.ok_or_else(|| {
        invalid_controlled_path(root, "created archive root unexpectedly disappeared")
    })?;
    let mut parent = root.to_path_buf();

    for component in parents {
        parent.push(component);
        let metadata = match fs::symlink_metadata(&parent) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&parent) {
                    Ok(()) => {}
                    Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(source) => {
                        return Err(LjError::Write {
                            path: parent.clone(),
                            source,
                        });
                    }
                }
                fs::symlink_metadata(&parent).map_err(|source| {
                    invalid_controlled_path(
                        &parent,
                        format!("could not inspect created controlled directory: {source}"),
                    )
                })?
            }
            Err(source) => {
                return Err(invalid_controlled_path(
                    &parent,
                    format!("could not inspect controlled directory: {source}"),
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            return Err(invalid_controlled_path(
                &parent,
                "controlled parent component is not a real directory",
            ));
        }
    }

    let canonical_parent = fs::canonicalize(&parent).map_err(|source| {
        invalid_controlled_path(
            &parent,
            format!("could not canonicalize controlled parent: {source}"),
        )
    })?;
    ensure_within_root(&canonical_root, &canonical_parent, &parent)?;

    let destination = parent.join(leaf);
    inspect_controlled_destination(&destination)
}

pub(crate) fn controlled_file_exists(root: &Path, relative: &Path) -> Result<bool> {
    let components = controlled_components(root, relative)?;
    let (leaf, parents) = components
        .split_last()
        .ok_or_else(|| invalid_controlled_path(relative, "controlled file path is empty"))?;
    let Some(canonical_root) = canonical_root(root, false)? else {
        return Ok(false);
    };
    let mut parent = root.to_path_buf();

    for component in parents {
        parent.push(component);
        let metadata = match fs::symlink_metadata(&parent) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => {
                return Err(invalid_controlled_path(
                    &parent,
                    format!("could not inspect controlled directory: {source}"),
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            return Err(invalid_controlled_path(
                &parent,
                "controlled parent component is not a real directory",
            ));
        }
    }

    let canonical_parent = fs::canonicalize(&parent).map_err(|source| {
        invalid_controlled_path(
            &parent,
            format!("could not canonicalize controlled parent: {source}"),
        )
    })?;
    ensure_within_root(&canonical_root, &canonical_parent, &parent)?;
    Ok(matches!(
        inspect_controlled_destination(&parent.join(leaf))?,
        ExistingControlledFile::Regular
    ))
}

pub(crate) fn write_atomic_controlled(root: &Path, relative: &Path, contents: &[u8]) -> Result<()> {
    prepare_controlled_file(root, relative)?;
    let path = root.join(relative);
    write_atomic(&path, contents)
}

fn controlled_components<'a>(root: &Path, relative: &'a Path) -> Result<Vec<&'a std::ffi::OsStr>> {
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(component) => components.push(component),
            Component::Prefix(_)
            | Component::RootDir
            | Component::ParentDir
            | Component::CurDir => {
                return Err(invalid_controlled_path(
                    &root.join(relative),
                    "controlled path must be a normalized relative path",
                ));
            }
        }
    }
    if components.is_empty() {
        return Err(invalid_controlled_path(
            &root.join(relative),
            "controlled path is empty",
        ));
    }
    Ok(components)
}

fn canonical_root(root: &Path, create: bool) -> Result<Option<PathBuf>> {
    if create {
        fs::create_dir_all(root).map_err(|source| LjError::Write {
            path: root.to_path_buf(),
            source,
        })?;
    }
    let metadata = match fs::metadata(root) {
        Ok(metadata) => metadata,
        Err(source) if !create && source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(invalid_controlled_path(
                root,
                format!("could not inspect archive root: {source}"),
            ));
        }
    };
    if !metadata.is_dir() {
        return Err(invalid_controlled_path(
            root,
            "archive root is not a directory",
        ));
    }
    fs::canonicalize(root).map(Some).map_err(|source| {
        invalid_controlled_path(
            root,
            format!("could not canonicalize archive root: {source}"),
        )
    })
}

fn inspect_controlled_destination(path: &Path) -> Result<ExistingControlledFile> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(ExistingControlledFile::Regular),
        Ok(_) => Err(invalid_controlled_path(
            path,
            "controlled destination is not a regular file",
        )),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(ExistingControlledFile::Missing)
        }
        Err(source) => Err(invalid_controlled_path(
            path,
            format!("could not inspect controlled destination: {source}"),
        )),
    }
}

fn ensure_within_root(canonical_root: &Path, canonical: &Path, path: &Path) -> Result<()> {
    if canonical.starts_with(canonical_root) {
        Ok(())
    } else {
        Err(invalid_controlled_path(
            path,
            "controlled path resolves outside archive root",
        ))
    }
}

fn invalid_controlled_path(path: &Path, reason: impl Into<String>) -> LjError {
    LjError::InvalidArchive {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

pub(crate) fn adoc_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controlled_file_preparation_reports_missing_and_regular_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("archive");

        assert_eq!(
            prepare_controlled_file(&root, Path::new("assets/file.bin")).unwrap(),
            ExistingControlledFile::Missing
        );
        std::fs::write(root.join("assets/file.bin"), b"existing").unwrap();
        assert_eq!(
            prepare_controlled_file(&root, Path::new("assets/file.bin")).unwrap(),
            ExistingControlledFile::Regular
        );
        assert!(controlled_file_exists(&root, Path::new("assets/file.bin")).unwrap());
    }

    #[test]
    fn controlled_directory_inspection_accepts_missing_and_regular_leaf_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("archive");
        std::fs::create_dir(&root).unwrap();

        inspect_controlled_directory(&root, Path::new("assets"), true).unwrap();
        assert!(!root.join("assets").exists());

        std::fs::create_dir(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/file.bin"), b"asset").unwrap();
        inspect_controlled_directory(&root, Path::new("assets"), true).unwrap();
    }

    #[test]
    fn controlled_paths_reject_escape_components_and_nonregular_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("archive");
        std::fs::create_dir_all(root.join("assets/file.bin")).unwrap();

        for relative in [Path::new("../escape.bin"), root.as_path()] {
            assert!(matches!(
                prepare_controlled_file(&root, relative),
                Err(LjError::InvalidArchive { .. })
            ));
        }
        assert!(matches!(
            prepare_controlled_file(&root, Path::new("assets/file.bin")),
            Err(LjError::InvalidArchive { .. })
        ));
        assert!(matches!(
            inspect_controlled_directory(&root, Path::new("assets"), true),
            Err(LjError::InvalidArchive { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn controlled_write_rejects_symlinked_parent_and_destination() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("archive");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        symlink(&outside, root.join("assets")).unwrap();

        let error =
            write_atomic_controlled(&root, Path::new("assets/escape.bin"), b"must stay inside")
                .unwrap_err();
        assert!(matches!(error, LjError::InvalidArchive { .. }));
        assert!(!outside.join("escape.bin").exists());

        std::fs::remove_file(root.join("assets")).unwrap();
        std::fs::create_dir(root.join("assets")).unwrap();
        let outside_file = outside.join("outside.bin");
        std::fs::write(&outside_file, b"outside").unwrap();
        symlink(&outside_file, root.join("assets/escape.bin")).unwrap();
        assert!(matches!(
            write_atomic_controlled(&root, Path::new("assets/escape.bin"), b"must stay inside"),
            Err(LjError::InvalidArchive { .. })
        ));
        assert_eq!(std::fs::read(outside_file).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn controlled_paths_reject_socket_leaf_entries() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("archive");
        std::fs::create_dir_all(root.join("assets")).unwrap();
        let socket_path = root.join("assets/socket.bin");
        let _listener = UnixListener::bind(&socket_path).unwrap();

        assert!(matches!(
            prepare_controlled_file(&root, Path::new("assets/socket.bin")),
            Err(LjError::InvalidArchive { .. })
        ));
        assert!(matches!(
            inspect_controlled_directory(&root, Path::new("assets"), true),
            Err(LjError::InvalidArchive { .. })
        ));
    }

    #[test]
    fn atomically_replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.adoc");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"second");
    }

    #[test]
    fn serializes_document_paths_with_forward_slashes() {
        assert_eq!(
            adoc_path(Path::new(r"posts\166511.adoc")),
            "posts/166511.adoc"
        );
        assert_eq!(
            adoc_path(Path::new("../assets/image.png")),
            "../assets/image.png"
        );
    }
}
