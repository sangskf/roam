use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, connect_async_tls_with_config, Connector, tungstenite::protocol::Message as WsMessage};
use url::Url;
use uuid::Uuid;
use std::time::Duration;
use tokio::time;
use tracing::{info, error, warn};
use std::fs;
// use std::path::Path;
use std::sync::Arc;
use rustls::client::danger::{ServerCertVerifier, ServerCertVerified, HandshakeSignatureValid};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::DigitallySignedStruct;

use common::Message;
use crate::config::ClientConfig;
use crate::command_handler;

pub async fn run(shutdown_signal: impl std::future::Future<Output = ()>) -> anyhow::Result<()> {
    // Install default crypto provider if not already installed
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Load .env file
    // 1. Prioritize loading from the directory of the executable (Service/Production behavior)
    let mut env_loaded = false;
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let env_path = exe_dir.join(".env");
            if env_path.exists() {
                 if let Err(e) = dotenvy::from_path(&env_path) {
                     warn!("Failed to load .env from executable directory: {}", e);
                 } else {
                     env_loaded = true;
                 }
            }
        }
    }
    
    // 2. Fallback to current directory (Development behavior) if not loaded from exe dir
    if !env_loaded {
        if let Err(e) = dotenvy::dotenv() {
            if !e.not_found() {
                warn!("Failed to load .env from current directory: {}", e);
            }
        }
    }
    
    // 3. Additional Development fallback (client/.env)
    if !env_loaded {
        let _ = dotenvy::from_filename("client/.env");
    }

    // Initialize tracing if not already initialized
    // Note: tracing subscriber should ideally be init in main, but here is also ok if single entry
    // However, for service, we might want different logging?
    // Let's assume main handles tracing init.

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

    tokio::select! {
        _ = async {
            loop {
                match connect_and_run(client_id, &hostname, &os, &version, &config).await {
                    Ok(_) => warn!("Connection closed, reconnecting..."),
                    Err(e) => error!("Connection error: {}, reconnecting in 5s...", e),
                }
                time::sleep(Duration::from_secs(5)).await;
            }
        } => {}
        _ = shutdown_signal => {
            info!("Shutdown signal received, exiting...");
        }
    }
    
    Ok(())
}

fn get_or_create_client_id() -> anyhow::Result<Uuid> {
    // Use executable directory for storage to ensure it works in Service mode
    let mut path = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Failed to get executable directory"))?
        .to_path_buf();
    path.push(".client_id");

    if path.exists() {
        let content = fs::read_to_string(&path)?;
        if let Ok(uuid) = Uuid::parse_str(content.trim()) {
            return Ok(uuid);
        }
    }
    
    let new_uuid = Uuid::new_v4();
    fs::write(&path, new_uuid.to_string())?;
    Ok(new_uuid)
}

async fn connect_and_run(client_id: Uuid, hostname: &str, os: &str, version: &str, config: &ClientConfig) -> anyhow::Result<()> {
    let url = Url::parse(&config.server_url)?;
    
    let (ws_stream, _) = if config.tls_insecure {
        info!("Connecting to server (insecure mode)...");
        let tls_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
            .with_no_client_auth();
            
        let connector = Connector::Rustls(Arc::new(tls_config));
        connect_async_tls_with_config(url.to_string(), None, false, Some(connector)).await?
    } else {
        info!("Connecting to server...");
        connect_async(url.to_string()).await?
    };
    
    info!("Connected to server at {}", config.server_url);

    let (mut write, mut read) = ws_stream.split();

    // 1. Register
    let mut ips = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if !iface.is_loopback() {
                ips.push(iface.addr.ip().to_string());
            }
        }
    }

    let register_msg = Message::Register {
        client_id,
        token: config.auth_token.clone(),
        hostname: hostname.to_string(),
        os: os.to_string(),
        alias: config.alias.clone(),
        version: version.to_string(),
        ips,
        started_at: Some(get_now()),
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
    let heartbeat_task = {
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
                                 let result = command_handler::handle_command(cmd, config.tls_insecure).await;
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

#[derive(Debug)]
struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

#[cfg(target_os = "windows")]
fn get_now() -> chrono::DateTime<chrono::Utc> {
    use std::sync::Once;
    static START: Once = Once::new();
    static mut HAS_PRECISE_TIME: bool = false;

    unsafe {
        START.call_once(|| {
            use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
            
            // Check if GetSystemTimePreciseAsFileTime exists in kernel32.dll
            let kernel32 = GetModuleHandleA(b"kernel32.dll\0".as_ptr());
            if kernel32 != 0 {
                let proc = GetProcAddress(kernel32, b"GetSystemTimePreciseAsFileTime\0".as_ptr());
                if proc.is_some() {
                    HAS_PRECISE_TIME = true;
                }
            }
        });

        if HAS_PRECISE_TIME {
            chrono::Utc::now()
        } else {
            // Fallback for Windows 7 and older: use GetSystemTimeAsFileTime
            use windows_sys::Win32::System::Time::GetSystemTimeAsFileTime;
            use windows_sys::Win32::Foundation::FILETIME;
            
            let mut ft: FILETIME = std::mem::zeroed();
            GetSystemTimeAsFileTime(&mut ft);
            
            // FILETIME is 100ns intervals since Jan 1, 1601 UTC
            let ticks = ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64);
            
            // Unix epoch (1970-01-01) is 11644473600 seconds after 1601-01-01
            // 11644473600 * 10,000,000 = 116444736000000000 ticks
            const UNIX_EPOCH_TICKS: u64 = 116444736000000000;
            
            let (seconds, nanos) = if ticks >= UNIX_EPOCH_TICKS {
                let diff = ticks - UNIX_EPOCH_TICKS;
                ((diff / 10_000_000) as i64, ((diff % 10_000_000) * 100) as u32)
            } else {
                // Before 1970, fallback to epoch
                (0, 0)
            };
            
            chrono::DateTime::from_timestamp(seconds, nanos).unwrap_or_default()
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn get_now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}
