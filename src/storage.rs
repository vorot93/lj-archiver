use crate::{LjError, Result};
use std::{io::Write, path::Path};
use tempfile::NamedTempFile;

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

pub(crate) fn adoc_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

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
