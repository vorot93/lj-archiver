//! Pure, Elm-style state for the GUI: `App`/`State`/`Message` plus the
//! `update_state` reducer. Deliberately free of iced (and of `rfd`/`open`) so
//! it is fully unit-testable without a display; the iced view/update glue
//! that actually constructs and consumes this API lives in `app.rs`.

use lj_archiver::{
    archive::Archive,
    models::{ArchiveTarget, PostFailure, RunOptions},
    progress::ProgressEvent,
    run::RunOutcome,
    target::parse_target,
};
use std::path::PathBuf;
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
    Progress {
        run_id: u64,
        event: ProgressEvent,
    },
    Finished {
        run_id: u64,
        result: Result<RunOutcome, String>,
    },
    OpenFolder,
    OpenUrl(String),
}

#[derive(Debug, Clone, Default)]
pub struct App {
    pub config: RunConfig,
    pub state: State,
    next_run_id: u64,
    active_run_id: Option<u64>,
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
            Message::Start => {
                if matches!(self.state, State::Running(_)) {
                    return;
                }
                self.next_run_id = self.next_run_id.wrapping_add(1);
                if self.next_run_id == 0 {
                    self.next_run_id = 1;
                }
                self.active_run_id = Some(self.next_run_id);
                self.state = State::Running(Phase::Discovering { found: 0 });
            }
            Message::Progress { run_id, event } => {
                if Some(*run_id) != self.active_run_id {
                    return;
                }
                self.apply_progress(event);
            }
            Message::Finished { run_id, result } => {
                if Some(*run_id) != self.active_run_id {
                    return;
                }
                match result {
                    Ok(outcome) => {
                        self.state = State::Done {
                            root: outcome.root.clone(),
                            archived: outcome.archived,
                            skipped: outcome.skipped,
                            failed: outcome.failed,
                            failed_pages: outcome.failed_pages,
                            failures: outcome.failures.clone(),
                        };
                    }
                    Err(message) => {
                        self.state = State::Error {
                            message: message.clone(),
                        };
                    }
                }
                self.active_run_id = None;
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

    pub(crate) fn active_run_id(&self) -> Option<u64> {
        self.active_run_id
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
        std::fs::read_dir(&folder).map_err(|error| {
            format!(
                "selected folder failed archive validation: could not read selected directory {}: {error}",
                folder.display()
            )
        })?;
        let existing = Archive::new(&folder, requested_journal)
            .inspect_existing()
            .map_err(|error| format!("selected folder failed archive validation: {error}"))?;
        let resume = existing.is_some_and(|archive| !archive.posts.is_empty());
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

#[cfg(test)]
mod tests {
    use super::*;
    use lj_archiver::{
        models::PostFailure,
        progress::{Progress, ProgressEvent},
        run::RunOutcome,
    };
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;
    use url::Url;

    const ARCHIVE_VALIDATION_PREFIX: &str = "selected folder failed archive validation: ";

    #[derive(Debug, PartialEq, Eq)]
    struct TreeEntry {
        path: PathBuf,
        is_dir: bool,
        contents: Vec<u8>,
    }

    fn app_for_folder(target: &str, folder: &Path) -> App {
        let mut app = App::default();
        app.update_state(&Message::TargetChanged(target.to_string()));
        app.update_state(&Message::FolderChosen(Some(folder.to_path_buf())));
        app
    }

    fn write_valid_index(root: &Path, journal: &str) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("index.adoc"), format!("= {journal} Archive\n")).unwrap();
    }

    fn write_public_post(root: &Path, filename: &str, contents: &str) {
        let posts = root.join("posts");
        std::fs::create_dir_all(&posts).unwrap();
        std::fs::write(posts.join(filename), contents).unwrap();
    }

    fn snapshot_tree(root: &Path) -> Vec<TreeEntry> {
        let Ok(root_metadata) = std::fs::symlink_metadata(root) else {
            return Vec::new();
        };
        if !root_metadata.is_dir() {
            return vec![TreeEntry {
                path: PathBuf::from("."),
                is_dir: false,
                contents: std::fs::read(root).unwrap(),
            }];
        }

        let mut snapshot = vec![TreeEntry {
            path: PathBuf::from("."),
            is_dir: true,
            contents: Vec::new(),
        }];
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let metadata = entry.metadata().unwrap();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                if metadata.is_dir() {
                    snapshot.push(TreeEntry {
                        path: relative,
                        is_dir: true,
                        contents: Vec::new(),
                    });
                    pending.push(path);
                } else {
                    snapshot.push(TreeEntry {
                        path: relative,
                        is_dir: false,
                        contents: std::fs::read(path).unwrap(),
                    });
                }
            }
        }
        snapshot.sort_by(|left, right| left.path.cmp(&right.path));
        snapshot
    }

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

    fn start_run(app: &mut App) -> u64 {
        app.update_state(&Message::Start);
        app.active_run_id().expect("Start should activate a run")
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
        let run_id = start_run(&mut app);
        assert_eq!(run_id, 1);
        assert!(matches!(
            app.state,
            State::Running(Phase::Discovering { found: 0 })
        ));

        app.update_state(&Message::Progress {
            run_id,
            event: ProgressEvent::DiscoveryTick {
                label: "/2024/".to_string(),
                found: 5,
            },
        });
        assert!(matches!(
            app.state,
            State::Running(Phase::Discovering { found: 5 })
        ));

        app.update_state(&Message::Progress {
            run_id,
            event: ProgressEvent::ArchivingStarted { total: 2 },
        });
        assert!(matches!(
            app.state,
            State::Running(Phase::Archiving { done: 0, total: 2 })
        ));

        app.update_state(&Message::Progress {
            run_id,
            event: ProgressEvent::PostDone {
                id: 1,
                title: "t".to_string(),
            },
        });
        app.update_state(&Message::Progress {
            run_id,
            event: ProgressEvent::PostFailed { id: 2 },
        });
        assert!(matches!(
            app.state,
            State::Running(Phase::Archiving { done: 2, total: 2 })
        ));
    }

    #[test]
    fn finished_ok_with_failures_reaches_done_carrying_failures() {
        let mut app = App::default();
        let run_id = start_run(&mut app);
        let failures = vec![PostFailure {
            id: 2,
            url: Url::parse("https://news.livejournal.com/2.html").unwrap(),
            message: "boom".to_string(),
        }];
        app.update_state(&Message::Finished {
            run_id,
            result: Ok(outcome(PathBuf::from("/tmp/out"), 1, failures.clone())),
        });
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
        let run_id = start_run(&mut app);
        let outcome = RunOutcome {
            root: PathBuf::from("/tmp/out"),
            archived: 3,
            skipped: 0,
            failed: 0,
            failed_pages: 2,
            failures: vec![],
            journal: Some("news".to_string()),
        };

        app.update_state(&Message::Finished {
            run_id,
            result: Ok(outcome),
        });

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
    fn matching_finished_is_authoritative_when_bounded_progress_is_dropped() {
        let mut app = App::default();
        let run_id = start_run(&mut app);
        let (tx, rx) = futures::channel::mpsc::channel(1);
        let progress = Progress::channel(tx);
        for found in 0..8 {
            progress.discovery_tick("/2024/", found);
        }
        drop(rx);
        progress.archiving_started(10);
        progress.post_done(1, "One");
        drop(progress);

        let failures = vec![PostFailure {
            id: 9,
            url: Url::parse("https://news.livejournal.com/9.html").unwrap(),
            message: "failed post".to_string(),
        }];
        app.update_state(&Message::Finished {
            run_id,
            result: Ok(RunOutcome {
                root: PathBuf::from("/archive/output"),
                archived: 7,
                skipped: 2,
                failed: 1,
                failed_pages: 3,
                failures: failures.clone(),
                journal: Some("news".to_string()),
            }),
        });

        assert_eq!(app.active_run_id(), None);
        match &app.state {
            State::Done {
                root,
                archived,
                skipped,
                failed,
                failed_pages,
                failures: actual_failures,
            } => {
                assert_eq!(root, &PathBuf::from("/archive/output"));
                assert_eq!((*archived, *skipped, *failed), (7, 2, 1));
                assert_eq!(*failed_pages, 3);
                assert_eq!(actual_failures.len(), 1);
                assert_eq!(actual_failures[0].id, failures[0].id);
                assert_eq!(actual_failures[0].url, failures[0].url);
                assert_eq!(actual_failures[0].message, failures[0].message);
            }
            other => panic!("expected authoritative Done state, got {other:?}"),
        }
    }

    #[test]
    fn finished_err_reaches_error() {
        let mut app = App::default();
        let run_id = start_run(&mut app);
        app.update_state(&Message::Finished {
            run_id,
            result: Err("bad target".to_string()),
        });
        assert!(matches!(app.state, State::Error { .. }));
    }

    #[test]
    fn late_progress_event_does_not_reset_terminal_state() {
        let mut app = App::default();
        let run_id = start_run(&mut app);
        app.update_state(&Message::Finished {
            run_id,
            result: Ok(outcome(PathBuf::from("/tmp/out"), 0, vec![])),
        });
        assert!(matches!(app.state, State::Done { .. }));
        // A stray progress event arriving after completion must be ignored.
        app.update_state(&Message::Progress {
            run_id,
            event: ProgressEvent::ArchivingStarted { total: 5 },
        });
        assert!(
            matches!(app.state, State::Done { .. }),
            "late progress event must not reset Done back to Running"
        );
    }

    #[test]
    fn late_run_one_messages_do_not_mutate_run_two() {
        let mut app = App::default();
        let run_one = start_run(&mut app);
        app.update_state(&Message::Finished {
            run_id: run_one,
            result: Ok(outcome(PathBuf::from("/tmp/run-one"), 0, vec![])),
        });
        let run_two = start_run(&mut app);
        assert_eq!((run_one, run_two), (1, 2));
        app.update_state(&Message::Progress {
            run_id: run_two,
            event: ProgressEvent::ArchivingStarted { total: 3 },
        });
        let run_two_snapshot = format!("{app:?}");

        let late_messages = [
            Message::Progress {
                run_id: run_one,
                event: ProgressEvent::DiscoveryTick {
                    label: "/late-run-one/".to_string(),
                    found: 99,
                },
            },
            Message::Progress {
                run_id: run_one,
                event: ProgressEvent::PostDone {
                    id: 1,
                    title: "late".to_string(),
                },
            },
            Message::Finished {
                run_id: run_one,
                result: Ok(outcome(PathBuf::from("/tmp/late-success"), 0, vec![])),
            },
            Message::Finished {
                run_id: run_one,
                result: Err("late error".to_string()),
            },
        ];
        for message in late_messages {
            app.update_state(&message);
            assert_eq!(format!("{app:?}"), run_two_snapshot);
        }
    }

    #[test]
    fn wrong_generation_messages_are_ignored_while_running() {
        let mut app = App::default();
        let run_id = start_run(&mut app);
        let running_snapshot = format!("{app:?}");

        let wrong_generation = run_id + 1;
        let messages = [
            Message::Progress {
                run_id: wrong_generation,
                event: ProgressEvent::DiscoveryTick {
                    label: "/wrong/".to_string(),
                    found: 9,
                },
            },
            Message::Finished {
                run_id: wrong_generation,
                result: Ok(outcome(PathBuf::from("/tmp/wrong"), 0, vec![])),
            },
            Message::Finished {
                run_id: wrong_generation,
                result: Err("wrong generation".to_string()),
            },
        ];
        for message in messages {
            app.update_state(&message);
            assert_eq!(format!("{app:?}"), running_snapshot);
        }
    }

    #[test]
    fn matching_terminal_result_retires_generation_and_makes_trailing_messages_inert() {
        let mut app = App::default();
        let run_id = start_run(&mut app);
        app.update_state(&Message::Finished {
            run_id,
            result: Ok(outcome(PathBuf::from("/tmp/out"), 0, vec![])),
        });
        assert_eq!(app.active_run_id(), None);
        let terminal_snapshot = format!("{app:?}");

        let trailing_messages = [
            Message::Progress {
                run_id,
                event: ProgressEvent::DiscoveryTick {
                    label: "/trailing/".to_string(),
                    found: 50,
                },
            },
            Message::Finished {
                run_id,
                result: Err("duplicate terminal result".to_string()),
            },
        ];
        for message in trailing_messages {
            app.update_state(&message);
            assert_eq!(format!("{app:?}"), terminal_snapshot);
        }
    }

    #[test]
    fn valid_restart_gets_fresh_nonzero_generation_even_across_wrap() {
        let mut app = App {
            next_run_id: u64::MAX - 1,
            ..App::default()
        };

        let before_wrap = start_run(&mut app);
        assert_eq!(before_wrap, u64::MAX);
        app.update_state(&Message::Finished {
            run_id: before_wrap,
            result: Err("finish before wrap".to_string()),
        });

        let after_wrap = start_run(&mut app);
        assert_eq!(after_wrap, 1);
        assert_ne!(after_wrap, 0);
    }

    #[test]
    fn start_while_running_does_not_advance_generation() {
        let mut app = App::default();
        let run_id = start_run(&mut app);
        let running_snapshot = format!("{app:?}");

        app.update_state(&Message::Start);

        assert_eq!(app.active_run_id(), Some(run_id));
        assert_eq!(format!("{app:?}"), running_snapshot);
    }

    #[test]
    fn unsafe_output_folder_matrix_is_rejected_without_mutation() {
        let dir = tempdir().unwrap();
        let cases = [
            "unrelated-file",
            "lone-book",
            "assets-only",
            "posts-without-index",
            "malformed-index",
            "mismatched-index",
            "invalid-journal-index",
            "index-as-directory",
            "book-as-directory",
            "public-adoc-as-directory",
            "posts-as-file",
            "zero-post-id",
            "nonnumeric-post-id",
            "duplicate-numeric-post-id",
            "missing-post-title",
            "malformed-post-title",
            "selected-path-is-file",
            "selected-path-vanished",
        ];
        let mut accepted = Vec::new();
        let mut unstable_errors = Vec::new();

        for name in cases {
            let root = dir.path().join(name);
            match name {
                "unrelated-file" => {
                    std::fs::create_dir(&root).unwrap();
                    std::fs::write(root.join("notes.txt"), "unrelated\n").unwrap();
                }
                "lone-book" => {
                    std::fs::create_dir(&root).unwrap();
                    std::fs::write(root.join("book.adoc"), "= news Archive\n").unwrap();
                }
                "assets-only" => {
                    std::fs::create_dir_all(root.join("assets")).unwrap();
                    std::fs::write(root.join("assets/image.jpg"), "asset").unwrap();
                }
                "posts-without-index" => write_public_post(&root, "1.adoc", "= One\n"),
                "malformed-index" => {
                    std::fs::create_dir(&root).unwrap();
                    std::fs::write(root.join("index.adoc"), "news Archive\n").unwrap();
                }
                "mismatched-index" => write_valid_index(&root, "other"),
                "invalid-journal-index" => write_valid_index(&root, "bad.name"),
                "index-as-directory" => {
                    std::fs::create_dir_all(root.join("index.adoc")).unwrap();
                }
                "book-as-directory" => {
                    write_valid_index(&root, "news");
                    std::fs::create_dir(root.join("book.adoc")).unwrap();
                }
                "public-adoc-as-directory" => {
                    write_valid_index(&root, "news");
                    std::fs::create_dir_all(root.join("posts/1.adoc")).unwrap();
                }
                "posts-as-file" => {
                    write_valid_index(&root, "news");
                    std::fs::write(root.join("posts"), "not a directory\n").unwrap();
                }
                "zero-post-id" => {
                    write_valid_index(&root, "news");
                    write_public_post(&root, "0.adoc", "= Zero\n");
                }
                "nonnumeric-post-id" => {
                    write_valid_index(&root, "news");
                    write_public_post(&root, "not-a-number.adoc", "= Text\n");
                }
                "duplicate-numeric-post-id" => {
                    write_valid_index(&root, "news");
                    write_public_post(&root, "1.adoc", "= One\n");
                    write_public_post(&root, "01.adoc", "= Duplicate\n");
                }
                "missing-post-title" => {
                    write_valid_index(&root, "news");
                    write_public_post(&root, "1.adoc", "");
                }
                "malformed-post-title" => {
                    write_valid_index(&root, "news");
                    write_public_post(&root, "1.adoc", "== Not level zero\n");
                }
                "selected-path-is-file" => {
                    std::fs::write(&root, "not a directory\n").unwrap();
                }
                "selected-path-vanished" => {}
                other => panic!("unhandled unsafe-folder fixture {other}"),
            }

            let before = snapshot_tree(&root);
            match app_for_folder("news", &root).run_request() {
                Ok(Some(_)) => accepted.push(name),
                Ok(None) => panic!("{name}: configured request unexpectedly was not startable"),
                Err(error) => {
                    if !error.starts_with(ARCHIVE_VALIDATION_PREFIX) {
                        unstable_errors.push((name, error));
                    }
                }
            }
            assert_eq!(
                snapshot_tree(&root),
                before,
                "{name}: validation mutated root"
            );
        }

        assert!(
            accepted.is_empty(),
            "manual GUI checks accepted unsafe folders: {accepted:?}"
        );
        assert!(
            unstable_errors.is_empty(),
            "archive-validation errors lacked stable prefix: {unstable_errors:?}"
        );
    }

    #[test]
    fn safe_output_folder_matrix_preserves_core_alias_and_numeric_resume_rules() {
        let dir = tempdir().unwrap();
        let cases = [
            ("truly-empty", "news", false),
            ("exact-zero-post", "news", false),
            ("matching-alias-zero-post", "evo_lutio", false),
            ("canonical-public-post", "news", true),
            ("numeric-alias-public-post", "evo_lutio", true),
        ];

        for (name, target, expected_resume) in cases {
            let root = dir.path().join(name);
            match name {
                "truly-empty" => std::fs::create_dir(&root).unwrap(),
                "exact-zero-post" => write_valid_index(&root, "news"),
                "matching-alias-zero-post" => write_valid_index(&root, "Evo-Lutio"),
                "canonical-public-post" => {
                    write_valid_index(&root, "news");
                    write_public_post(&root, "1.adoc", "= Canonical\n");
                }
                "numeric-alias-public-post" => {
                    write_valid_index(&root, "Evo-Lutio");
                    write_public_post(&root, "0001.adoc", "= Numeric alias\n");
                }
                other => panic!("unhandled safe-folder fixture {other}"),
            }

            let before = snapshot_tree(&root);
            let (_, options) = app_for_folder(target, &root)
                .run_request()
                .unwrap()
                .unwrap();
            assert_eq!(
                options.output_dir.as_deref(),
                Some(root.as_path()),
                "{name}"
            );
            assert_eq!(options.resume, expected_resume, "{name}");
            assert_eq!(
                snapshot_tree(&root),
                before,
                "{name}: validation mutated root"
            );
        }
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
        assert!(error.starts_with(ARCHIVE_VALIDATION_PREFIX));
        assert!(error.contains("missing controlled index"));
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
        assert!(error.starts_with(ARCHIVE_VALIDATION_PREFIX));
        assert!(error.contains("could not read selected directory"));
    }

    #[test]
    fn posts_file_is_rejected_when_inspection_fails() {
        let dir = tempdir().unwrap();
        write_valid_index(dir.path(), "news");
        std::fs::write(dir.path().join("posts"), "not a directory\n").unwrap();

        let mut app = App::default();
        app.update_state(&Message::TargetChanged("news".to_string()));
        app.update_state(&Message::FolderChosen(Some(dir.path().to_path_buf())));

        let error = app.run_request().unwrap_err();
        assert!(error.starts_with(ARCHIVE_VALIDATION_PREFIX));
        assert!(error.contains("posts path is not a directory"));
    }
}
