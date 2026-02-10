mod db;
mod handlers;
mod state;
mod config;
mod service;
mod assets;

use axum::{routing::{get, post}, Router, extract::DefaultBodyLimit};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use clap::{Parser, Subcommand};

use crate::state::AppState;
use crate::config::ServerConfig;
use uuid::Uuid;

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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Install) => return service::install_service(),
        Some(Commands::Uninstall) => return service::uninstall_service(),
        Some(Commands::Start) => return service::start_service(),
        Some(Commands::Stop) => return service::stop_service(),
        None => {}
    }

    // Load .env file
    // 1. Try loading from current directory (standard behavior)
    dotenvy::dotenv().ok();
    
    // 2. Try loading from the directory of the executable (service behavior fallback)
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let env_path = exe_dir.join(".env");
            if env_path.exists() {
                 let _ = dotenvy::from_path(&env_path);
            }
        }
    }
    
    // 3. Development fallback
    let _ = dotenvy::from_filename("server/.env");

    // Initialize tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "server=debug,tower_http=debug".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load Config
    let config = ServerConfig::new()?;
    tracing::info!("Loaded config: {:?}", config);

    // Initialize Database
    let pool = db::init_db(&config.database_url).await?;

    // App State
    let app_state = Arc::new(AppState::new(pool, config.clone()));

    // Router
    let app = Router::new()
        .route("/api/clients", get(handlers::list_clients))
        .route("/api/clients/:id/command", post(handlers::send_command))
        .route("/api/commands/:id/result", get(handlers::get_command_result))
        .route("/api/files/admin-upload", post(handlers::upload_file_admin))
        .route("/api/files/client-upload/:id", post(handlers::upload_file_client))
        .nest_service("/api/files/download", ServeDir::new("server/uploads"))
        .route("/api/scripts", get(handlers::list_scripts).post(handlers::create_script))
        .route("/api/scripts/:id", axum::routing::put(handlers::update_script).delete(handlers::delete_script))
        .route("/api/scripts/:id/run", post(handlers::run_script))
        .route("/api/history", get(handlers::get_script_history).delete(handlers::clear_script_history))
        .route("/ws", get(handlers::ws_handler))
        .fallback(assets::static_handler)
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024 * 2)) // 2GB
        .with_state(app_state);

    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    tracing::info!("listening on {}", addr);
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;

    Ok(())
}
