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
use sha2::{Sha256, Digest};
use hex;

use crate::state::{AppState, ClientConnection, ScriptGroup, ScriptStep, ExecutionProgress};
use common::{Message, CommandPayload, CommandResult};

#[allow(dead_code)]
pub async fn index() -> &'static str {
    "Roam Server Running"
}

// API: Get Server Info
#[derive(serde::Serialize)]
pub struct ServerInfo {
    pub version: String,
}

pub async fn get_server_info() -> Json<ServerInfo> {
    Json(ServerInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

use serde::Deserialize;

// API: List Groups
#[derive(serde::Serialize)]
pub struct ClientGroup {
    pub id: Uuid,
    pub name: String,
    pub client_ids: Vec<Uuid>,
    pub script_ids: Vec<Uuid>,
}

pub async fn list_groups(State(state): State<Arc<AppState>>) -> Json<Vec<ClientGroup>> {
    let groups = sqlx::query!("SELECT id, name FROM client_groups ORDER BY created_at DESC")
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

    let mut result = Vec::new();
    for group in groups {
        let group_id_str = group.id.unwrap_or_default();
        let group_id = Uuid::parse_str(&group_id_str).unwrap_or_default();
        
        let members = sqlx::query!("SELECT client_id FROM client_group_members WHERE group_id = ?", group_id_str)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();
            
        let client_ids = members.into_iter()
            .map(|m| Uuid::parse_str(&m.client_id).unwrap_or_default())
            .collect();
            
        let scripts = sqlx::query!("SELECT script_id FROM group_scripts WHERE group_id = ?", group_id_str)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();

        let script_ids = scripts.into_iter()
            .map(|s| Uuid::parse_str(&s.script_id).unwrap_or_default())
            .collect();
            
        result.push(ClientGroup {
            id: group_id,
            name: group.name,
            client_ids,
            script_ids,
        });
    }
    
    Json(result)
}

// API: Create Group
#[derive(Deserialize)]
pub struct CreateGroupRequest {
    pub name: String,
}

pub async fn create_group(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateGroupRequest>,
) -> impl IntoResponse {
    let id = Uuid::new_v4();
    let id_str = id.to_string();
    
    if let Err(e) = sqlx::query!(
        "INSERT INTO client_groups (id, name) VALUES (?, ?)",
        id_str, payload.name
    ).execute(&state.db).await {
         return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create group: {}", e)).into_response();
    }
    
    (StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response()
}

// API: Delete Group
pub async fn delete_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let id_str = id.to_string();
    if let Err(e) = sqlx::query!("DELETE FROM client_groups WHERE id = ?", id_str).execute(&state.db).await {
         return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete group: {}", e)).into_response();
    }
    (StatusCode::OK, "Group deleted").into_response()
}

// API: Update Group (Members and Scripts)
#[derive(Deserialize)]
pub struct UpdateGroupRequest {
    pub client_ids: Option<Vec<Uuid>>,
    pub script_ids: Option<Vec<Uuid>>,
}

pub async fn update_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateGroupRequest>,
) -> impl IntoResponse {
    let group_id_str = id.to_string();
    
    // Update Members
    if let Some(client_ids) = payload.client_ids {
        if let Err(e) = sqlx::query!("DELETE FROM client_group_members WHERE group_id = ?", group_id_str).execute(&state.db).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to clear members: {}", e)).into_response();
        }
        for client_id in client_ids {
            let client_id_str = client_id.to_string();
            if let Err(e) = sqlx::query!(
                "INSERT INTO client_group_members (group_id, client_id) VALUES (?, ?)",
                group_id_str, client_id_str
            ).execute(&state.db).await {
                 error!("Failed to add member to group: {}", e);
            }
        }
    }

    // Update Scripts
    if let Some(script_ids) = payload.script_ids {
        if let Err(e) = sqlx::query!("DELETE FROM group_scripts WHERE group_id = ?", group_id_str).execute(&state.db).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to clear scripts: {}", e)).into_response();
        }
        for script_id in script_ids {
            let script_id_str = script_id.to_string();
            if let Err(e) = sqlx::query!(
                "INSERT INTO group_scripts (group_id, script_id) VALUES (?, ?)",
                group_id_str, script_id_str
            ).execute(&state.db).await {
                 error!("Failed to add script to group: {}", e);
            }
        }
    }
    
    (StatusCode::OK, "Group updated").into_response()
}

// API: Run Group Scripts
pub async fn run_group_scripts(
    State(state): State<Arc<AppState>>,
    Path(group_id): Path<Uuid>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let group_id_str = group_id.to_string();
    
    // Determine server host
    let host = headers.get("host")
        .and_then(|h| h.to_str().ok())
        .map(|h| h.to_string())
        .unwrap_or_else(|| format!("{}:{}", state.config.host, state.config.port));
    
    // 1. Fetch Group Members
    let members = match sqlx::query!("SELECT client_id FROM client_group_members WHERE group_id = ?", group_id_str)
        .fetch_all(&state.db)
        .await {
            Ok(m) => m,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to fetch members: {}", e)).into_response(),
        };

    if members.is_empty() {
        return (StatusCode::BAD_REQUEST, "Group has no members").into_response();
    }

    // 2. Fetch Group Scripts
    let scripts_rows = match sqlx::query!("SELECT script_id FROM group_scripts WHERE group_id = ?", group_id_str)
        .fetch_all(&state.db)
        .await {
            Ok(s) => s,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to fetch scripts: {}", e)).into_response(),
        };

    if scripts_rows.is_empty() {
        return (StatusCode::BAD_REQUEST, "Group has no bound scripts").into_response();
    }

    let mut scripts = Vec::new();
    for row in scripts_rows {
        let script_id_str = row.script_id;
         let script_row = match sqlx::query!("SELECT id, name, steps FROM scripts WHERE id = ?", script_id_str)
            .fetch_optional(&state.db)
            .await {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB Error: {}", e)).into_response(),
            };
        
        let steps: Vec<ScriptStep> = serde_json::from_str(&script_row.steps).unwrap_or_default();
        scripts.push(ScriptGroup {
            id: Uuid::parse_str(script_row.id.as_deref().unwrap_or("")).unwrap_or_default(),
            name: script_row.name,
            steps,
        });
    }

    // 3. Spawn Tasks
    for member in members {
        let client_id = Uuid::parse_str(&member.client_id).unwrap_or_default();
        if !state.clients.contains_key(&client_id) {
            continue;
        }

        let state_clone = state.clone();
        let scripts_clone = scripts.clone();
        let host_clone = host.clone();
        
        tokio::spawn(async move {
            for script in scripts_clone {
                let history_id = Uuid::new_v4();
                let history_id_str = history_id.to_string();
                let script_id_str = script.id.to_string();
                let client_id_str = client_id.to_string();
                
                // Create History Record
                let now_utc = chrono::Utc::now();
                if let Err(e) = sqlx::query!(
                    "INSERT INTO execution_history (id, script_id, client_id, status, started_at) VALUES (?, ?, ?, ?, ?)",
                    history_id_str, script_id_str, client_id_str, "running", now_utc
                ).execute(&state_clone.db).await {
                    error!("Failed to create history record: {}", e);
                    continue;
                }

                run_script_task(state_clone.clone(), client_id, script, history_id, host_clone.clone()).await;
            }
        });
    }

    (StatusCode::OK, "Group execution started").into_response()
}

// API: Get Active Executions
pub async fn get_active_executions(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<ExecutionProgress>> {
    let mut list: Vec<ExecutionProgress> = state.active_executions.iter().map(|r| r.value().clone()).collect();
    // Sort by script name or client?
    // Let's sort by client hostname
    list.sort_by(|a, b| a.client_hostname.cmp(&b.client_hostname));
    Json(list)
}

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
    headers: HeaderMap,
    Json(payload): Json<RunScriptRequest>,
) -> impl IntoResponse {
    let script_id_str = script_id.to_string();
    
    // Determine server host
    let host = headers.get("host")
        .and_then(|h| h.to_str().ok())
        .map(|h| h.to_string())
        .unwrap_or_else(|| format!("{}:{}", state.config.host, state.config.port));

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
        let now_utc = chrono::Utc::now();
        if let Err(e) = sqlx::query!(
            "INSERT INTO execution_history (id, script_id, client_id, status, started_at) VALUES (?, ?, ?, ?, ?)",
            history_id_str, script_id_str_run, client_id_str, "running", now_utc
        ).execute(&state.db).await {
            error!("Failed to create history record: {}", e);
            continue;
        }

        let state_clone = state.clone();
        let script_clone = script.clone();
        let host_clone = host.clone();
        tokio::spawn(async move {
            run_script_task(state_clone, client_id, script_clone, history_id, host_clone).await;
        });
    }

    (StatusCode::OK, "Script execution started on selected clients").into_response()
}

use walkdir::WalkDir;
use zip::write::FileOptions;
use std::io;

fn zip_directory(src_dir: &str, dst_file: &str) -> anyhow::Result<()> {
    if !std::path::Path::new(src_dir).is_dir() {
        return Err(anyhow::anyhow!("Source is not a directory"));
    }

    let file = std::fs::File::create(dst_file)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    let walkdir = WalkDir::new(src_dir);
    let it = walkdir.into_iter();

    for entry in it {
        let entry = entry?;
        let path = entry.path();
        let name = path.strip_prefix(std::path::Path::new(src_dir))?;
        let path_as_string = name
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("Invalid path"))?;

        if path.is_file() {
            zip.start_file(path_as_string, options)?;
            let mut f = std::fs::File::open(path)?;
            std::io::copy(&mut f, &mut zip)?;
        } else if !name.as_os_str().is_empty() {
            zip.add_directory(path_as_string, options)?;
        }
    }
    zip.finish()?;
    Ok(())
}

async fn run_script_task(state: Arc<AppState>, client_id: Uuid, script: ScriptGroup, history_id: Uuid, server_host: String) {
    info!("Starting script {} on client {}", script.name, client_id);
    
    // Get client hostname for progress
    let client_hostname = if let Some(c) = state.clients.get(&client_id) {
        c.hostname.clone()
    } else {
        "Unknown".to_string()
    };

    let mut logs = Vec::new();
    let mut success = true;
    let total_steps = script.steps.len();

    // Initialize Active Execution
    state.active_executions.insert(history_id, ExecutionProgress {
        execution_id: history_id,
        script_name: script.name.clone(),
        client_hostname: client_hostname.clone(),
        status: "running".to_string(),
        logs: Vec::new(),
        current_step: 0,
        total_steps,
    });

    for (i, step) in script.steps.iter().enumerate() {
        // Update Progress
        if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
            progress.current_step = i + 1;
        }

        let cmd_payload_result = match step {
            ScriptStep::Shell { cmd, args } => Ok(CommandPayload::ShellExec { cmd: cmd.clone(), args: args.clone() }),
            ScriptStep::Upload { local_path, remote_path } => {
                let download_url = format!("http://{}/api/files/download/staging/{}", server_host, local_path);
                Ok(CommandPayload::DownloadFile { url: download_url, dest_path: remote_path.clone() })
            },
            ScriptStep::Download { remote_path, browser_download } => {
                let upload_id = Uuid::new_v4();
                let upload_url = format!("http://{}/api/files/client-upload/{}", server_host, upload_id);
                
                if browser_download.unwrap_or(false) {
                    let file_name = std::path::Path::new(remote_path).file_name().unwrap_or_default().to_string_lossy();
                    let download_link = format!("http://{}/api/files/download/client_data/{}/{}", server_host, upload_id, file_name);
                    let log_msg = format!("BROWSER_DOWNLOAD: {}", download_link);
                    logs.push(log_msg.clone());
                    if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                        progress.logs.push(log_msg);
                    }
                }
                
                Ok(CommandPayload::UploadFile { src_path: remote_path.clone(), upload_url })
            },
            ScriptStep::UploadDir { local_path, remote_path } => {
                // Zip the directory first
                let src_dir = format!("uploads/staging/{}", local_path);
                let zip_name = format!("{}.zip", local_path);
                let dst_zip = format!("uploads/staging/{}", zip_name);
                
                match zip_directory(&src_dir, &dst_zip) {
                    Ok(_) => {
                        let download_url = format!("http://{}/api/files/download/staging/{}", server_host, zip_name);
                        Ok(CommandPayload::DownloadAndUnzip { url: download_url, dest_path: remote_path.clone() })
                    },
                    Err(e) => Err(format!("Failed to zip directory: {}", e))
                }
            },
            ScriptStep::DownloadDir { remote_path, browser_download } => {
                let upload_id = Uuid::new_v4();
                // Client will upload a zip file, server receives it as generic file upload
                let upload_url = format!("http://{}/api/files/client-upload/{}", server_host, upload_id);
                
                if browser_download.unwrap_or(false) {
                    let file_name = format!("{}.zip", std::path::Path::new(remote_path).file_name().unwrap_or_default().to_string_lossy());
                    let download_link = format!("http://{}/api/files/download/client_data/{}/{}", server_host, upload_id, file_name);
                    let log_msg = format!("BROWSER_DOWNLOAD: {}", download_link);
                    logs.push(log_msg.clone());
                    if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                        progress.logs.push(log_msg);
                    }
                }

                Ok(CommandPayload::ZipAndUpload { src_path: remote_path.clone(), upload_url })
            }
        };
        
        let step_desc = match step {
            ScriptStep::Shell { cmd, args } => format!("Shell: {} {}", cmd, args.join(" ")),
            ScriptStep::Upload { local_path, remote_path } => format!("Upload: {} -> {}", local_path, remote_path),
            ScriptStep::Download { remote_path, .. } => format!("Download: {}", remote_path),
            ScriptStep::UploadDir { local_path, remote_path } => format!("UploadDir: {} -> {}", local_path, remote_path),
            ScriptStep::DownloadDir { remote_path, .. } => format!("DownloadDir: {}", remote_path),
        };
        
        let log_start = format!("Step {}: Started - {}", i + 1, step_desc);
        logs.push(log_start.clone());
        if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
            progress.logs.push(log_start);
        }

        if let Err(e) = cmd_payload_result {
             let log_err = format!("Step {}: Setup failed: {}", i + 1, e);
             logs.push(log_err.clone());
             if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                 progress.logs.push(log_err);
             }
             success = false;
             break;
        }
        let cmd_payload = cmd_payload_result.unwrap();
        
        // Send command
        if let Some(client) = state.clients.get(&client_id) {
            let cmd_id = Uuid::new_v4();
            let msg = Message::Command {
                id: cmd_id,
                cmd: cmd_payload,
            };
            
            if let Err(e) = client.tx.send(msg).await {
                let log_err = format!("Step {}: Failed to send command: {}", i + 1, e);
                logs.push(log_err.clone());
                if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                    progress.logs.push(log_err);
                }
                success = false;
                break;
            }
            
            // Wait for result
            let mut step_success = false;
            for _ in 0..60 { // Wait up to 30s
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                if let Some(result) = state.results.get(&cmd_id) {
                     let log_res = match result.value() {
                         CommandResult::Error(e) => {
                             format!("Step {}: Failed: {}", i + 1, e)
                         },
                         CommandResult::ShellOutput { stdout, stderr, exit_code } => {
                             if *exit_code != 0 {
                                 format!("Step {}: Shell command failed (Exit Code: {}). Stderr: {}", i + 1, exit_code, stderr)
                             } else {
                                 step_success = true;
                                 format!("Step {}: Completed. Output: {}", i + 1, stdout)
                             }
                         },
                         res => {
                             step_success = true;
                             format!("Step {}: Completed. Result: {:?}", i + 1, res)
                         }
                     };
                     
                     logs.push(log_res.clone());
                     if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                        progress.logs.push(log_res);
                     }
                     break;
                }
            }
            
            if !step_success {
                let log_timeout = format!("Step {}: Timed out or failed", i + 1);
                logs.push(log_timeout.clone());
                if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                    progress.logs.push(log_timeout);
                }
                success = false;
                break;
            }
            
        } else {
            let log_disc = "Client disconnected".to_string();
            logs.push(log_disc.clone());
            if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                progress.logs.push(log_disc);
            }
            success = false;
            break;
        }
    }
    
    let status = if success { "completed" } else { "failed" };
    
    // Update Active Execution Status
    if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
        progress.status = status.to_string();
    }
    
    let logs_json = serde_json::to_string(&logs).unwrap_or("[]".to_string());
    let history_id_str = history_id.to_string();
    
    // Update history
    let _ = sqlx::query!(
        "UPDATE execution_history SET status = ?, completed_at = CURRENT_TIMESTAMP, logs = ? WHERE id = ?",
        status, logs_json, history_id_str
    ).execute(&state.db).await;
    
    info!("Script {} finished on client {} with status {}", script.name, client_id, status);
    
    // Keep in active_executions for a bit? Or remove?
    // If we remove immediately, the frontend might miss the final status if it's polling.
    // Let's remove it after a short delay (e.g. 5 seconds) to allow the frontend to catch the completion.
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        state.active_executions.remove(&history_id);
    });
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
        let host = headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or("localhost:3333");
        let url = format!("http://{}/api/files/download/staging/{}", host, file_name);
        
        return (StatusCode::OK, Json(serde_json::json!({ "url": url }))).into_response();
    }
    (StatusCode::BAD_REQUEST, "No file provided").into_response()
}

// API: Client uploads file (Result of UploadFile command)
pub async fn upload_file_client(
    Path(id): Path<Uuid>, // Command ID
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
    pub ips: Vec<String>,
    pub version: String,
    pub status: String,
    pub last_seen: Option<String>,
}

pub async fn list_clients(State(state): State<Arc<AppState>>) -> Json<Vec<ClientSummary>> {
    let rows = sqlx::query("SELECT id, hostname, os, alias, ip, ips, version, status, last_seen FROM clients ORDER BY last_seen DESC")
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

    let clients = rows.into_iter().map(|r| {
        let id_str: String = r.get("id");
        let id = Uuid::parse_str(&id_str).unwrap_or_default();
        let is_connected = state.clients.contains_key(&id);
        
        let db_hostname: String = r.get("hostname");
        let db_os: String = r.get("os");
        let db_alias: Option<String> = r.get("alias");
        let db_ip: Option<String> = r.get("ip");
        let db_ips: Option<String> = r.get("ips");
        let db_version: Option<String> = r.get("version");
        let _db_status: String = r.get("status");
        let db_last_seen: Option<chrono::NaiveDateTime> = r.get("last_seen");
        
        let last_seen = db_last_seen.map(|d| format!("{}Z", d.format("%Y-%m-%dT%H:%M:%S")));
        let parsed_db_ips: Vec<String> = db_ips.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();

        let (hostname, os, alias, ip, ips, version, status) = if is_connected {
            if let Some(conn) = state.clients.get(&id) {
                (
                    conn.hostname.clone(),
                    conn.os.clone(),
                    conn.alias.clone(),
                    conn.ip.clone(),
                    conn.ips.clone(),
                    conn.version.clone(),
                    "online".to_string()
                )
            } else {
                (
                    db_hostname,
                    db_os,
                    db_alias,
                    db_ip.unwrap_or_default(),
                    parsed_db_ips,
                    db_version.unwrap_or_default(),
                    "online".to_string()
                )
            }
        } else {
            (
                db_hostname,
                db_os,
                db_alias,
                db_ip.unwrap_or_default(),
                parsed_db_ips,
                db_version.unwrap_or_default(),
                "offline".to_string()
            )
        };

        ClientSummary {
            id,
            hostname,
            os,
            alias,
            ip,
            ips,
            version,
            status,
            last_seen,
        }
    }).collect();
    Json(clients)
}

// API: Delete Client (Remove from DB and disconnect)
pub async fn delete_client(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let id_str = id.to_string();
    
    // 1. Remove from active connections (this will effectively disconnect the client)
    if state.clients.remove(&id).is_some() {
        info!("Client {} disconnected due to deletion", id);
    }
    
    // 2. Remove from DB (client_group_members first)
    if let Err(e) = sqlx::query!("DELETE FROM client_group_members WHERE client_id = ?", id_str).execute(&state.db).await {
         error!("Failed to remove client from groups: {}", e);
    }
    
    // 3. Remove from clients table
    if let Err(e) = sqlx::query!("DELETE FROM clients WHERE id = ?", id_str).execute(&state.db).await {
         return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete client from DB: {}", e)).into_response();
    }
    
    (StatusCode::OK, "Client deleted").into_response()
}

// API: Send command to client
pub async fn send_command(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(cmd): Json<CommandPayload>,
) -> impl IntoResponse {
    if let Some(client) = state.clients.get(&id) {
        let cmd_id = Uuid::new_v4();
        let msg = Message::Command {
            id: cmd_id,
            cmd,
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


// API: Client Update Management

#[derive(serde::Serialize)]
pub struct ClientUpdateItem {
    pub id: Uuid,
    pub version: String,
    pub filename: String,
    pub platform: String,
    pub uploaded_at: String,
}

pub async fn list_updates(State(state): State<Arc<AppState>>) -> Json<Vec<ClientUpdateItem>> {
    let rows = sqlx::query!("SELECT id, version, filename, platform, CAST(uploaded_at AS TEXT) as uploaded_at FROM client_updates ORDER BY uploaded_at DESC")
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    
    let items = rows.into_iter().map(|r| ClientUpdateItem {
        id: Uuid::parse_str(&r.id.unwrap_or_default()).unwrap_or_default(),
        version: r.version,
        filename: r.filename,
        platform: r.platform,
        uploaded_at: r.uploaded_at.unwrap_or_default(),
    }).collect();
    
    Json(items)
}

pub async fn delete_update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let id_str = id.to_string();
    
    // Get filename to delete file
    let row = match sqlx::query!("SELECT filename FROM client_updates WHERE id = ?", id_str)
        .fetch_optional(&state.db)
        .await {
            Ok(Some(r)) => r,
            Ok(None) => return (StatusCode::NOT_FOUND, "Update not found").into_response(),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB Error: {}", e)).into_response(),
        };
    
    // Delete from DB
    if let Err(e) = sqlx::query!("DELETE FROM client_updates WHERE id = ?", id_str).execute(&state.db).await {
         return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete update record: {}", e)).into_response();
    }
    
    // Delete file
    let path = format!("uploads/updates/{}", row.filename);
    let _ = tokio::fs::remove_file(path).await;
    
    (StatusCode::OK, "Update deleted").into_response()
}

pub async fn upload_update(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart
) -> impl IntoResponse {
    let mut version = String::new();
    let mut platform = String::new();
    let mut file_saved = false;
    let mut saved_filename = String::new();

    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        let name = field.name().unwrap_or("").to_string();
        
        if name == "version" {
            version = field.text().await.unwrap_or_default();
        } else if name == "platform" {
            platform = field.text().await.unwrap_or_default();
        } else if name == "file" {
            let file_name = field.file_name().map(|s| s.to_string()).unwrap_or_else(|| "client_update".to_string());
            let data = match field.bytes().await {
                Ok(d) => d,
                Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read bytes: {}", e)).into_response(),
            };
            
            let dir_path = "uploads/updates";
            if let Err(e) = tokio::fs::create_dir_all(dir_path).await {
                 return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create directory: {}", e)).into_response();
            }
            
            // Avoid collisions? Or overwrite? 
            // Let's prepend UUID or just use original name if unique enough.
            // Or better: use UUID as filename on disk, keep original name in DB? 
            // For simplicity, let's use original filename but user should be careful.
            let path = format!("{}/{}", dir_path, file_name);
            if let Err(e) = tokio::fs::write(&path, &data).await {
                 return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write file: {}", e)).into_response();
            }
            saved_filename = file_name;
            file_saved = true;
        }
    }
    
    if !file_saved || version.is_empty() || platform.is_empty() {
        return (StatusCode::BAD_REQUEST, "Missing fields (version, platform, file)").into_response();
    }
    
    let id = Uuid::new_v4();
    let id_str = id.to_string();
    
    if let Err(e) = sqlx::query!(
        "INSERT INTO client_updates (id, version, filename, platform) VALUES (?, ?, ?, ?)",
        id_str, version, saved_filename, platform
    ).execute(&state.db).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to save metadata: {}", e)).into_response();
    }
    
    (StatusCode::CREATED, "Update uploaded").into_response()
}

#[derive(serde::Deserialize)]
pub struct TriggerUpdatePayload {
    pub client_ids: Vec<Uuid>,
    pub update_id: Uuid,
}

pub async fn trigger_update_clients(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<TriggerUpdatePayload>,
) -> impl IntoResponse {
    let update_id_str = payload.update_id.to_string();
    
    // Get update file info
    let update = match sqlx::query!("SELECT filename FROM client_updates WHERE id = ?", update_id_str)
        .fetch_optional(&state.db)
        .await {
            Ok(Some(r)) => r,
            Ok(None) => return (StatusCode::NOT_FOUND, "Update package not found").into_response(),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB Error: {}", e)).into_response(),
        };

    let host = headers.get("host")
        .and_then(|h| h.to_str().ok())
        .map(|h| h.to_string())
        .unwrap_or_else(|| format!("{}:{}", state.config.host, state.config.port));
    // Note: We need to ensure we expose uploads/updates via ServeDir in main.rs
    let download_url = format!("http://{}/api/files/download/updates/{}", host, update.filename);
    
    let mut count = 0;
    for client_id in payload.client_ids {
        if let Some(client) = state.clients.get(&client_id) {
             let cmd_id = Uuid::new_v4();
             let msg = Message::Command {
                id: cmd_id,
                cmd: CommandPayload::UpdateClient { url: download_url.clone() },
            };
            let _ = client.tx.send(msg).await;
            count += 1;
        }
    }
    
    (StatusCode::OK, format!("Update triggered for {} clients", count)).into_response()
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
    let ips: Vec<String>;

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
        Ok(Message::Register { client_id: id, token, hostname: h, os: o, alias: a, version: v, ips: i }) => {
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
            ips = i;
            
            info!("Client registered: {} ({}) - {} [Alias: {:?}] [IP: {}] [Ver: {}]", client_id, hostname, os, alias, addr, version);
            
            // Persist client to DB for history joins
            let client_id_str = client_id.to_string();
            let ip_str = addr.ip().to_string();
            let ips_json = serde_json::to_string(&ips).unwrap_or("[]".to_string());
            
            if let Err(e) = sqlx::query(
                "INSERT INTO clients (id, hostname, os, last_seen, status, alias, ip, ips, version) VALUES (?, ?, ?, CURRENT_TIMESTAMP, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET hostname = excluded.hostname, os = excluded.os, last_seen = CURRENT_TIMESTAMP, status = excluded.status, alias = excluded.alias, ip = excluded.ip, ips = excluded.ips, version = excluded.version"
            )
            .bind(&client_id_str)
            .bind(&hostname)
            .bind(&os)
            .bind("connected")
            .bind(&alias)
            .bind(&ip_str)
            .bind(&ips_json)
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
        ips: ips.clone(),
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
                                // Update last seen in DB
                                let client_id_str = client_id.to_string();
                                let _ = sqlx::query("UPDATE clients SET last_seen = CURRENT_TIMESTAMP WHERE id = ?")
                                    .bind(&client_id_str)
                                    .execute(&state.db).await;
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

// API: Auth
#[derive(serde::Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(serde::Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub username: String,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<LoginRequest>,
) -> impl IntoResponse {
    if !state.config.web_auth_enabled {
        return (StatusCode::OK, Json(LoginResponse {
            token: "auth-disabled".to_string(),
            username: "admin".to_string(),
        })).into_response();
    }

    // Verify password
    let row = sqlx::query("SELECT id, password_hash FROM web_users WHERE username = ?")
        .bind(&payload.username)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);

    if let Some(user) = row {
        let password_hash: String = user.get("password_hash");
        
        let mut hasher = Sha256::new();
        hasher.update(payload.password.as_bytes());
        let hash = hex::encode(hasher.finalize());

        if hash == password_hash {
            let token = Uuid::new_v4().to_string();
            state.web_sessions.insert(token.clone(), payload.username.clone());
            return (StatusCode::OK, Json(LoginResponse {
                token,
                username: payload.username,
            })).into_response();
        }
    }

    (StatusCode::UNAUTHORIZED, "Invalid credentials").into_response()
}

#[derive(serde::Deserialize)]
pub struct ChangePasswordRequest {
    pub old_password: String,
    pub new_password: String,
}

pub async fn change_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<ChangePasswordRequest>,
) -> impl IntoResponse {
    // Auth check
    let token = headers.get("Authorization")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.replace("Bearer ", ""))
        .unwrap_or_default();

    let username = if state.config.web_auth_enabled {
        if let Some(u) = state.web_sessions.get(&token) {
            u.value().clone()
        } else {
             return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        "admin".to_string()
    };

    // Verify old password
    let row = sqlx::query("SELECT password_hash FROM web_users WHERE username = ?")
        .bind(&username)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);
        
    if let Some(user) = row {
        let password_hash: String = user.get("password_hash");
        
        let mut hasher = Sha256::new();
        hasher.update(payload.old_password.as_bytes());
        let old_hash = hex::encode(hasher.finalize());
        
        if old_hash != password_hash {
             return (StatusCode::BAD_REQUEST, "Incorrect old password").into_response();
        }
        
        let mut hasher_new = Sha256::new();
        hasher_new.update(payload.new_password.as_bytes());
        let new_hash = hex::encode(hasher_new.finalize());
        
        if let Err(e) = sqlx::query("UPDATE web_users SET password_hash = ? WHERE username = ?")
            .bind(new_hash)
            .bind(username)
            .execute(&state.db).await {
                 return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to update password: {}", e)).into_response();
        }
        
        return (StatusCode::OK, "Password updated").into_response();
    }
    
    (StatusCode::BAD_REQUEST, "User not found").into_response()
}

#[derive(serde::Serialize)]
pub struct AuthStatus {
    pub enabled: bool,
    pub username: Option<String>,
}

pub async fn get_auth_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<AuthStatus> {
    let token = headers.get("Authorization")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.replace("Bearer ", ""))
        .unwrap_or_default();
        
    let username = if state.config.web_auth_enabled {
        state.web_sessions.get(&token).map(|u| u.value().clone())
    } else {
        Some("admin".to_string())
    };
    
    Json(AuthStatus {
        enabled: state.config.web_auth_enabled,
        username,
    })
}

pub async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let token = headers.get("Authorization")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.replace("Bearer ", ""))
        .unwrap_or_default();
        
    state.web_sessions.remove(&token);
    (StatusCode::OK, "Logged out").into_response()
}
