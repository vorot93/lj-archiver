//! Pure, Elm-style state for the GUI: `App`/`State`/`Message` plus the
//! `update_state` reducer. Deliberately free of iced (and of `rfd`/`open`) so
//! it is fully unit-testable without a display; the iced view/update glue
//! that actually constructs and consumes this API lives in `app.rs`.

use lj_archiver::{
    models::{ArchiveTarget, PostFailure, RunOptions},
    progress::ProgressEvent,
    run::RunOutcome,
    target::{is_valid_journal_name, journals_alias, parse_target},
};
use std::path::{Path, PathBuf};
use url::Url;

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub target: String,
    pub folder: Option<PathBuf>,
    pub include_comments: bool,
    pub download_media: bool,
    pub generate_book: bool,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            target: String::new(),
            folder: None,
            include_comments: true,
            download_media: true,
            generate_book: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Phase {
    Discovering { found: usize },
    Archiving { done: usize, total: usize },
}

#[derive(Debug, Clone, Default)]
pub enum State {
    #[default]
    Idle,
    Running(Phase),
    Done {
        root: PathBuf,
        archived: usize,
        skipped: usize,
        failed: usize,
        failed_pages: usize,
        failures: Vec<PostFailure>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub enum Message {
    TargetChanged(String),
    ChooseFolder,
    FolderChosen(Option<PathBuf>),
    ToggleComments(bool),
    ToggleMedia(bool),
    ToggleBook(bool),
    Start,
    Progress(ProgressEvent),
    Finished(Result<RunOutcome, String>),
    OpenFolder,
    OpenUrl(String),
}

#[derive(Debug, Clone, Default)]
pub struct App {
    pub config: RunConfig,
    pub state: State,
}

impl App {
    /// Pure state transitions. Side-effecting messages (`ChooseFolder`,
    /// `OpenFolder`, `OpenUrl`, and the launch part of `Start`) are handled by
    /// the iced `update` in app.rs; here `Start` only moves to `Running`.
    pub fn update_state(&mut self, message: &Message) {
        match message {
            Message::TargetChanged(target) => self.config.target = target.clone(),
            Message::FolderChosen(folder) => {
                if folder.is_some() {
                    self.config.folder = folder.clone();
                }
            }
            Message::ToggleComments(on) => self.config.include_comments = *on,
            Message::ToggleMedia(on) => self.config.download_media = *on,
            Message::ToggleBook(on) => self.config.generate_book = *on,
            Message::Start => self.state = State::Running(Phase::Discovering { found: 0 }),
            Message::Progress(event) => self.apply_progress(event),
            Message::Finished(Ok(outcome)) => {
                self.state = State::Done {
                    root: outcome.root.clone(),
                    archived: outcome.archived,
                    skipped: outcome.skipped,
                    failed: outcome.failed,
                    failed_pages: outcome.failed_pages,
                    failures: outcome.failures.clone(),
                };
            }
            Message::Finished(Err(message)) => {
                self.state = State::Error {
                    message: message.clone(),
                };
            }
            Message::ChooseFolder | Message::OpenFolder | Message::OpenUrl(_) => {}
        }
    }

    fn apply_progress(&mut self, event: &ProgressEvent) {
        // Task::batch polls the progress stream and the terminal Finished
        // concurrently (no ordering guarantee), so a late/trailing event must
        // never clobber a terminal Done/Error back into Running.
        if !matches!(self.state, State::Running(_)) {
            return;
        }
        match event {
            ProgressEvent::DiscoveryStarted | ProgressEvent::SingleStarted { .. } => {
                self.state = State::Running(Phase::Discovering { found: 0 });
            }
            ProgressEvent::DiscoveryTick { found, .. } => {
                self.state = State::Running(Phase::Discovering { found: *found });
            }
            ProgressEvent::ArchivingStarted { total } => {
                self.state = State::Running(Phase::Archiving {
                    done: 0,
                    total: *total,
                });
            }
            ProgressEvent::PostDone { .. }
            | ProgressEvent::PostSkipped { .. }
            | ProgressEvent::PostFailed { .. } => {
                if let State::Running(Phase::Archiving { done, .. }) = &mut self.state {
                    *done += 1;
                }
            }
            ProgressEvent::ArchivingFinished
            | ProgressEvent::Summary { .. }
            | ProgressEvent::SingleDone { .. } => {}
        }
    }

    pub fn can_start(&self) -> bool {
        !self.config.target.trim().is_empty()
            && self.config.folder.is_some()
            && !matches!(self.state, State::Running(_))
    }

    /// The target string and `RunOptions` for a launch, or `None` if not
    /// startable. Auto-detects resume from the chosen folder and rejects an
    /// existing archive whose journal identity cannot be verified or matched.
    pub fn run_request(&self) -> Result<Option<(String, RunOptions)>, String> {
        if !self.can_start() {
            return Ok(None);
        }
        let folder = self
            .config
            .folder
            .clone()
            .expect("can_start requires a chosen folder");
        let target = self.config.target.trim();
        let requested_journal =
            match parse_target(target, None).map_err(|error| error.to_string())? {
                ArchiveTarget::Journal { journal } | ArchiveTarget::Post { journal, .. } => journal,
            };
        let existing_journal = archive_journal(&folder)?;
        if let Some(existing_journal) = &existing_journal
            && !journals_alias(existing_journal, &requested_journal)
        {
            return Err(format!(
                "selected folder contains archive for {existing_journal}, not requested journal {requested_journal}"
            ));
        }
        let resume = existing_journal.is_some() && folder_has_existing_posts(&folder)?;
        let options = RunOptions {
            output_dir: Some(folder),
            include_comments: self.config.include_comments,
            download_media: self.config.download_media,
            concurrency: 4,
            resume,
            generate_book: self.config.generate_book,
            journal_context: None,
            user_agent: None,
            base_url: Url::parse("https://livejournal.com").expect("static url is valid"),
        };
        Ok(Some((target.to_string(), options)))
    }
}

fn archive_journal(folder: &Path) -> Result<Option<String>, String> {
    let index = folder.join("index.adoc");
    let has_posts = folder_has_existing_posts(folder)?;
    let has_index = match std::fs::metadata(&index) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!(
                "archive identity cannot be verified: could not inspect index.adoc: {error}"
            ));
        }
    };
    if !has_index && !has_posts {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&index).map_err(|error| {
        format!("archive identity cannot be verified: could not read index.adoc: {error}")
    })?;
    let first_line = contents.lines().next().unwrap_or_default();
    let Some(journal) = first_line
        .strip_prefix("= ")
        .and_then(|line| line.strip_suffix(" Archive"))
        .map(str::trim)
        .filter(|journal| is_valid_journal_name(journal))
    else {
        return Err(
            "archive identity cannot be verified: index.adoc has an invalid archive title"
                .to_string(),
        );
    };

    Ok(Some(journal.to_string()))
}

/// Whether `folder/posts/` already contains at least one `.adoc` file.
pub fn folder_has_existing_posts(folder: &Path) -> Result<bool, String> {
    let entries = match std::fs::read_dir(folder.join("posts")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "archive identity cannot be verified: could not inspect posts folder: {error}"
            ));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "archive identity cannot be verified: could not inspect posts folder entry: {error}"
            )
        })?;
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "adoc")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lj_archiver::{models::PostFailure, progress::ProgressEvent, run::RunOutcome};
    use std::path::PathBuf;
    use tempfile::tempdir;
    use url::Url;

    fn outcome(root: PathBuf, failed: usize, failures: Vec<PostFailure>) -> RunOutcome {
        RunOutcome {
            root,
            archived: 3,
            skipped: 0,
            failed,
            failed_pages: 0,
            failures,
            journal: Some("news".to_string()),
        }
    }

    #[test]
    fn defaults_to_idle_with_comments_and_media_on() {
        let app = App::default();
        assert!(matches!(app.state, State::Idle));
        assert!(app.config.include_comments);
        assert!(app.config.download_media);
        assert!(!app.config.generate_book);
    }

    #[test]
    fn cannot_start_without_target_and_folder() {
        let mut app = App::default();
        assert!(!app.can_start());
        app.update_state(&Message::TargetChanged("news".to_string()));
        assert!(!app.can_start()); // still no folder
        app.update_state(&Message::FolderChosen(Some(PathBuf::from("/tmp/out"))));
        assert!(app.can_start());
    }

    #[test]
    fn toggles_update_config() {
        let mut app = App::default();
        app.update_state(&Message::ToggleComments(false));
        app.update_state(&Message::ToggleMedia(false));
        app.update_state(&Message::ToggleBook(true));
        assert!(!app.config.include_comments);
        assert!(!app.config.download_media);
        assert!(app.config.generate_book);
    }

    #[test]
    fn start_enters_discovering_and_progress_advances_bar() {
        let mut app = App::default();
        app.update_state(&Message::TargetChanged("news".to_string()));
        app.update_state(&Message::FolderChosen(Some(PathBuf::from("/tmp/out"))));
        app.update_state(&Message::Start);
        assert!(matches!(
            app.state,
            State::Running(Phase::Discovering { found: 0 })
        ));

        app.update_state(&Message::Progress(ProgressEvent::DiscoveryTick {
            label: "/2024/".to_string(),
            found: 5,
        }));
        assert!(matches!(
            app.state,
            State::Running(Phase::Discovering { found: 5 })
        ));

        app.update_state(&Message::Progress(ProgressEvent::ArchivingStarted {
            total: 2,
        }));
        assert!(matches!(
            app.state,
            State::Running(Phase::Archiving { done: 0, total: 2 })
        ));

        app.update_state(&Message::Progress(ProgressEvent::PostDone {
            id: 1,
            title: "t".to_string(),
        }));
        app.update_state(&Message::Progress(ProgressEvent::PostFailed { id: 2 }));
        assert!(matches!(
            app.state,
            State::Running(Phase::Archiving { done: 2, total: 2 })
        ));
    }

    #[test]
    fn finished_ok_with_failures_reaches_done_carrying_failures() {
        let mut app = App::default();
        let failures = vec![PostFailure {
            id: 2,
            url: Url::parse("https://news.livejournal.com/2.html").unwrap(),
            message: "boom".to_string(),
        }];
        app.update_state(&Message::Finished(Ok(outcome(
            PathBuf::from("/tmp/out"),
            1,
            failures.clone(),
        ))));
        match &app.state {
            State::Done {
                failed,
                failures: got,
                root,
                ..
            } => {
                assert_eq!(*failed, 1);
                assert_eq!(got.len(), 1);
                assert_eq!(root, &PathBuf::from("/tmp/out"));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn finished_ok_retains_failed_archive_pages() {
        let mut app = App::default();
        let outcome = RunOutcome {
            root: PathBuf::from("/tmp/out"),
            archived: 3,
            skipped: 0,
            failed: 0,
            failed_pages: 2,
            failures: vec![],
            journal: Some("news".to_string()),
        };

        app.update_state(&Message::Finished(Ok(outcome)));

        assert!(matches!(
            app.state,
            State::Done {
                failed_pages: 2,
                failed: 0,
                ..
            }
        ));
    }

    #[test]
    fn finished_err_reaches_error() {
        let mut app = App::default();
        app.update_state(&Message::Finished(Err("bad target".to_string())));
        assert!(matches!(app.state, State::Error { .. }));
    }

    #[test]
    fn late_progress_event_does_not_reset_terminal_state() {
        let mut app = App::default();
        app.update_state(&Message::Finished(Ok(outcome(
            PathBuf::from("/tmp/out"),
            0,
            vec![],
        ))));
        assert!(matches!(app.state, State::Done { .. }));
        // A stray progress event arriving after completion must be ignored.
        app.update_state(&Message::Progress(ProgressEvent::ArchivingStarted {
            total: 5,
        }));
        assert!(
            matches!(app.state, State::Done { .. }),
            "late progress event must not reset Done back to Running"
        );
    }

    #[test]
    fn detects_existing_posts_for_resume() {
        let dir = tempdir().unwrap();
        assert!(!folder_has_existing_posts(dir.path()).unwrap());
        let posts = dir.path().join("posts");
        std::fs::create_dir_all(&posts).unwrap();
        std::fs::write(posts.join("166511.adoc"), "= T\n").unwrap();
        assert!(folder_has_existing_posts(dir.path()).unwrap());
    }

    #[test]
    fn run_request_builds_options_with_autoresume() {
        let dir = tempdir().unwrap();
        let posts = dir.path().join("posts");
        std::fs::create_dir_all(&posts).unwrap();
        std::fs::write(posts.join("1.adoc"), "= T\n").unwrap();
        std::fs::write(dir.path().join("index.adoc"), "= news Archive\n").unwrap();

        let mut app = App::default();
        app.update_state(&Message::TargetChanged("news".to_string()));
        app.update_state(&Message::FolderChosen(Some(dir.path().to_path_buf())));
        app.update_state(&Message::ToggleMedia(false));
        app.update_state(&Message::ToggleBook(true));

        let (target, options) = app.run_request().unwrap().unwrap();
        assert_eq!(target, "news");
        assert_eq!(options.output_dir.as_deref(), Some(dir.path()));
        assert!(options.resume); // posts/ already has a .adoc
        assert!(!options.download_media);
        assert!(options.generate_book);
        assert!(options.include_comments);
        assert_eq!(options.concurrency, 4);
    }

    #[test]
    fn matching_archive_alias_can_resume() {
        let dir = tempdir().unwrap();
        let posts = dir.path().join("posts");
        std::fs::create_dir_all(&posts).unwrap();
        std::fs::write(posts.join("1.adoc"), "= T\n").unwrap();
        std::fs::write(dir.path().join("index.adoc"), "= evo-lutio Archive\n").unwrap();

        let mut app = App::default();
        app.update_state(&Message::TargetChanged("evo_lutio".to_string()));
        app.update_state(&Message::FolderChosen(Some(dir.path().to_path_buf())));

        let (target, options) = app.run_request().unwrap().unwrap();
        assert_eq!(target, "evo_lutio");
        assert!(options.resume);
    }

    #[test]
    fn mismatched_archive_is_rejected() {
        let dir = tempdir().unwrap();
        let posts = dir.path().join("posts");
        std::fs::create_dir_all(&posts).unwrap();
        std::fs::write(posts.join("1.adoc"), "= T\n").unwrap();
        std::fs::write(dir.path().join("index.adoc"), "= news Archive\n").unwrap();

        let mut app = App::default();
        app.update_state(&Message::TargetChanged("other".to_string()));
        app.update_state(&Message::FolderChosen(Some(dir.path().to_path_buf())));

        let error = app.run_request().unwrap_err();
        assert!(error.contains("news"));
        assert!(error.contains("other"));
    }

    #[test]
    fn unverifiable_nonempty_archive_is_rejected() {
        let dir = tempdir().unwrap();
        let posts = dir.path().join("posts");
        std::fs::create_dir_all(&posts).unwrap();
        std::fs::write(posts.join("1.adoc"), "= T\n").unwrap();

        let mut app = App::default();
        app.update_state(&Message::TargetChanged("news".to_string()));
        app.update_state(&Message::FolderChosen(Some(dir.path().to_path_buf())));

        let error = app.run_request().unwrap_err();
        assert!(error.contains("archive identity cannot be verified"));
    }

    #[test]
    fn empty_folder_starts_without_resume() {
        let dir = tempdir().unwrap();

        let mut app = App::default();
        app.update_state(&Message::TargetChanged("news".to_string()));
        app.update_state(&Message::FolderChosen(Some(dir.path().to_path_buf())));

        let (_, options) = app.run_request().unwrap().unwrap();
        assert!(!options.resume);
    }

    #[test]
    fn selected_output_file_is_rejected_when_inspection_fails() {
        let dir = tempdir().unwrap();
        let output = dir.path().join("output");
        std::fs::write(&output, "not a directory\n").unwrap();

        let mut app = App::default();
        app.update_state(&Message::TargetChanged("news".to_string()));
        app.update_state(&Message::FolderChosen(Some(output)));

        let error = app.run_request().unwrap_err();
        assert!(error.contains("archive identity cannot be verified"));
    }

    #[test]
    fn posts_file_is_rejected_when_inspection_fails() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("posts"), "not a directory\n").unwrap();

        let mut app = App::default();
        app.update_state(&Message::TargetChanged("news".to_string()));
        app.update_state(&Message::FolderChosen(Some(dir.path().to_path_buf())));

        let error = app.run_request().unwrap_err();
        assert!(error.contains("archive identity cannot be verified"));
    }
}
