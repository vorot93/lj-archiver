use clap::Parser;
use lj_archiver::cli::Cli;

#[tokio::main]
async fn main() -> lj_archiver::Result<()> {
    let cli = Cli::parse();
    lj_archiver::run::run(cli).await
}
