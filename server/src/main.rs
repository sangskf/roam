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
    GenCert {
        #[arg(short, long, value_delimiter = ',')]
        san: Vec<String>,
        #[arg(short, long, default_value = "cert.pem")]
        cert_out: String,
        #[arg(short, long, default_value = "key.pem")]
        key_out: String,
    },
}

fn generate_cert(san: Vec<String>, cert_out: String, key_out: String) -> anyhow::Result<()> {
    let mut subject_alt_names = san;
    if subject_alt_names.is_empty() {
        subject_alt_names = vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "0.0.0.0".to_string(),
            "::1".to_string(),
        ];
    }
    
    println!("Generating certificate for SANs: {:?}", subject_alt_names);

    let cert = rcgen::generate_simple_self_signed(subject_alt_names)?;
    let pem_serialized = cert.cert.pem();
    let key_serialized = cert.signing_key.serialize_pem();
    std::fs::write(&cert_out, pem_serialized)?;
    std::fs::write(&key_out, key_serialized)?;
    
    println!("Certificate generated: {}", cert_out);
    println!("Private key generated: {}", key_out);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    // Install default crypto provider if possible (ignore if already installed)
    let _ = rustls::crypto::ring::default_provider().install_default();

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
        Some(Commands::GenCert { san, cert_out, key_out }) => return generate_cert(san, cert_out, key_out),
        None => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(app::run())
        }
    }
}
