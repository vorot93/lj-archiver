use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, LjError>;

#[derive(Debug, Error)]
pub enum LjError {
    #[error("invalid run options: {reason}")]
    InvalidOptions { reason: String },
    #[error("invalid target `{input}`: {reason}")]
    InvalidTarget { input: String, reason: String },
    #[error("http failure for {url}: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("unexpected http status {status} for {url}")]
    HttpStatus { url: String, status: u16 },
    #[error("response for {url} exceeded {limit} bytes")]
    ResponseTooLarge { url: String, limit: usize },
    #[error("could not parse {url}: {reason}")]
    Parse { url: String, reason: String },
    #[error(
        "post is age-gated and the age-confirmation was rejected \
         (LiveJournal may have changed the mechanism): {url}"
    )]
    AgeGated { url: String },
    #[error("link integrity failure for {url}: {reason}")]
    LinkIntegrity { url: String, reason: String },
    #[error("could not discover journal `{journal}`: {reason}")]
    Discovery { journal: String, reason: String },
    #[error("invalid archive at {path}: {reason}")]
    InvalidArchive { path: PathBuf, reason: String },
    #[error("archive at {path} is being updated by another lj-archiver process")]
    ArchiveBusy { path: PathBuf },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The full message of `error`, including every `#[source]` cause.
///
/// `thiserror`'s generated `Display` prints one variant's own message and never
/// walks the chain, so what actually went wrong — the `ConnectionRefused` under
/// an HTTP failure, the `io::Error` under a write failure — is invisible to a
/// caller that only formats the outermost error.
///
/// A cause whose message the accumulated text already carries is skipped:
/// several variants interpolate `{source}` into their own message, and repeating
/// it reads as a stutter.
pub fn error_chain<E: std::error::Error + ?Sized>(error: &E) -> String {
    let mut message = error.to_string();
    let mut cause = error.source();

    while let Some(current) = cause {
        let current_message = current.to_string();
        if !message.contains(&current_message) {
            message.push_str(": ");
            message.push_str(&current_message);
        }
        cause = current.source();
    }

    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Error)]
    #[error("outer")]
    struct Outer(#[source] Inner);

    #[derive(Debug, Error)]
    #[error("inner")]
    struct Inner(#[source] std::io::Error);

    #[test]
    fn walks_every_source_in_the_chain() {
        let error = Outer(Inner(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        )));

        assert_eq!(error.to_string(), "outer");
        assert_eq!(error_chain(&error), "outer: inner: connection refused");
    }

    #[test]
    fn does_not_repeat_a_cause_a_variant_already_interpolated() {
        let error = LjError::Write {
            path: PathBuf::from("/archive/index.adoc"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied"),
        };

        assert_eq!(
            error_chain(&error),
            "could not write /archive/index.adoc: permission denied"
        );
    }
}
