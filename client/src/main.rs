pub mod command_handler;
pub mod config;
pub mod service;
pub mod app;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "roam-client")]
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
    // Note: For Windows service, stdout might not be visible.
    // Ideally we should log to a file or Event Log.
    // But for now, we keep standard initialization.
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    tracing_subscriber::fmt::init();

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
