use std::path::PathBuf;

use chrono::{DateTime, Utc};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveTarget {
    Journal { journal: String },
    Post { journal: String, id: u64, url: Url },
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub output_dir: Option<PathBuf>,
    pub include_comments: bool,
    pub download_media: bool,
    pub concurrency: usize,
    pub resume: bool,
    pub journal_context: Option<String>,
    /// Optional `User-Agent` header sent on every request. `None` (the default)
    /// sends no `User-Agent`.
    pub user_agent: Option<String>,
    pub base_url: Url,
}

#[derive(Debug, Clone)]
pub struct Post {
    pub journal: String,
    pub id: u64,
    pub url: Url,
    pub title: String,
    pub author: String,
    pub poster: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub tags: Vec<String>,
    pub body_html: String,
    pub comments: Vec<Comment>,
}

#[derive(Debug, Clone)]
pub struct Comment {
    pub id: Option<String>,
    pub parent_id: Option<String>,
    pub depth: usize,
    pub author: String,
    pub subject: Option<String>,
    pub posted_at: Option<DateTime<Utc>>,
    pub body_html: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedPost {
    pub id: u64,
    pub title: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaAsset {
    pub original_url: Url,
    pub relative_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct PostFailure {
    pub id: u64,
    pub url: Url,
    pub message: String,
}
