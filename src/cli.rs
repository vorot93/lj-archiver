use std::path::PathBuf;

use clap::Parser;
use url::Url;

use crate::models::RunOptions;

#[derive(Debug, Clone, Parser)]
#[command(version, about = "Archive public LiveJournal journals into AsciiDoc")]
pub struct Cli {
    #[arg(value_name = "TARGET")]
    pub target: String,

    #[arg(long, value_name = "DIR")]
    pub output_dir: Option<PathBuf>,

    #[arg(long)]
    pub no_comments: bool,

    #[arg(long)]
    pub no_media: bool,

    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,

    #[arg(long)]
    pub resume: bool,

    #[arg(long)]
    pub book: bool,

    #[arg(long, value_name = "NAME")]
    pub journal: Option<String>,

    #[arg(short, long)]
    pub verbose: bool,

    #[arg(short = 'q', long, conflicts_with = "verbose")]
    pub quiet: bool,

    #[arg(long, value_name = "STRING")]
    pub user_agent: Option<String>,

    #[arg(long, hide = true, default_value = "https://livejournal.com")]
    pub base_url: Url,
}

impl Cli {
    pub fn options(&self) -> RunOptions {
        RunOptions {
            output_dir: self.output_dir.clone(),
            include_comments: !self.no_comments,
            download_media: !self.no_media,
            concurrency: self.concurrency.max(1),
            resume: self.resume,
            generate_book: self.book,
            journal_context: self.journal.clone(),
            user_agent: self.user_agent.clone(),
            base_url: self.base_url.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn quiet_flag_parses() {
        let cli = Cli::try_parse_from(["lj-archiver", "-q", "news"]).unwrap();
        assert!(cli.quiet);
    }

    #[test]
    fn quiet_defaults_false() {
        let cli = Cli::try_parse_from(["lj-archiver", "news"]).unwrap();
        assert!(!cli.quiet);
    }

    #[test]
    fn quiet_and_verbose_conflict() {
        let err = Cli::try_parse_from(["lj-archiver", "-q", "-v", "news"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
