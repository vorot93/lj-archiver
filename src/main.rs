use clap::Parser;
use lj_archiver::cli::Cli;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // `Result<(), E>`'s default `Termination` impl prints the error via `Debug`
    // (a raw `Variant { field: .. }` dump), not the human-readable `Display`
    // message every `LjError` variant defines. Print `Display` explicitly so a
    // CLI failure — including `ArchiveBusy` — reads the same intelligible
    // message the GUI shows.
    match lj_archiver::run::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}
