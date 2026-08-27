use crate::state::{App, Message, PendingRunValidation, Phase, State, ValidatedRun};
use iced::{
    Element, Task,
    widget::{
        button, checkbox, column, container, progress_bar, row, scrollable, text, text_input,
    },
};
use lj_archiver::{
    progress::{Progress, ProgressEvent},
    run::{RunOutcome, RunWarning, run_archive},
};

fn running_status_text(phase: &Phase) -> String {
    match phase {
        Phase::Validating => "Validating archive folder…".to_string(),
        Phase::Discovering { found } => format!("Discovering posts… ({found} found)"),
        Phase::FetchingPost => "Fetching post…".to_string(),
        Phase::Archiving { done, total } => format!("Archiving {done}/{total}…"),
    }
}

fn completion_warning_lines(warnings: &[RunWarning]) -> Vec<String> {
    warnings.iter().map(ToString::to_string).collect()
}

fn progress_message(run_id: u64, event: ProgressEvent) -> Message {
    Message::Progress { run_id, event }
}

fn finished_message(run_id: u64, result: Result<RunOutcome, String>) -> Message {
    Message::Finished { run_id, result }
}

fn validation_message(run_id: u64, result: Result<ValidatedRun, String>) -> Message {
    Message::ValidationFinished { run_id, result }
}

fn launch_message(launch_id: u64, action: &'static str, result: Result<(), String>) -> Message {
    Message::LaunchFinished {
        launch_id,
        result: result.map_err(|error| format!("{action} failed: {error}")),
    }
}

fn launch_task<T>(launch_id: u64, action: &'static str, target: T) -> Task<Message>
where
    T: AsRef<std::ffi::OsStr> + Send + 'static,
{
    Task::perform(
        async move { open::that(target).map_err(|error| error.to_string()) },
        move |result| launch_message(launch_id, action, result),
    )
}

fn validation_task(run_id: u64, pending: PendingRunValidation) -> Task<Message> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(App::validate_pending(pending));
    });
    Task::perform(
        async move {
            rx.await
                .map_err(|_| "archive validation worker stopped unexpectedly".to_string())?
        },
        move |result| validation_message(run_id, result),
    )
}

impl App {
    pub fn boot() -> (App, Task<Message>) {
        (App::default(), Task::none())
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ChooseFolder => {
                Task::perform(rfd::AsyncFileDialog::new().pick_folder(), |handle| {
                    Message::FolderChosen(handle.map(|h| h.path().to_path_buf()))
                })
            }
            Message::Start => self.start_archive(),
            Message::ValidationFinished { run_id, result } => {
                self.finish_validation(run_id, result)
            }
            Message::OpenFolder => {
                let Some(root) = (match &self.state {
                    State::Done { root, .. } => Some(root.clone()),
                    _ => None,
                }) else {
                    return Task::none();
                };
                let launch_id = self.begin_launch();
                launch_task(launch_id, "Open folder", root)
            }
            Message::OpenUrl(url) => {
                let launch_id = self.begin_launch();
                launch_task(launch_id, "Open link", url)
            }
            other => {
                self.update_state(&other);
                Task::none()
            }
        }
    }

    fn start_archive(&mut self) -> Task<Message> {
        let pending = match self.pending_validation() {
            Ok(Some(request)) => request,
            Ok(None) => return Task::none(),
            Err(message) => {
                self.state = State::Error { message };
                return Task::none();
            }
        };
        self.update_state(&Message::Start);
        let run_id = self
            .active_run_id()
            .expect("startable Start should activate a run generation");

        validation_task(run_id, pending)
    }

    fn finish_validation(
        &mut self,
        run_id: u64,
        result: Result<ValidatedRun, String>,
    ) -> Task<Message> {
        let accepts_validation = self.active_run_id() == Some(run_id)
            && matches!(self.state, State::Running(Phase::Validating));
        self.update_state(&Message::ValidationFinished {
            run_id,
            result: result.clone(),
        });
        if !accepts_validation {
            return Task::none();
        }
        let Ok(validated) = result else {
            return Task::none();
        };

        let (tx, rx) = futures::channel::mpsc::channel::<ProgressEvent>(128);
        let progress = Progress::channel(tx);
        let job = async move {
            run_archive(&validated.target, &validated.options, &progress)
                .await
                .map_err(|error| error.to_string())
        };

        Task::batch([
            Task::run(rx, move |event| progress_message(run_id, event)),
            Task::perform(job, move |result| finished_message(run_id, result)),
        ])
    }

    pub fn view(&self) -> Element<'_, Message> {
        let form = column![
            text("Journal or post to archive"),
            text_input("news, news/166511, or a post URL", &self.config.target)
                .on_input(Message::TargetChanged),
            row![
                button("Choose folder…").on_press(Message::ChooseFolder),
                text(match &self.config.folder {
                    Some(path) => path.display().to_string(),
                    None => "No folder chosen".to_string(),
                }),
            ]
            .spacing(10),
            checkbox(self.config.include_comments)
                .label("Include comments")
                .on_toggle(Message::ToggleComments),
            checkbox(self.config.download_media)
                .label("Download media")
                .on_toggle(Message::ToggleMedia),
            checkbox(self.config.generate_book)
                .label("Generate book.adoc")
                .on_toggle(Message::ToggleBook),
            self.start_button(),
            self.status_view(),
            self.launch_notice_view(),
        ]
        .spacing(12);

        container(form).padding(20).into()
    }

    fn start_button(&self) -> Element<'_, Message> {
        let mut b = button("Start");
        if self.can_start() {
            b = b.on_press(Message::Start);
        }
        b.into()
    }

    fn status_view(&self) -> Element<'_, Message> {
        match &self.state {
            State::Idle => text("").into(),
            State::Running(
                phase @ (Phase::Validating | Phase::Discovering { .. } | Phase::FetchingPost),
            ) => text(running_status_text(phase)).into(),
            State::Running(phase @ Phase::Archiving { done, total }) => column![
                text(running_status_text(phase)),
                progress_bar(0.0..=(*total as f32).max(1.0), *done as f32),
            ]
            .spacing(6)
            .into(),
            State::Done {
                archived,
                skipped,
                failed,
                failed_pages,
                failures,
                warnings,
                ..
            } => {
                let mut col = column![
                    text!("Done: {archived} archived, {skipped} skipped, {failed} failed"),
                    button("Open folder").on_press(Message::OpenFolder),
                ]
                .spacing(10);

                if *failed_pages > 0 {
                    col = col.push(text!(
                        "{failed_pages} archive page(s) failed; the archive may be incomplete"
                    ));
                }

                if !failures.is_empty() {
                    let mut list = column![text("Failed posts (click to open):")].spacing(4);
                    for failure in failures {
                        let url = failure.url.to_string();
                        list = list.push(
                            button(text(url.clone()))
                                .on_press(Message::OpenUrl(url))
                                .style(button::text),
                        );
                    }
                    col = col.push(scrollable(list).height(200.0));
                }
                if !warnings.is_empty() {
                    let mut list = column![text("Warnings:")].spacing(4);
                    for warning in completion_warning_lines(warnings) {
                        list = list.push(text(warning));
                    }
                    col = col.push(scrollable(list).height(200.0));
                }
                col.into()
            }
            State::Error { message } => text!("Error: {message}").into(),
        }
    }

    fn launch_notice_view(&self) -> Element<'_, Message> {
        match &self.launch_notice {
            Some(message) => text(message).into(),
            None => text("").into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lj_archiver::models::RunOptions;
    use std::path::PathBuf;
    use tempfile::tempdir;
    use url::Url;

    fn validated_run(target: &str, output_dir: PathBuf) -> ValidatedRun {
        ValidatedRun {
            target: target.to_string(),
            options: RunOptions {
                output_dir: Some(output_dir),
                include_comments: true,
                download_media: false,
                concurrency: 4,
                resume: false,
                generate_book: false,
                journal_context: None,
                user_agent: None,
                base_url: Url::parse("https://livejournal.com").unwrap(),
            },
        }
    }

    #[test]
    fn callback_constructors_tag_messages_with_captured_generations() {
        let mut app = App::default();
        app.update_state(&Message::Start);
        let run_id = app.active_run_id().unwrap();

        let progress = progress_message(
            run_id,
            ProgressEvent::DiscoveryTick {
                label: "/2024/".to_string(),
                found: 4,
            },
        );
        assert!(matches!(
            progress,
            Message::Progress {
                run_id: tagged_id,
                event: ProgressEvent::DiscoveryTick { found: 4, .. },
            } if tagged_id == run_id
        ));

        let finished = finished_message(run_id, Err("terminal".to_string()));
        assert!(matches!(
            finished,
            Message::Finished {
                run_id: tagged_id,
                result: Err(message),
            } if tagged_id == run_id && message == "terminal"
        ));

        let validation =
            validation_message(run_id, Ok(validated_run("news", PathBuf::from("/tmp/out"))));
        assert!(matches!(
            validation,
            Message::ValidationFinished {
                run_id: tagged_id,
                result: Ok(ValidatedRun { target, .. }),
            } if tagged_id == run_id && target == "news"
        ));

        let launch_id = app.begin_launch();
        let launch = launch_message(
            launch_id,
            "Open folder",
            Err("permission denied".to_string()),
        );
        assert!(matches!(
            launch,
            Message::LaunchFinished {
                launch_id: tagged_id,
                result: Err(message),
            } if tagged_id == launch_id && message == "Open folder failed: permission denied"
        ));
    }

    #[test]
    fn valid_start_activates_generation_before_scheduling_callbacks() {
        let dir = tempdir().unwrap();
        let mut app = App::default();
        app.update_state(&Message::TargetChanged("news".to_string()));
        app.update_state(&Message::FolderChosen(Some(dir.path().to_path_buf())));

        let task = app.update(Message::Start);

        assert_eq!(app.active_run_id(), Some(1));
        assert!(matches!(app.state, State::Running(Phase::Validating)));
        assert_eq!(task.units(), 1);
        drop(task);
    }

    #[test]
    fn matching_successful_validation_schedules_archive_work_only_once() {
        let mut app = App::default();
        app.update_state(&Message::Start);
        let run_id = app.active_run_id().unwrap();
        let result = Ok(validated_run("news", PathBuf::from("/tmp/out")));

        let first = app.update(Message::ValidationFinished {
            run_id,
            result: result.clone(),
        });
        assert_eq!(first.units(), 2);
        drop(first);
        assert_eq!(app.active_run_id(), Some(run_id));
        assert!(matches!(
            app.state,
            State::Running(Phase::Discovering { found: 0 })
        ));

        let duplicate = app.update(Message::ValidationFinished { run_id, result });
        assert_eq!(duplicate.units(), 0);
        drop(duplicate);
        assert_eq!(app.active_run_id(), Some(run_id));
    }

    #[test]
    fn stale_validation_completion_schedules_no_archive_work() {
        let mut app = App::default();
        app.update_state(&Message::Start);
        let run_id = app.active_run_id().unwrap();
        let before = format!("{app:?}");

        let task = app.update(Message::ValidationFinished {
            run_id: run_id.wrapping_add(1),
            result: Ok(validated_run("news", PathBuf::from("/tmp/out"))),
        });

        assert_eq!(task.units(), 0);
        drop(task);
        assert_eq!(format!("{app:?}"), before);
    }

    #[test]
    fn fetching_phase_has_a_distinct_rendered_status() {
        assert_eq!(running_status_text(&Phase::FetchingPost), "Fetching post…");
    }

    #[test]
    fn validating_phase_has_a_distinct_rendered_status() {
        assert_eq!(
            running_status_text(&Phase::Validating),
            "Validating archive folder…"
        );
    }

    #[test]
    fn completion_warning_lines_render_typed_terminal_warnings() {
        let warning = lj_archiver::run::RunWarning::PendingCleanupFailed {
            post_id: 8,
            message: "permission denied".to_string(),
        };

        assert_eq!(
            completion_warning_lines(&[warning]),
            ["post 8: finalized pending file could not be removed: permission denied"]
        );
    }

    #[test]
    fn matching_validation_error_schedules_no_archive_work() {
        let mut app = App::default();
        app.update_state(&Message::Start);
        let run_id = app.active_run_id().unwrap();

        let task = app.update(Message::ValidationFinished {
            run_id,
            result: Err(
                "selected folder failed archive validation: could not read selected directory"
                    .to_string(),
            ),
        });
        assert_eq!(
            task.units(),
            0,
            "invalid GUI validation must not schedule archive work"
        );
        drop(task);

        assert!(!matches!(app.state, State::Running(_)));
        let State::Error { message } = &app.state else {
            panic!("invalid output folder should enter the error state");
        };
        assert!(message.starts_with("selected folder failed archive validation: "));
        assert!(message.contains("could not read selected directory"));
        assert_eq!(app.active_run_id(), None);
        app.update_state(&Message::Start);
        assert_eq!(app.active_run_id(), Some(2));
    }

    #[test]
    fn open_folder_allocates_launch_generation_and_schedules_result() {
        let mut app = App::default();
        app.state = State::Done {
            root: PathBuf::from("/tmp/out"),
            archived: 1,
            skipped: 0,
            failed: 0,
            failed_pages: 0,
            failures: Vec::new(),
            warnings: Vec::new(),
        };

        let task = app.update(Message::OpenFolder);

        assert_eq!(task.units(), 1);
        drop(task);
        assert_eq!(app.active_launch_id(), Some(1));
        assert_eq!(app.active_run_id(), None);
    }

    #[test]
    fn invalid_target_schedules_no_work_and_does_not_advance_generation() {
        let dir = tempdir().unwrap();
        let mut app = App::default();
        app.update_state(&Message::TargetChanged(
            "https://example.com/not-livejournal".to_string(),
        ));
        app.update_state(&Message::FolderChosen(Some(dir.path().to_path_buf())));
        let task = app.update(Message::Start);

        assert_eq!(task.units(), 0);
        drop(task);
        assert_eq!(app.active_run_id(), None);
        assert!(matches!(app.state, State::Error { .. }));
        app.update_state(&Message::Start);
        assert_eq!(app.active_run_id(), Some(1));
    }

    #[test]
    fn start_while_running_schedules_no_work_and_does_not_advance_generation() {
        let dir = tempdir().unwrap();
        let mut app = App::default();
        app.update_state(&Message::TargetChanged("news".to_string()));
        app.update_state(&Message::FolderChosen(Some(dir.path().to_path_buf())));
        app.update_state(&Message::Start);
        let run_id = app.active_run_id().unwrap();

        let task = app.update(Message::Start);

        assert_eq!(task.units(), 0);
        drop(task);
        assert_eq!(app.active_run_id(), Some(run_id));
        assert!(matches!(app.state, State::Running(_)));
        app.update_state(&Message::Finished {
            run_id,
            result: Err("finished".to_string()),
        });
        app.update_state(&Message::Start);
        assert_eq!(app.active_run_id(), Some(2));
    }
}
