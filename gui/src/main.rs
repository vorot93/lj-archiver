#![windows_subsystem = "windows"]

mod app;
mod state;

use crate::state::App;

fn main() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title("LJ Archiver")
        .window_size((520.0, 640.0))
        .run()
}
