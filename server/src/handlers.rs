use axum::{
    extract::{ws::{Message as WsMessage, WebSocket, WebSocketUpgrade}, State, Json, Path, ConnectInfo, Multipart},
    response::IntoResponse,
    http::{StatusCode, HeaderMap},
    body::Body,
};
use futures::{sink::SinkExt, stream::StreamExt};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;
use tracing::{info, error, warn};
use std::net::SocketAddr;

use crate::state::{AppState, ClientConnection};
use common::{Message, CommandPayload};

pub async fn index() -> &'static str {
    "Roam Server Running"
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

        // Save to server/uploads/staging/
        let path = format!("server/uploads/staging/{}", file_name);
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

        // Save to server/uploads/client_data/<id>/
        let dir_path = format!("server/uploads/client_data/{}", id);
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
}

pub async fn list_clients(State(state): State<Arc<AppState>>) -> Json<Vec<ClientSummary>> {
    let clients = state.clients.iter().map(|c| ClientSummary {
        id: *c.key(),
        hostname: c.value().hostname.clone(),
        os: c.value().os.clone(),
        alias: c.value().alias.clone(),
        ip: c.value().ip.clone(),
        version: c.value().version.clone(),
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
