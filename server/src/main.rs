pub mod db;
pub mod handlers;
pub mod state;
pub mod config;
pub mod service;
pub mod assets;
pub mod app;

use clap::{Parser, Subcommand};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "roam-server")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Install,
    Uninstall,
    Start,
    Stop,
    #[cfg(windows)]
    RunService,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Initialize tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "server=debug,tower_http=debug".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    match cli.command {
        Some(Commands::Install) => return service::install_service(),
        Some(Commands::Uninstall) => return service::uninstall_service(),
        Some(Commands::Start) => return service::start_service(),
        Some(Commands::Stop) => return service::stop_service(),
        #[cfg(windows)]
        Some(Commands::RunService) => return service::run_windows_service(),
        None => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(app::run())
        }
    }
}
