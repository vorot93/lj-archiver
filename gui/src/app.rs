use crate::state::{App, Message, Phase, State};
use iced::{
    Element, Task,
    widget::{
        button, checkbox, column, container, progress_bar, row, scrollable, text, text_input,
    },
};
use lj_archiver::{
    progress::{Progress, ProgressEvent},
    run::{RunOutcome, run_archive},
};

fn progress_message(run_id: u64, event: ProgressEvent) -> Message {
    Message::Progress { run_id, event }
}

fn finished_message(run_id: u64, result: Result<RunOutcome, String>) -> Message {
    Message::Finished { run_id, result }
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
            Message::OpenFolder => {
                if let State::Done { root, .. } = &self.state {
                    let _ = open::that(root);
                }
                Task::none()
            }
            Message::OpenUrl(url) => {
                let _ = open::that(url);
                Task::none()
            }
            other => {
                self.update_state(&other);
                Task::none()
            }
        }
    }

    fn start_archive(&mut self) -> Task<Message> {
        let (target, options) = match self.run_request() {
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
            .expect("validated Start should activate a run generation");

        let (tx, rx) = futures::channel::mpsc::channel::<ProgressEvent>(128);
        let progress = Progress::channel(tx);
        let job = async move {
            run_archive(&target, &options, &progress)
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
            State::Running(Phase::Discovering { found }) => {
                text!("Discovering posts… ({found} found)").into()
            }
            State::Running(Phase::Archiving { done, total }) => column![
                text!("Archiving {done}/{total}…"),
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
                col.into()
            }
            State::Error { message } => text!("Error: {message}").into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn callback_constructors_tag_messages_with_captured_active_generation() {
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
    }

    #[test]
    fn valid_start_activates_generation_before_scheduling_callbacks() {
        let dir = tempdir().unwrap();
        let mut app = App::default();
        app.update_state(&Message::TargetChanged("news".to_string()));
        app.update_state(&Message::FolderChosen(Some(dir.path().to_path_buf())));

        let task = app.update(Message::Start);

        assert_eq!(app.active_run_id(), Some(1));
        assert!(matches!(app.state, State::Running(_)));
        assert_eq!(task.units(), 2);
        drop(task);
    }

    #[test]
    fn start_routes_invalid_folder_to_error_without_launch() {
        let dir = tempdir().unwrap();
        let output = dir.path().join("output");
        std::fs::write(&output, "not a directory\n").unwrap();

        let mut app = App::default();
        app.update_state(&Message::TargetChanged("news".to_string()));
        app.update_state(&Message::FolderChosen(Some(output)));

        let task = app.update(Message::Start);
        assert_eq!(
            task.units(),
            0,
            "invalid GUI starts must not schedule archive work"
        );
        drop(task);

        assert!(!matches!(app.state, State::Running(_)));
        let State::Error { message } = &app.state else {
            panic!("invalid output folder should enter the error state");
        };
        assert!(message.starts_with("selected folder failed archive validation: "));
        assert!(message.contains("could not read selected directory"));
        assert!(
            !message.contains("posts folder"),
            "selected-path errors must not claim that a posts folder was inspected: {message}"
        );
        assert_eq!(app.active_run_id(), None);
        app.update_state(&Message::Start);
        assert_eq!(app.active_run_id(), Some(1));
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
