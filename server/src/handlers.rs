use axum::{
    extract::{ws::{Message as WsMessage, WebSocket, WebSocketUpgrade}, State, Json, Path, ConnectInfo, Multipart},
    response::IntoResponse,
    http::{StatusCode, HeaderMap},
};
use futures::{sink::SinkExt, stream::StreamExt};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::fs::File;
use uuid::Uuid;
use tracing::{info, error, warn};
use std::net::SocketAddr;
use sqlx::Row;

use crate::state::{AppState, ClientConnection, ScriptGroup, ScriptStep};
use common::{Message, CommandPayload, CommandResult};

pub async fn index() -> &'static str {
    "Roam Server Running"
}

// API: List Scripts
pub async fn list_scripts(State(state): State<Arc<AppState>>) -> Json<Vec<ScriptGroup>> {
    let rows = sqlx::query!("SELECT id, name, steps FROM scripts ORDER BY created_at DESC")
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

    let scripts = rows.into_iter().map(|r| {
        let steps: Vec<ScriptStep> = serde_json::from_str(&r.steps).unwrap_or_default();
        ScriptGroup {
            id: Uuid::parse_str(r.id.as_deref().unwrap_or("")).unwrap_or_default(),
            name: r.name,
            steps,
        }
    }).collect();
    Json(scripts)
}

// API: Create Script
#[derive(serde::Deserialize)]
pub struct CreateScriptRequest {
    pub name: String,
    pub steps: Vec<ScriptStep>,
}

pub async fn create_script(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateScriptRequest>,
) -> impl IntoResponse {
    let id = Uuid::new_v4();
    let id_str = id.to_string();
    let name = &payload.name;
    let steps_json = serde_json::to_string(&payload.steps).unwrap_or("[]".to_string());
    
    if let Err(e) = sqlx::query!(
        "INSERT INTO scripts (id, name, steps) VALUES (?, ?, ?)",
        id_str, name, steps_json
    ).execute(&state.db).await {
         return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create script: {}", e)).into_response();
    }
    
    (StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response()
}

// API: Update Script
pub async fn update_script(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(payload): Json<CreateScriptRequest>,
) -> impl IntoResponse {
    let id_str = id.to_string();
    let name = &payload.name;
    let steps_json = serde_json::to_string(&payload.steps).unwrap_or("[]".to_string());
    
    if let Err(e) = sqlx::query!(
        "UPDATE scripts SET name = ?, steps = ? WHERE id = ?",
        name, steps_json, id_str
    ).execute(&state.db).await {
         return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to update script: {}", e)).into_response();
    }
    
    (StatusCode::OK, "Script updated").into_response()
}

// API: Delete Script
pub async fn delete_script(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let id_str = id.to_string();
    if let Err(e) = sqlx::query!(
        "DELETE FROM scripts WHERE id = ?",
        id_str
    ).execute(&state.db).await {
         return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete script: {}", e)).into_response();
    }
    
    (StatusCode::OK, "Script deleted").into_response()
}

// API: Run Script on Multiple Clients
#[derive(serde::Deserialize)]
pub struct RunScriptRequest {
    pub client_ids: Vec<Uuid>,
}

pub async fn run_script(
    State(state): State<Arc<AppState>>,
    Path(script_id): Path<Uuid>,
    Json(payload): Json<RunScriptRequest>,
) -> impl IntoResponse {
    let script_id_str = script_id.to_string();
    // Fetch script from DB
    let row = match sqlx::query!("SELECT name, steps FROM scripts WHERE id = ?", script_id_str)
        .fetch_optional(&state.db)
        .await {
            Ok(Some(r)) => r,
            Ok(None) => return (StatusCode::NOT_FOUND, "Script not found").into_response(),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB Error: {}", e)).into_response(),
        };

    let steps: Vec<ScriptStep> = serde_json::from_str(&row.steps).unwrap_or_default();
    let script = ScriptGroup {
        id: script_id,
        name: row.name,
        steps,
    };

    // For each client, create execution history and spawn task
    for client_id in payload.client_ids {
        if !state.clients.contains_key(&client_id) {
            continue; // Skip offline/invalid clients
        }
        
        let history_id = Uuid::new_v4();
        let history_id_str = history_id.to_string();
        let script_id_str_run = script_id.to_string();
        let client_id_str = client_id.to_string();
        
        // Insert history record
        if let Err(e) = sqlx::query!(
            "INSERT INTO execution_history (id, script_id, client_id, status) VALUES (?, ?, ?, ?)",
            history_id_str, script_id_str_run, client_id_str, "running"
        ).execute(&state.db).await {
            error!("Failed to create history record: {}", e);
            continue;
        }

        let state_clone = state.clone();
        let script_clone = script.clone();
        tokio::spawn(async move {
            run_script_task(state_clone, client_id, script_clone, history_id).await;
        });
    }

    (StatusCode::OK, "Script execution started on selected clients").into_response()
}

async fn run_script_task(state: Arc<AppState>, client_id: Uuid, script: ScriptGroup, history_id: Uuid) {
    info!("Starting script {} on client {}", script.name, client_id);
    let mut logs = Vec::new();
    let mut success = true;

    for (i, step) in script.steps.iter().enumerate() {
        let cmd_payload = match step {
            ScriptStep::Shell { cmd, args } => CommandPayload::ShellExec { cmd: cmd.clone(), args: args.clone() },
            ScriptStep::Upload { local_path, remote_path } => {
                let host = format!("{}:{}", state.config.host, state.config.port);
                let download_url = format!("http://{}/api/files/download/staging/{}", host, local_path);
                CommandPayload::DownloadFile { url: download_url, dest_path: remote_path.clone() }
            },
            ScriptStep::Download { remote_path } => {
                let upload_id = Uuid::new_v4();
                let host = format!("{}:{}", state.config.host, state.config.port);
                let upload_url = format!("http://{}/api/files/client-upload/{}", host, upload_id);
                CommandPayload::UploadFile { src_path: remote_path.clone(), upload_url }
            }
        };
        
        logs.push(format!("Step {}: Started", i + 1));
        
        // Send command
        if let Some(client) = state.clients.get(&client_id) {
            let cmd_id = Uuid::new_v4();
            let msg = Message::Command {
                id: cmd_id,
                cmd: cmd_payload,
            };
            
            if let Err(e) = client.tx.send(msg).await {
                logs.push(format!("Step {}: Failed to send command: {}", i + 1, e));
                success = false;
                break;
            }
            
            // Wait for result
            let mut step_success = false;
            for _ in 0..60 { // Wait up to 30s
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                if let Some(result) = state.results.get(&cmd_id) {
                     match result.value() {
                         CommandResult::Error(e) => {
                             logs.push(format!("Step {}: Failed: {}", i + 1, e));
                         },
                         CommandResult::ShellOutput { stdout, stderr, exit_code } => {
                             if *exit_code != 0 {
                                 logs.push(format!("Step {}: Shell command failed (Exit Code: {}). Stderr: {}", i + 1, exit_code, stderr));
                             } else {
                                 logs.push(format!("Step {}: Completed. Output: {}", i + 1, stdout));
                                 step_success = true;
                             }
                         },
                         res => {
                             logs.push(format!("Step {}: Completed. Result: {:?}", i + 1, res));
                             step_success = true;
                         }
                     }
                     break;
                }
            }
            
            if !step_success {
                logs.push(format!("Step {}: Timed out or failed", i + 1));
                success = false;
                break;
            }
            
        } else {
            logs.push("Client disconnected".to_string());
            success = false;
            break;
        }
    }
    
    let status = if success { "completed" } else { "failed" };
    let logs_json = serde_json::to_string(&logs).unwrap_or("[]".to_string());
    let history_id_str = history_id.to_string();
    
    // Update history
    let _ = sqlx::query!(
        "UPDATE execution_history SET status = ?, completed_at = CURRENT_TIMESTAMP, logs = ? WHERE id = ?",
        status, logs_json, history_id_str
    ).execute(&state.db).await;
    
    info!("Script {} finished on client {} with status {}", script.name, client_id, status);
}

// API: Get Execution History
#[derive(serde::Serialize)]
pub struct ExecutionHistoryItem {
    pub id: Uuid,
    pub script_name: String,
    pub client_hostname: String,
    pub status: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub logs: Vec<String>,
}

pub async fn get_script_history(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<ExecutionHistoryItem>> {
    let rows = sqlx::query!(
        r#"
        SELECT h.id, s.name as script_name, c.hostname as client_hostname, h.status, CAST(h.started_at AS TEXT) as started_at, CAST(h.completed_at AS TEXT) as completed_at, h.logs
        FROM execution_history h
        JOIN scripts s ON h.script_id = s.id
        LEFT JOIN clients c ON h.client_id = c.id
        ORDER BY h.started_at DESC
        LIMIT 50
        "#
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let history = rows.into_iter().map(|r| {
        let logs: Vec<String> = r.logs.as_deref().and_then(|l| serde_json::from_str(l).ok()).unwrap_or_default();
        ExecutionHistoryItem {
            id: Uuid::parse_str(r.id.as_deref().unwrap_or("")).unwrap_or_default(),
            script_name: r.script_name,
            client_hostname: r.client_hostname.unwrap_or("Unknown".to_string()),
            status: r.status,
            started_at: r.started_at.unwrap_or_default(),
            completed_at: r.completed_at,
            logs,
        }
    }).collect();
    Json(history)
}

// API: Clear Execution History
pub async fn clear_script_history(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Err(e) = sqlx::query!("DELETE FROM execution_history").execute(&state.db).await {
         return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to clear history: {}", e)).into_response();
    }
    (StatusCode::OK, "History cleared").into_response()
}

// API: Admin uploads file to Staging (to be downloaded by Client)
pub async fn upload_file_admin(
    headers: HeaderMap,
    mut multipart: Multipart
) -> impl IntoResponse {
    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        let file_name = field.file_name().map(|s| s.to_string()).unwrap_or_else(|| "uploaded_file".to_string());
        let data = match field.bytes().await {
            Ok(d) => d,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read bytes: {}", e)).into_response(),
        };

        // Save to uploads/staging/
        let dir_path = "uploads/staging";
        if let Err(e) = tokio::fs::create_dir_all(dir_path).await {
             return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create directory: {}", e)).into_response();
        }

        let path = format!("{}/{}", dir_path, file_name);
        if let Err(e) = File::create(&path).await {
             return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create file: {}", e)).into_response();
        }
        if let Err(e) = tokio::fs::write(&path, &data).await {
             return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write file: {}", e)).into_response();
        }
        
        // Construct download URL
        let host = headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or("localhost:3000");
        let url = format!("http://{}/api/files/download/staging/{}", host, file_name);
        
        return (StatusCode::OK, Json(serde_json::json!({ "url": url }))).into_response();
    }
    (StatusCode::BAD_REQUEST, "No file provided").into_response()
}

// API: Client uploads file (Result of UploadFile command)
pub async fn upload_file_client(
    Path(id): Path<Uuid>, // Command ID
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart
) -> impl IntoResponse {
    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        let file_name = field.file_name().map(|s| s.to_string()).unwrap_or_else(|| "client_upload".to_string());
        let data = match field.bytes().await {
            Ok(d) => d,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read bytes: {}", e)).into_response(),
        };

        // Save to uploads/client_data/<id>/
        let dir_path = format!("uploads/client_data/{}", id);
        if let Err(_) = tokio::fs::create_dir_all(&dir_path).await {
             // ignore error if exists
        }
        
        let file_path = format!("{}/{}", dir_path, file_name);
         if let Err(e) = tokio::fs::write(&file_path, &data).await {
             return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write file: {}", e)).into_response();
        }
        
        // Update the Command Result in State
        // The client will also send a Response via WebSocket, but this confirms the file is here.
        // We can optionally update the result here, but the WebSocket response is the source of truth for "Command Finished".
        // However, we can store the file path in the result via the Response message.
        
        info!("File uploaded by client for command {}: {}", id, file_path);
        
        return (StatusCode::OK, "Upload successful").into_response();
    }
    (StatusCode::BAD_REQUEST, "No file provided").into_response()
}

// API: Download file (Generic)
// Serves files from staging or client_data
// path_type: "staging" or "client_data"
// id_or_file: filename (for staging) or uuid/filename (for client_data)
// Since Axum path matching is simple, we can make two routes or one flexible one.
// Let's rely on ServeDir for this! It's much easier and supports ranges, etc.
// We will configure ServeDir in main.rs to serve server/uploads under /api/files/download/


// API: List connected clients
#[derive(serde::Serialize)]
pub struct ClientSummary {
    pub id: Uuid,
    pub hostname: String,
    pub os: String,
    pub alias: Option<String>,
    pub ip: String,
    pub version: String,
    pub status: String,
}

pub async fn list_clients(State(state): State<Arc<AppState>>) -> Json<Vec<ClientSummary>> {
    let rows = sqlx::query("SELECT id, hostname, os, alias, ip, version, status FROM clients ORDER BY last_seen DESC")
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

    let clients = rows.into_iter().map(|r| {
        let id_str: String = r.get("id");
        let id = Uuid::parse_str(&id_str).unwrap_or_default();
        let is_connected = state.clients.contains_key(&id);
        
        let hostname: String = r.get("hostname");
        let os: String = r.get("os");
        let alias: Option<String> = r.get("alias");
        let db_ip: Option<String> = r.get("ip");
        let db_version: Option<String> = r.get("version");
        let _db_status: String = r.get("status");

        let (ip, version, status) = if is_connected {
            if let Some(conn) = state.clients.get(&id) {
                (conn.ip.clone(), conn.version.clone(), "online".to_string())
            } else {
                (db_ip.unwrap_or_default(), db_version.unwrap_or_default(), "online".to_string())
            }
        } else {
            (db_ip.unwrap_or_default(), db_version.unwrap_or_default(), "offline".to_string())
        };

        ClientSummary {
            id,
            hostname,
            os,
            alias,
            ip,
            version,
            status,
        }
    }).collect();
    Json(clients)
}

// API: Send command to client
#[derive(serde::Deserialize)]
pub struct CommandRequest {
    pub cmd: CommandPayload,
}

pub async fn send_command(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(payload): Json<CommandRequest>,
) -> impl IntoResponse {
    if let Some(client) = state.clients.get(&id) {
        let cmd_id = Uuid::new_v4();
        let msg = Message::Command {
            id: cmd_id,
            cmd: payload.cmd,
        };
        match client.tx.send(msg).await {
            Ok(_) => (StatusCode::OK, format!("{}", cmd_id)).into_response(), // Return just the ID
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to send").into_response(),
        }
    } else {
        (StatusCode::NOT_FOUND, "Client not found").into_response()
    }
}

// API: Get command result
pub async fn get_command_result(
    State(state): State<Arc<AppState>>,
    Path(cmd_id): Path<Uuid>,
) -> impl IntoResponse {
    if let Some(result) = state.results.get(&cmd_id) {
        (StatusCode::OK, Json(result.clone())).into_response()
    } else {
        (StatusCode::NOT_FOUND, "Result not ready or invalid ID").into_response()
    }
}


pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state, addr))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, addr: SocketAddr) {
    let (mut sender, mut receiver) = socket.split();

    // Authenticate first
    // Wait for the first message which MUST be Register
    let client_id: Uuid;
    let hostname: String;
    let os: String;
    let alias: Option<String>;
    let version: String;

    // We can't really read "first message" easily without consuming the stream.
    // So we'll enter a loop but expect registration first.
    
    // For simplicity, let's just assume the first message is Register.
    // In a real app, we might want a timeout here.
    
    let msg = match receiver.next().await {
        Some(Ok(msg)) => msg,
        Some(Err(e)) => {
            error!("Error receiving registration: {}", e);
            return;
        }
        None => return,
    };

    match parse_message(msg) {
        Ok(Message::Register { client_id: id, token, hostname: h, os: o, alias: a, version: v }) => {
            // Verify token
            if token != state.config.auth_token {
                 let _ = sender.send(WsMessage::Text(serde_json::to_string(&Message::AuthFailed("Invalid token".into())).unwrap())).await;
                 return;
            }
            
            client_id = id;
            hostname = h;
            os = o;
            alias = a;
            version = v;
            
            info!("Client registered: {} ({}) - {} [Alias: {:?}] [IP: {}] [Ver: {}]", client_id, hostname, os, alias, addr, version);
            
            // Persist client to DB for history joins
            let client_id_str = client_id.to_string();
            let ip_str = addr.ip().to_string();
            
            if let Err(e) = sqlx::query(
                "INSERT INTO clients (id, hostname, os, last_seen, status, alias, ip, version) VALUES (?, ?, ?, CURRENT_TIMESTAMP, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET hostname = excluded.hostname, os = excluded.os, last_seen = CURRENT_TIMESTAMP, status = excluded.status, alias = excluded.alias, ip = excluded.ip, version = excluded.version"
            )
            .bind(&client_id_str)
            .bind(&hostname)
            .bind(&os)
            .bind("connected")
            .bind(&alias)
            .bind(&ip_str)
            .bind(&version)
            .execute(&state.db).await {
                error!("Failed to persist client to DB: {}", e);
            }

            let _ = sender.send(WsMessage::Text(serde_json::to_string(&Message::AuthSuccess).unwrap())).await;
        }
        _ => {
            warn!("First message was not Register");
            return;
        }
    }

    // Create a channel for this client
    let (tx, mut rx) = mpsc::channel::<Message>(100);

    // Add to state
    state.clients.insert(client_id, ClientConnection {
        tx,
        hostname: hostname.clone(),
        os: os.clone(),
        alias: alias.clone(),
        ip: addr.ip().to_string(),
        version: version.clone(),
    });

    // Spawn task to send messages FROM channel TO websocket
    let mut send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let json = serde_json::to_string(&msg).unwrap();
            if sender.send(WsMessage::Text(json)).await.is_err() {
                break;
            }
        }
    });

    // Handle incoming messages FROM websocket
    let mut recv_task = {
        let state = state.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = receiver.next().await {
                match parse_message(msg) {
                    Ok(parsed_msg) => {
                        match parsed_msg {
                            Message::Heartbeat => {
                                // Update last seen in DB (TODO)
                                // info!("Heartbeat from {}", client_id);
                            }
                            Message::Response { id, result } => {
                                info!("Received response for command {}: {:?}", id, result);
                                state.results.insert(id, result);
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        error!("Failed to parse message: {}", e);
                    }
                }
            }
            // Cleanup
            state.clients.remove(&client_id);
            let client_id_str = client_id.to_string();
            let _ = sqlx::query("UPDATE clients SET status = ? WHERE id = ?")
                .bind("disconnected")
                .bind(&client_id_str)
                .execute(&state.db).await;
            info!("Client disconnected: {}", client_id);
        })
    };

    // Wait for either task to finish
    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    }
}

fn parse_message(msg: WsMessage) -> anyhow::Result<Message> {
    match msg {
        WsMessage::Text(text) => {
            Ok(serde_json::from_str(&text)?)
        }
        WsMessage::Binary(bin) => {
             Ok(serde_json::from_slice(&bin)?)
        }
        _ => Err(anyhow::anyhow!("Unsupported message type")),
    }
}
