use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, LjError>;

#[derive(Debug, Error)]
pub enum LjError {
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
    #[error("could not discover journal `{journal}`: {reason}")]
    Discovery { journal: String, reason: String },
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
