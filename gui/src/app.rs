use iced::{
    Element, Task,
    widget::{
        button, checkbox, column, container, progress_bar, row, scrollable, text, text_input,
    },
};
use lj_archiver::{
    progress::{Progress, ProgressEvent},
    run::run_archive,
};

use crate::state::{App, Message, Phase, State};

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

        let (tx, rx) = futures::channel::mpsc::unbounded::<ProgressEvent>();
        let progress = Progress::channel(tx);
        let job = async move {
            run_archive(&target, &options, &progress)
                .await
                .map_err(|error| error.to_string())
        };

        Task::batch([
            Task::run(rx, Message::Progress),
            Task::perform(job, Message::Finished),
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
