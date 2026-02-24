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
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }

    let _guard = {
        #[cfg(windows)]
        if let Some(Commands::RunService) = &cli.command {
            // Log to file when running as service
            if let Ok(exe_path) = std::env::current_exe() {
                if let Some(exe_dir) = exe_path.parent() {
                    let file_appender = tracing_appender::rolling::daily(exe_dir, "roam-client.log");
                    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
                    
                    tracing_subscriber::fmt()
                        .with_writer(non_blocking)
                        .with_ansi(false)
                        .init();
                    Some(guard)
                } else {
                    tracing_subscriber::fmt::init();
                    None
                }
            } else {
                tracing_subscriber::fmt::init();
                None
            }
        } else {
            tracing_subscriber::fmt::init();
            None
        }

        #[cfg(not(windows))]
        {
            tracing_subscriber::fmt::init();
            None::<()> // No guard needed for stdout
        }
    };

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
