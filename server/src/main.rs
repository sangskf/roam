mod db;
mod handlers;
mod state;
mod config;

use axum::{routing::{get, post}, Router};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::state::AppState;
use crate::config::ServerConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
        .route("/ws", get(handlers::ws_handler))
        .nest_service("/", ServeDir::new("server/web"))
        .with_state(app_state);

    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    tracing::info!("listening on {}", addr);
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;

    Ok(())
}
