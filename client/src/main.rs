mod command_handler;
mod config;
mod service;

use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as WsMessage};
use url::Url;
use uuid::Uuid;
use std::time::Duration;
use tokio::time;
use tracing::{info, error, warn};
use std::fs;
use std::path::Path;

use common::Message;
use crate::config::ClientConfig;

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
    let _ = dotenvy::from_filename("client/.env");

    tracing_subscriber::fmt::init();

    // Load Config
    let config = ClientConfig::new()?;
    info!("Loaded config: {:?}", config);

    let client_id = get_or_create_client_id()?;
    let hostname = hostname::get().unwrap().to_string_lossy().to_string();
    let os = std::env::consts::OS.to_string();
    let version = env!("CARGO_PKG_VERSION").to_string();

    info!("Starting client: {} ({}) - {} - v{}", client_id, hostname, os, version);
    if let Some(alias) = &config.alias {
        info!("Client alias: {}", alias);
    }

    loop {
        match connect_and_run(client_id, &hostname, &os, &version, &config).await {
            Ok(_) => warn!("Connection closed, reconnecting..."),
            Err(e) => error!("Connection error: {}, reconnecting in 5s...", e),
        }
        time::sleep(Duration::from_secs(5)).await;
    }
}

fn get_or_create_client_id() -> anyhow::Result<Uuid> {
    let path = Path::new(".client_id");
    if path.exists() {
        let content = fs::read_to_string(path)?;
        if let Ok(uuid) = Uuid::parse_str(content.trim()) {
            return Ok(uuid);
        }
    }
    
    let new_uuid = Uuid::new_v4();
    fs::write(path, new_uuid.to_string())?;
    Ok(new_uuid)
}

async fn connect_and_run(client_id: Uuid, hostname: &str, os: &str, version: &str, config: &ClientConfig) -> anyhow::Result<()> {
    let url = Url::parse(&config.server_url)?;
    let (ws_stream, _) = connect_async(url.to_string()).await?;
    info!("Connected to server at {}", config.server_url);

    let (mut write, mut read) = ws_stream.split();

    // 1. Register
    let register_msg = Message::Register {
        client_id,
        token: config.auth_token.clone(),
        hostname: hostname.to_string(),
        os: os.to_string(),
        alias: config.alias.clone(),
        version: version.to_string(),
    };
    write.send(WsMessage::Text(serde_json::to_string(&register_msg)?)).await?;

    // 2. Wait for AuthSuccess
    if let Some(msg) = read.next().await {
        let msg = msg?;
        if let WsMessage::Text(text) = msg {
            let parsed: Message = serde_json::from_str(&text)?;
            match parsed {
                Message::AuthSuccess => info!("Authentication successful"),
                Message::AuthFailed(reason) => return Err(anyhow::anyhow!("Auth failed: {}", reason)),
                _ => return Err(anyhow::anyhow!("Unexpected response during auth")),
            }
        } else {
            return Err(anyhow::anyhow!("Unexpected message type during auth"));
        }
    } else {
        return Err(anyhow::anyhow!("Connection closed during auth"));
    }

    // 3. Main Loop (Heartbeat + Command Handling)
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(100);

    // Heartbeat Task
    let mut heartbeat_task = {
        let interval = config.heartbeat_interval_sec;
        let tx = tx.clone();
        tokio::spawn(async move {
            loop {
                time::sleep(Duration::from_secs(interval)).await;
                if tx.send(Message::Heartbeat).await.is_err() {
                    break;
                }
            }
        })
    };

    // Handle Incoming Commands
    loop {
        tokio::select! {
            // Send outgoing messages (Heartbeat, Responses)
            Some(msg) = rx.recv() => {
                let json = serde_json::to_string(&msg)?;
                write.send(WsMessage::Text(json)).await?;
            }
            // Receive incoming messages
            Some(msg) = read.next() => {
                let msg = msg?;
                match msg {
                    WsMessage::Text(text) => {
                         let parsed: Message = serde_json::from_str(&text)?;
                         match parsed {
                             Message::Command { id, cmd } => {
                                 info!("Received command: {:?}", cmd);
                                 let result = command_handler::handle_command(cmd).await;
                                 info!("Command execution finished. Result: {:?}", result);
                                 let response = Message::Response { id, result };
                                 let json = serde_json::to_string(&response)?;
                                 write.send(WsMessage::Text(json)).await?;
                             }
                             _ => {}
                         }
                    }
                    WsMessage::Close(_) => return Ok(()),
                    _ => {}
                }
            }
            else => break,
        }
    }
    
    heartbeat_task.abort();
    Ok(())
}
