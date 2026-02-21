use axum::{
    routing::{get, post},
    Router,
    extract::{DefaultBodyLimit, State, Request},
    middleware::{self, Next},
    response::{Response, IntoResponse},
    http::StatusCode,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;

use crate::state::AppState;
use crate::config::ServerConfig;
use crate::db;
use crate::handlers;
use crate::assets;

pub async fn run() -> anyhow::Result<()> {
    // Load .env file
    dotenvy::dotenv().ok();
    
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let env_path = exe_dir.join(".env");
            if env_path.exists() {
                 let _ = dotenvy::from_path(&env_path);
            }
        }
    }
    
    let _ = dotenvy::from_filename("server/.env");

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
        .route("/api/clients/:id", axum::routing::delete(handlers::delete_client))
        .route("/api/info", get(handlers::get_server_info))
        .route("/api/clients/:id/command", post(handlers::send_command))
        .route("/api/commands/:id/result", get(handlers::get_command_result))
        .route("/api/files/admin-upload", post(handlers::upload_file_admin))
        .route("/api/files/client-upload/:id", post(handlers::upload_file_client))
        .nest_service("/api/files/download", ServeDir::new("uploads"))
        .route("/api/groups", get(handlers::list_groups).post(handlers::create_group))
        .route("/api/groups/:id", axum::routing::delete(handlers::delete_group).put(handlers::update_group))
        .route("/api/groups/:id/run", post(handlers::run_group_scripts))
        .route("/api/executions/active", get(handlers::get_active_executions))
        .route("/api/scripts", get(handlers::list_scripts).post(handlers::create_script))
        .route("/api/scripts/:id", axum::routing::put(handlers::update_script).delete(handlers::delete_script))
        .route("/api/scripts/:id/run", post(handlers::run_script))
        .route("/api/updates", get(handlers::list_updates).post(handlers::upload_update))
        .route("/api/updates/:id", axum::routing::delete(handlers::delete_update))
        .route("/api/updates/trigger", post(handlers::trigger_update_clients))
        .route("/api/history", get(handlers::get_script_history).delete(handlers::clear_script_history))
        .route("/ws", get(handlers::ws_handler))
        // Auth Routes
        .route("/api/auth/login", post(handlers::login))
        .route("/api/auth/logout", post(handlers::logout))
        .route("/api/auth/password", post(handlers::change_password))
        .route("/api/auth/status", get(handlers::get_auth_status))
        .fallback(assets::static_handler)
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024 * 2)) // 2GB
        .layer(middleware::from_fn_with_state(app_state.clone(), auth_middleware))
        .with_state(app_state);

    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    tracing::info!("listening on {}", addr);

    if let (Some(cert_path), Some(key_path)) = (&config.tls_cert_path, &config.tls_key_path) {
        tracing::info!("TLS enabled. Cert: {}, Key: {}", cert_path, key_path);
        let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            cert_path,
            key_path,
        )
        .await?;

        axum_server::bind_rustls(addr, tls_config)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await?;
    } else {
        tracing::info!("TLS disabled. Using plain TCP.");
        let listener = TcpListener::bind(addr).await?;
        axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    }

    Ok(())
}

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if !state.config.web_auth_enabled {
        return next.run(request).await;
    }

    let path = request.uri().path().to_string(); // Clone path to avoid borrow issues

    // Allow static assets/fallback (not starting with /api)
    if !path.starts_with("/api") {
        return next.run(request).await;
    }

    // Allow login and status
    if path == "/api/auth/login" || path == "/api/auth/status" {
        return next.run(request).await;
    }
    
    // Allow public API
    // /api/info is public
    if path.starts_with("/api/info") {
         return next.run(request).await;
    }

    // /api/files/download and /api/files/client-upload are public (used by clients)
    // /api/files/admin-upload should be protected
    if path.starts_with("/api/files/download/") || path.starts_with("/api/files/client-upload/") {
         return next.run(request).await;
    }

    let token = request.headers().get("Authorization")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.replace("Bearer ", ""))
        .unwrap_or_default();

    if state.web_sessions.contains_key(&token) {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
    }
}
