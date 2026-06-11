use axum::{
    extract::{ws::{Message as WsMessage, WebSocket, WebSocketUpgrade}, State, Json, Path, ConnectInfo, Multipart, Query},
    response::IntoResponse,
    http::{StatusCode, HeaderMap},
    body::Bytes,
};
use futures::{sink::SinkExt, stream::StreamExt};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;
use tracing::{info, error, warn};
use std::net::SocketAddr;
use std::time::Duration;
use sqlx::Row;
use sha2::{Sha256, Digest};
use hex;
use std::io::Write;
use flate2::read::{GzDecoder as GzDecoderRead, GzEncoder as GzEncoderRead};
use flate2::Compression;

use crate::state::{AppState, ClientConnection, ScriptGroup, ScriptStep, ExecutionProgress};
use common::{Message, CommandPayload, CommandResult, FileInfo};

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
            
        let scripts = sqlx::query("SELECT script_id FROM group_scripts WHERE group_id = ? ORDER BY sort_order ASC")
            .bind(group_id_str)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();

        let script_ids = scripts.into_iter()
            .map(|s| {
                let id: String = s.get("script_id");
                Uuid::parse_str(&id).unwrap_or_default()
            })
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
        for (idx, script_id) in script_ids.iter().enumerate() {
            let script_id_str = script_id.to_string();
            if let Err(e) = sqlx::query("INSERT INTO group_scripts (group_id, script_id, sort_order) VALUES (?, ?, ?)")
                .bind(&group_id_str)
                .bind(&script_id_str)
                .bind(idx as i32)
                .execute(&state.db).await {
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
    let scripts_rows = match sqlx::query("SELECT script_id FROM group_scripts WHERE group_id = ? ORDER BY sort_order ASC")
        .bind(group_id_str)
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
        let script_id_str: String = row.get("script_id");
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

    let scripts = Arc::new(scripts);

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
            for script in scripts_clone.iter().cloned() {
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

    (StatusCode::ACCEPTED, "Group execution started").into_response()
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

    let client_ids = payload.client_ids;
    let state_clone = state.clone();
    let script_clone = script.clone();
    let host_clone = host.clone();

    tokio::spawn(async move {
        // Create history and dispatch in the background to avoid request timeouts when many clients are selected.
        for client_id in client_ids {
            if !state_clone.clients.contains_key(&client_id) {
                continue;
            }

            let history_id = Uuid::new_v4();
            let history_id_str = history_id.to_string();
            let script_id_str_run = script_clone.id.to_string();
            let client_id_str = client_id.to_string();

            let now_utc = chrono::Utc::now();
            if let Err(e) = sqlx::query!(
                "INSERT INTO execution_history (id, script_id, client_id, status, started_at) VALUES (?, ?, ?, ?, ?)",
                history_id_str, script_id_str_run, client_id_str, "running", now_utc
            )
            .execute(&state_clone.db)
            .await
            {
                error!("Failed to create history record: {}", e);
                continue;
            }

            let state_task = state_clone.clone();
            let script_task = script_clone.clone();
            let host_task = host_clone.clone();
            tokio::spawn(async move {
                run_script_task(state_task, client_id, script_task, history_id, host_task).await;
            });
        }
    });

    (StatusCode::ACCEPTED, "Script execution started on selected clients").into_response()
}

use walkdir::WalkDir;
use zip::write::FileOptions;

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

fn expand_path(path: &str) -> String {
    if path == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    }
    if path.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        return format!("{}/{}", home.trim_end_matches('/'), &path[2..]);
    }
    path.to_string()
}

async fn execute_command_locally(cmd: CommandPayload) -> CommandResult {
    match cmd {
        CommandPayload::ShellExec { cmd, args } => {
            let full_cmd = if args.is_empty() {
                cmd
            } else {
                format!("{} {}", cmd, args.join(" "))
            };
            match tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&full_cmd)
                .output()
                .await
            {
                Ok(output) => CommandResult::ShellOutput {
                    stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                    exit_code: output.status.code().unwrap_or(-1),
                    cwd: std::env::current_dir().map(|d| d.to_string_lossy().to_string()).unwrap_or_default(),
                },
                Err(e) => CommandResult::Error(format!("Shell execution failed: {}", e)),
            }
        }
        CommandPayload::CopyFile { src_path, dest_path } => {
            fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
                if !dst.exists() { std::fs::create_dir_all(dst)?; }
                for entry in std::fs::read_dir(src)? {
                    let entry = entry?;
                    let ty = entry.file_type()?;
                    let sp = entry.path();
                    let dp = dst.join(entry.file_name());
                    if ty.is_dir() { copy_dir(&sp, &dp)?; }
                    else { std::fs::copy(&sp, &dp)?; }
                }
                Ok(())
            }
            let src = std::path::Path::new(&src_path);
            let dst = std::path::Path::new(&dest_path);
            if src.is_dir() {
                match copy_dir(src, dst) {
                    Ok(_) => CommandResult::Success(format!("Directory copied from {} to {}", src_path, dest_path)),
                    Err(e) => CommandResult::Error(format!("Failed to copy directory: {}", e)),
                }
            } else {
                match std::fs::copy(src, dst) {
                    Ok(_) => CommandResult::Success(format!("File copied from {} to {}", src_path, dest_path)),
                    Err(e) => CommandResult::Error(format!("Failed to copy file: {}", e)),
                }
            }
        }
        CommandPayload::MoveFile { src_path, dest_path } => {
            match std::fs::rename(&src_path, &dest_path) {
                Ok(_) => CommandResult::Success(format!("Moved from {} to {}", src_path, dest_path)),
                Err(e) => CommandResult::Error(format!("Failed to move: {}", e)),
            }
        }
        CommandPayload::DeleteFile { path } => {
            let p = std::path::Path::new(&path);
            if p.is_dir() {
                match std::fs::remove_dir_all(p) {
                    Ok(_) => CommandResult::Success(format!("Directory deleted: {}", path)),
                    Err(e) => CommandResult::Error(format!("Failed to delete directory: {}", e)),
                }
            } else {
                match std::fs::remove_file(p) {
                    Ok(_) => CommandResult::Success(format!("File deleted: {}", path)),
                    Err(e) => CommandResult::Error(format!("Failed to delete file: {}", e)),
                }
            }
        }
        CommandPayload::HttpRequest { method, url, headers, query_params, body } => {
            let client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
            {
                Ok(c) => c,
                Err(e) => return CommandResult::Error(format!("Failed to build HTTP client: {}", e)),
            };
            let http_method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
            let mut req = client.request(http_method, &url);
            for h in &headers { req = req.header(&h.key, &h.value); }
            for qp in &query_params { req = req.query(&[(qp.key.clone(), qp.value.clone())]); }
            if let Some(b) = &body { req = req.body(b.clone()); }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let body_text = resp.text().await.unwrap_or_default();
                    CommandResult::Success(format!("HTTP {} {}\n\nStatus: {}\n\nBody:\n{}", method, url, status, body_text))
                }
                Err(e) => CommandResult::Error(format!("HTTP request failed: {}", e)),
            }
        }
        CommandPayload::ReadFile { path } => {
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => CommandResult::FileContent { content },
                Err(e) => CommandResult::Error(format!("Failed to read file: {}", e)),
            }
        }
        CommandPayload::WriteFile { path, content } => {
            match tokio::fs::write(&path, &content).await {
                Ok(_) => CommandResult::Success("File saved successfully".to_string()),
                Err(e) => CommandResult::Error(format!("Failed to write file: {}", e)),
            }
        }
        CommandPayload::ListDir { path } => {
            match std::fs::read_dir(&path) {
                Ok(entries) => {
                    let mut files = Vec::new();
                    for entry in entries.flatten() {
                        let metadata = entry.metadata().ok();
                        let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                        let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
                        let modified = metadata.as_ref().and_then(|m| m.modified().ok())
                            .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs());
                        files.push(FileInfo {
                            name: entry.file_name().to_string_lossy().to_string(),
                            is_dir, size, modified,
                        });
                    }
                    CommandResult::FileList { files }
                }
                Err(e) => CommandResult::Error(format!("Failed to read dir: {}", e)),
            }
        }
        CommandPayload::ChangeDir { path } => {
            match std::env::set_current_dir(&path) {
                Ok(_) => CommandResult::DirChanged { new_path: path },
                Err(e) => CommandResult::Error(format!("Failed to change dir: {}", e)),
            }
        }
        _ => CommandResult::Error("This command type is not supported for server-side execution".to_string()),
    }
}

pub(crate) async fn run_script_task(state: Arc<AppState>, client_id: Uuid, script: ScriptGroup, history_id: Uuid, server_host: String) {
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
        
        let base_url = get_download_base_url(&state, Some(client_id), Some(&server_host));

        let mut pending_server_save: Option<(Uuid, String)> = None;
        let mut step_temp_files: Vec<String> = Vec::new();

        let cmd_payload_result = match step {
            ScriptStep::Shell { cmd, args, .. } => Ok(CommandPayload::ShellExec { cmd: cmd.clone(), args: args.clone() }),
            ScriptStep::Upload { local_path, remote_path, local_path_is_absolute, compress, .. } => {
                let should_compress = compress.unwrap_or(false);
                if local_path_is_absolute.unwrap_or(false) {
                    let expanded = expand_path(local_path);
                    let file_name = std::path::Path::new(&expanded).file_name().unwrap_or_default().to_string_lossy();
                    let staging_name = format!("absolute_{}_{}", Uuid::new_v4(), file_name);
                    let staging_path = format!("uploads/staging/{}", staging_name);
                    match tokio::fs::copy(&expanded, &staging_path).await {
                        Ok(_) => {
                            // Gzip the staging file for faster client download (if compress enabled and file exceeds threshold)
                            let should_gzip = should_compress && {
                                std::fs::metadata(&staging_path).map(|m| m.len()).unwrap_or(0) >= state.config.compress_threshold
                            };
                            if should_gzip {
                                if let Some(gz_path) = try_gzip_staging_file(&staging_path).await {
                                    step_temp_files.push(gz_path);
                                    let gz_name = format!("{}.gz", staging_name);
                                    let download_url = format!("{}/api/files/download/staging/{}", base_url, gz_name);
                                    Ok(CommandPayload::DownloadFile { url: download_url, dest_path: remote_path.clone() })
                                } else {
                                    let download_url = format!("{}/api/files/download/staging/{}", base_url, staging_name);
                                    Ok(CommandPayload::DownloadFile { url: download_url, dest_path: remote_path.clone() })
                                }
                            } else {
                                let download_url = format!("{}/api/files/download/staging/{}", base_url, staging_name);
                                Ok(CommandPayload::DownloadFile { url: download_url, dest_path: remote_path.clone() })
                            }
                        },
                        Err(e) => Err(format!("Failed to copy absolute path file: {}", e))
                    }
                } else {
                    let staging_path = format!("uploads/staging/{}", local_path);
                    // Gzip the staging file for faster client download (if compress enabled and file exceeds threshold)
                    let should_gzip = should_compress && {
                        std::fs::metadata(&staging_path).map(|m| m.len()).unwrap_or(0) >= state.config.compress_threshold
                    };
                    if should_gzip {
                        if let Some(gz_path) = try_gzip_staging_file(&staging_path).await {
                            step_temp_files.push(gz_path);
                            let gz_name = format!("{}.gz", local_path);
                            let download_url = format!("{}/api/files/download/staging/{}", base_url, gz_name);
                            Ok(CommandPayload::DownloadFile { url: download_url, dest_path: remote_path.clone() })
                        } else {
                            let download_url = format!("{}/api/files/download/staging/{}", base_url, local_path);
                            Ok(CommandPayload::DownloadFile { url: download_url, dest_path: remote_path.clone() })
                        }
                    } else {
                        let download_url = format!("{}/api/files/download/staging/{}", base_url, local_path);
                        Ok(CommandPayload::DownloadFile { url: download_url, dest_path: remote_path.clone() })
                    }
                }
            },
            ScriptStep::Download { remote_path, browser_download, server_save_path, compress, .. } => {
                let upload_id = Uuid::new_v4();
                let upload_url = format!("{}/api/files/client-upload/{}", base_url, upload_id);

                if let Some(save_path) = server_save_path {
                    if !save_path.is_empty() {
                        pending_server_save = Some((upload_id, save_path.clone()));
                    }
                }

                if browser_download.unwrap_or(false) {
                    let file_name = std::path::Path::new(remote_path).file_name().unwrap_or_default().to_string_lossy();
                    let download_link = format!("{}/api/files/download/client_data/{}/{}", base_url, upload_id, file_name);
                    let log_msg = format!("BROWSER_DOWNLOAD: {}", download_link);
                    logs.push(log_msg.clone());
                    if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                        progress.logs.push(log_msg);
                    }
                }

                Ok(CommandPayload::UploadFile { src_path: remote_path.clone(), upload_url, compress: *compress })
            },
            ScriptStep::UploadDir { local_path, remote_path, local_path_is_absolute, .. } => {
                let (src_dir, zip_name) = if local_path_is_absolute.unwrap_or(false) {
                    // Absolute path — zip directly from the filesystem
                    let expanded = expand_path(local_path);
                    let name = std::path::Path::new(&expanded).file_name().unwrap_or_default().to_string_lossy();
                    let safe_name = format!("absolute_dir_{}_{}.zip", Uuid::new_v4(), name);
                    (expanded, safe_name)
                } else {
                    let safe_name = format!("{}.zip", local_path);
                    (format!("uploads/staging/{}", local_path), safe_name)
                };
                let dst_zip = format!("uploads/staging/{}", zip_name);

                match zip_directory(&src_dir, &dst_zip) {
                    Ok(_) => {
                        let download_url = format!("{}/api/files/download/staging/{}", base_url, zip_name);
                        Ok(CommandPayload::DownloadAndUnzip { url: download_url, dest_path: remote_path.clone() })
                    },
                    Err(e) => Err(format!("Failed to zip directory: {}", e))
                }
            },
            ScriptStep::DownloadDir { remote_path, browser_download, server_save_path, .. } => {
                let upload_id = Uuid::new_v4();
                // Client will upload a zip file, server receives it as generic file upload
                let upload_url = format!("{}/api/files/client-upload/{}", base_url, upload_id);

                if let Some(save_path) = server_save_path {
                    if !save_path.is_empty() {
                        pending_server_save = Some((upload_id, save_path.clone()));
                    }
                }
                
                if browser_download.unwrap_or(false) {
                    let file_name = format!("{}.zip", std::path::Path::new(remote_path).file_name().unwrap_or_default().to_string_lossy());
                    let download_link = format!("{}/api/files/download/client_data/{}/{}", base_url, upload_id, file_name);
                    let log_msg = format!("BROWSER_DOWNLOAD: {}", download_link);
                    logs.push(log_msg.clone());
                    if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                        progress.logs.push(log_msg);
                    }
                }

                Ok(CommandPayload::ZipAndUpload { src_path: remote_path.clone(), upload_url })
            },
            ScriptStep::Copy { src_path, dest_path, .. } => {
                Ok(CommandPayload::CopyFile { src_path: src_path.clone(), dest_path: dest_path.clone() })
            },
            ScriptStep::Move { src_path, dest_path, .. } => {
                Ok(CommandPayload::MoveFile { src_path: src_path.clone(), dest_path: dest_path.clone() })
            },
            ScriptStep::Delete { path, .. } => {
                Ok(CommandPayload::DeleteFile { path: path.clone() })
            }
            ScriptStep::HttpRequest { url, method, headers, query_params, body, .. } => {
                Ok(CommandPayload::HttpRequest {
                    url: url.clone(),
                    method: method.clone().unwrap_or_else(|| "GET".to_string()),
                    headers: headers.clone().unwrap_or_default(),
                    query_params: query_params.clone().unwrap_or_default(),
                    body: body.clone(),
                })
            }
        };

        let step_desc = match step {
            ScriptStep::Shell { cmd, args, .. } => format!("Shell: {} {}", cmd, args.join(" ")),
            ScriptStep::Upload { local_path, remote_path, .. } => format!("Upload: {} -> {}", local_path, remote_path),
            ScriptStep::Download { remote_path, .. } => format!("Download: {}", remote_path),
            ScriptStep::UploadDir { local_path, remote_path, .. } => format!("UploadDir: {} -> {}", local_path, remote_path),
            ScriptStep::DownloadDir { remote_path, .. } => format!("DownloadDir: {}", remote_path),
            ScriptStep::Copy { src_path, dest_path, .. } => format!("Copy: {} -> {}", src_path, dest_path),
            ScriptStep::Move { src_path, dest_path, .. } => format!("Move: {} -> {}", src_path, dest_path),
            ScriptStep::Delete { path, .. } => format!("Delete: {}", path),
            ScriptStep::HttpRequest { url, method, .. } => format!("HttpRequest: {} {}", method.clone().unwrap_or_else(|| "GET".to_string()), url),
        };

        let step_desc = if step.is_run_on_server() {
            format!("{} [Server]", step_desc)
        } else {
            step_desc
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

        // Server-side execution
        if step.is_run_on_server() {
            let result = execute_command_locally(cmd_payload).await;
            let mut step_success = false;
            let log_res = match result {
                CommandResult::Error(e) => format!("Step {}: Failed: {}", i + 1, e),
                CommandResult::ShellOutput { stdout, stderr, exit_code, .. } => {
                    if exit_code != 0 {
                        format!("Step {}: Shell command failed (Exit Code: {}). Stderr: {}", i + 1, exit_code, stderr)
                    } else {
                        step_success = true;
                        format!("Step {}: Completed. Output: {}", i + 1, stdout)
                    }
                }
                res => {
                    step_success = true;
                    format!("Step {}: Completed. Result: {:?}", i + 1, res)
                }
            };
            logs.push(log_res.clone());
            if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                progress.logs.push(log_res);
            }

            // Handle server_save_path for Download/DownloadDir steps
            if step_success {
                if let Some((upload_id, save_path)) = pending_server_save.take() {
                    let client_data_dir = format!("uploads/client_data/{}", upload_id);
                    if let Ok(mut entries) = tokio::fs::read_dir(&client_data_dir).await {
                        if let Ok(Some(entry)) = entries.next_entry().await {
                            let src = entry.path();
                            let dst = std::path::Path::new(&save_path);
                            if let Some(parent) = dst.parent() {
                                let _ = tokio::fs::create_dir_all(parent).await;
                            }
                            match tokio::fs::copy(&src, dst).await {
                                Ok(_) => {
                                    let log_save = format!("Step {}: File saved to server path: {}", i + 1, save_path);
                                    logs.push(log_save.clone());
                                    if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                                        progress.logs.push(log_save);
                                    }
                                }
                                Err(e) => {
                                    let log_save = format!("Step {}: Failed to save to server path {}: {}", i + 1, save_path, e);
                                    logs.push(log_save.clone());
                                    if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                                        progress.logs.push(log_save);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Clean up temp files created for this step (e.g. gzip staging)
            for path in &step_temp_files {
                let _ = tokio::fs::remove_file(path).await;
            }

            if !step_success {
                success = false;
                break;
            }
            continue;
        }

        // Send command
        if let Some(client) = state.clients.get(&client_id) {
            let cmd_id = Uuid::new_v4();
            let (wait_tx, wait_rx) = tokio::sync::oneshot::channel();
            state.waiters.insert(cmd_id, wait_tx);
            state.cmd_history_map.insert(cmd_id, history_id);

            let is_file_transfer = matches!(cmd_payload, CommandPayload::UploadFile { .. } | CommandPayload::DownloadFile { .. });
            let step_timeout = if is_file_transfer { 3600 } else { 300 };

            let msg = Message::Command {
                id: cmd_id,
                cmd: cmd_payload,
            };

            if let Err(e) = client.tx.send(msg).await {
                state.waiters.remove(&cmd_id);
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

            // Log start of file transfers
            if is_file_transfer {
                let log_start = format!("Step {}: File transfer starting (timeout: {}s)", i + 1, step_timeout);
                logs.push(log_start.clone());
                if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                    progress.logs.push(log_start);
                }
            }

            let result = match tokio::time::timeout(tokio::time::Duration::from_secs(step_timeout), wait_rx).await {
                Ok(Ok(r)) => Some(r),
                Ok(Err(_)) => None,
                Err(_) => {
                    state.waiters.remove(&cmd_id);
                    None
                }
            };

            if let Some(result) = result {
                let log_res = match result {
                    CommandResult::Error(e) => {
                        format!("Step {}: Failed: {}", i + 1, e)
                    }
                    CommandResult::ShellOutput { stdout, stderr, exit_code, .. } => {
                        if exit_code != 0 {
                            format!("Step {}: Shell command failed (Exit Code: {}). Stderr: {}", i + 1, exit_code, stderr)
                        } else {
                            step_success = true;
                            format!("Step {}: Completed. Output: {}", i + 1, stdout)
                        }
                    }
                    res => {
                        step_success = true;
                        format!("Step {}: Completed. Result: {:?}", i + 1, res)
                    }
                };

                logs.push(log_res.clone());
                if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                    progress.logs.push(log_res);
                }
            }
            
            // Handle server_save_path for client-executed Download/DownloadDir steps
            if step_success {
                if let Some((upload_id, save_path)) = pending_server_save.take() {
                    let client_data_dir = format!("uploads/client_data/{}", upload_id);
                    if let Ok(mut entries) = tokio::fs::read_dir(&client_data_dir).await {
                        if let Ok(Some(entry)) = entries.next_entry().await {
                            let src = entry.path();
                            let dst = std::path::Path::new(&save_path);
                            if let Some(parent) = dst.parent() {
                                let _ = tokio::fs::create_dir_all(parent).await;
                            }
                            match tokio::fs::copy(&src, dst).await {
                                Ok(_) => {
                                    let log_save = format!("Step {}: File saved to server path: {}", i + 1, save_path);
                                    logs.push(log_save.clone());
                                    if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                                        progress.logs.push(log_save);
                                    }
                                }
                                Err(e) => {
                                    let log_save = format!("Step {}: Failed to save to server path {}: {}", i + 1, save_path, e);
                                    logs.push(log_save.clone());
                                    if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                                        progress.logs.push(log_save);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if !step_success {
                let log_timeout = format!("Step {}: Timed out or failed", i + 1);
                logs.push(log_timeout.clone());
                if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                    progress.logs.push(log_timeout);
                }
                success = false;
                for path in &step_temp_files {
                    let _ = tokio::fs::remove_file(path).await;
                }
                break;
            }

        } else {
            let log_disc = "Client disconnected".to_string();
            logs.push(log_disc.clone());
            if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                progress.logs.push(log_disc);
            }
            success = false;
            for path in &step_temp_files {
                let _ = tokio::fs::remove_file(path).await;
            }
            break;
        }

        for path in &step_temp_files {
            let _ = tokio::fs::remove_file(path).await;
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
    pub script_id: Uuid,
    pub client_id: Uuid,
    pub script_name: String,
    pub client_hostname: String,
    pub client_alias: Option<String>,
    pub status: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub logs: Vec<String>,
    pub scheduled_task_id: Option<String>,
}

#[derive(serde::Serialize)]
pub struct PaginatedHistory {
    pub history: Vec<ExecutionHistoryItem>,
    pub total: i64,
}

#[derive(serde::Deserialize)]
pub struct HistoryParams {
    pub page: Option<i64>,
    pub limit: Option<i64>,
    pub scheduled_task_id: Option<String>,
}

pub async fn get_script_history(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HistoryParams>,
) -> Json<PaginatedHistory> {
    let page = params.page.unwrap_or(1).max(1);
    let limit = params.limit.unwrap_or(50).max(1);
    let offset = (page - 1) * limit;

    let has_filter = params.scheduled_task_id.is_some();

    let total = if let Some(ref st_id) = params.scheduled_task_id {
        sqlx::query("SELECT COUNT(*) as count FROM execution_history WHERE scheduled_task_id = ?")
            .bind(st_id)
            .fetch_one(&state.db)
            .await
            .map(|r| r.try_get::<i64, _>("count").unwrap_or(0))
            .unwrap_or(0)
    } else {
        sqlx::query("SELECT COUNT(*) as count FROM execution_history")
            .fetch_one(&state.db)
            .await
            .map(|r| r.try_get::<i64, _>("count").unwrap_or(0))
            .unwrap_or(0)
    };

    let rows = if has_filter {
        let st_id = params.scheduled_task_id.as_ref().unwrap();
        sqlx::query(
            r#"
            SELECT h.id, h.script_id, h.client_id, s.name as script_name, c.hostname as client_hostname, c.alias as client_alias, h.status, CAST(h.started_at AS TEXT) as started_at, CAST(h.completed_at AS TEXT) as completed_at, h.logs, h.scheduled_task_id
            FROM execution_history h
            JOIN scripts s ON h.script_id = s.id
            LEFT JOIN clients c ON h.client_id = c.id
            WHERE h.scheduled_task_id = ?
            ORDER BY h.started_at DESC
            LIMIT ? OFFSET ?
            "#)
        .bind(st_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
    } else {
        sqlx::query(
            r#"
            SELECT h.id, h.script_id, h.client_id, s.name as script_name, c.hostname as client_hostname, c.alias as client_alias, h.status, CAST(h.started_at AS TEXT) as started_at, CAST(h.completed_at AS TEXT) as completed_at, h.logs, h.scheduled_task_id
            FROM execution_history h
            JOIN scripts s ON h.script_id = s.id
            LEFT JOIN clients c ON h.client_id = c.id
            ORDER BY h.started_at DESC
            LIMIT ? OFFSET ?
            "#)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
    };

    let history: Vec<ExecutionHistoryItem> = rows.into_iter().map(|r| {
        let logs: Vec<String> = r.get::<Option<String>, _>("logs").as_deref().and_then(|l| serde_json::from_str(l).ok()).unwrap_or_default();
        ExecutionHistoryItem {
            id: Uuid::parse_str(r.get::<Option<String>, _>("id").as_deref().unwrap_or("")).unwrap_or_default(),
            script_id: Uuid::parse_str(r.get::<Option<String>, _>("script_id").as_deref().unwrap_or("")).unwrap_or_default(),
            client_id: Uuid::parse_str(r.get::<Option<String>, _>("client_id").as_deref().unwrap_or("")).unwrap_or_default(),
            script_name: r.get("script_name"),
            client_hostname: r.get::<Option<String>, _>("client_hostname").unwrap_or("Unknown".to_string()),
            client_alias: r.get("client_alias"),
            status: r.get("status"),
            started_at: r.get::<Option<String>, _>("started_at").unwrap_or_default(),
            completed_at: r.get("completed_at"),
            logs,
            scheduled_task_id: r.get("scheduled_task_id"),
        }
    }).collect();
    
    Json(PaginatedHistory { history, total })
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
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut multipart: Multipart
) -> impl IntoResponse {
    let field = match multipart.next_field().await.unwrap_or(None) {
        Some(f) => f,
        None => return (StatusCode::BAD_REQUEST, "No file provided").into_response(),
    };

    let file_name = field
        .file_name()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "uploaded_file".to_string());
    let data = match field.bytes().await {
        Ok(d) => d,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read bytes: {}", e)).into_response(),
    };

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

    let host_header = headers.get("host").and_then(|h| h.to_str().ok());
    let base_url = get_download_base_url(&state, None, host_header);
    let url = format!("{}/api/files/download/staging/{}", base_url, file_name);

    (StatusCode::OK, Json(serde_json::json!({ "url": url }))).into_response()
}

// API: Client uploads file (Result of UploadFile command)
pub async fn upload_file_client(
    Path(id): Path<Uuid>, // Command ID
    mut multipart: Multipart
) -> impl IntoResponse {
    let field = match multipart.next_field().await.unwrap_or(None) {
        Some(f) => f,
        None => return (StatusCode::BAD_REQUEST, "No file provided").into_response(),
    };

    let file_name = field
        .file_name()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "client_upload".to_string());
    let data = match field.bytes().await {
        Ok(d) => d,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read bytes: {}", e)).into_response(),
    };

    let dir_path = format!("uploads/client_data/{}", id);
    let _ = tokio::fs::create_dir_all(&dir_path).await;
    
    let file_path = format!("{}/{}", dir_path, file_name);
    if let Err(e) = tokio::fs::write(&file_path, &data).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write file: {}", e)).into_response();
    }
    
    info!("File uploaded by client for command {}: {}", id, file_path);

    // Decompress if gzip-compressed (synchronous to avoid races)
    if let Err(e) = maybe_decompress_gzip(&file_path).await {
        warn!("Failed to decompress uploaded file {}: {}", file_path, e);
    }

    (StatusCode::OK, "Upload successful").into_response()
}

/// Check if a file starts with gzip magic bytes and decompress it in-place.
async fn maybe_decompress_gzip(file_path: &str) -> Result<(), String> {
    use tokio::io::AsyncReadExt;

    // Read only the first 2 bytes to check gzip magic
    let mut file = match tokio::fs::File::open(file_path).await {
        Ok(f) => f,
        Err(_) => return Ok(()),
    };
    let mut magic = [0u8; 2];
    if file.read_exact(&mut magic).await.is_err() || magic != [0x1f, 0x8b] {
        return Ok(()); // Not gzip
    }
    drop(file);

    info!("Decompressing gzip file: {}", file_path);

    let path = file_path.to_string();
    let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let gz_file = std::fs::File::open(&path).map_err(|e| format!("Failed to open gzip: {}", e))?;
        let temp_path = format!("{}.decompressing", &path);
        let mut out_file = std::fs::File::create(&temp_path).map_err(|e| format!("Failed to create temp: {}", e))?;
        let mut decoder = GzDecoderRead::new(gz_file);
        std::io::copy(&mut decoder, &mut out_file).map_err(|e| format!("Decompression failed: {}", e))?;
        drop(out_file);
        std::fs::rename(&temp_path, &path).map_err(|e| format!("Failed to replace with decompressed: {}", e))?;
        Ok(())
    }).await;

    match result {
        Ok(Ok(())) => {
            info!("Decompressed: {}", file_path);
            Ok(())
        },
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("Join error: {}", e)),
    }
}

/// Gzip a staging file for faster client download. Returns the .gz path on success.
async fn try_gzip_staging_file(staging_path: &str) -> Option<String> {
    let gz_path = format!("{}.gz", staging_path);
    let staging = staging_path.to_string();
    let gz = gz_path.clone();

    let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let src = std::fs::File::open(&staging)?;
        let mut encoder = GzEncoderRead::new(std::io::BufReader::new(src), Compression::default());
        let mut dst = std::fs::File::create(&gz)?;
        std::io::copy(&mut encoder, &mut dst)?;
        dst.flush()?;
        Ok(())
    }).await;

    match result {
        Ok(Ok(())) => Some(gz_path),
        _ => None,
    }
}

// API: Chunked Upload - Receive a single chunk
pub async fn upload_chunk(
    Path((cmd_id, chunk_index)): Path<(Uuid, usize)>,
    body: Bytes,
) -> impl IntoResponse {
    let dir_path = format!("uploads/chunked/{}", cmd_id);
    if let Err(e) = tokio::fs::create_dir_all(&dir_path).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create chunk dir: {}", e)).into_response();
    }

    let chunk_path = format!("{}/{}", dir_path, chunk_index);
    if let Err(e) = tokio::fs::write(&chunk_path, &body).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write chunk: {}", e)).into_response();
    }

    info!("Chunk {} received for command {}", chunk_index, cmd_id);
    (StatusCode::OK, "Chunk uploaded").into_response()
}

#[derive(serde::Deserialize)]
pub struct CompleteChunkedUpload {
    pub filename: String,
    pub total_chunks: usize,
}

// API: Chunked Upload - Complete and assemble chunks into final file
pub async fn complete_chunked_upload(
    Path(cmd_id): Path<Uuid>,
    Json(payload): Json<CompleteChunkedUpload>,
) -> impl IntoResponse {
    let chunk_dir = format!("uploads/chunked/{}", cmd_id);
    let out_dir = format!("uploads/client_data/{}", cmd_id);
    let out_path = format!("{}/{}", out_dir, payload.filename);

    // Create output directory
    if let Err(e) = tokio::fs::create_dir_all(&out_dir).await {
        warn!("Failed to create output dir for {}: {}", cmd_id, e);
        let _ = tokio::fs::remove_dir_all(&chunk_dir).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create output dir: {}", e)).into_response();
    }

    // Open output file for writing
    let mut out_file = match tokio::fs::File::create(&out_path).await {
        Ok(f) => f,
        Err(e) => {
            warn!("Failed to create output file for {}: {}", cmd_id, e);
            let _ = tokio::fs::remove_dir_all(&chunk_dir).await;
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create output file: {}", e)).into_response();
        }
    };

    // Read and concatenate chunks in order
    for i in 0..payload.total_chunks {
        let chunk_path = format!("{}/{}", chunk_dir, i);
        match tokio::fs::read(&chunk_path).await {
            Ok(data) => {
                if let Err(e) = out_file.write_all(&data).await {
                    warn!("Failed to write chunk {} for {}: {}", i, cmd_id, e);
                    let _ = tokio::fs::remove_dir_all(&chunk_dir).await;
                    return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write chunk {}: {}", i, e)).into_response();
                }
            }
            Err(e) => {
                warn!("Failed to read chunk {} for {}: {}", i, cmd_id, e);
                let _ = tokio::fs::remove_dir_all(&chunk_dir).await;
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read chunk {}: {}", i, e)).into_response();
            }
        }
    }

    // Flush and close output file
    if let Err(e) = out_file.flush().await {
        warn!("Failed to flush output for {}: {}", cmd_id, e);
    }
    drop(out_file);

    // Decompress if gzip-compressed (synchronous to avoid races)
    if let Err(e) = maybe_decompress_gzip(&out_path).await {
        warn!("Failed to decompress chunked upload {} ({}): {}", cmd_id, out_path, e);
    }

    // Cleanup chunk directory (log error on failure instead of silently ignoring)
    if let Err(e) = tokio::fs::remove_dir_all(&chunk_dir).await {
        warn!("Failed to cleanup chunk directory for {}: {}", cmd_id, e);
    }

    info!("Chunked upload completed for command {}: {}", cmd_id, out_path);
    (StatusCode::OK, "Upload completed successfully").into_response()
}

// API: Download file (Generic)
// Serves files from staging or client_data
// path_type: "staging" or "client_data"
// id_or_file: filename (for staging) or uuid/filename (for client_data)
// Since Axum path matching is simple, we can make two routes or one flexible one.
// Let's rely on ServeDir for this! It's much easier and supports ranges, etc.
// We will configure ServeDir in main.rs to serve server/uploads under /api/files/download/


// API: List connected clients
#[derive(serde::Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub page: Option<i64>,
    pub limit: Option<i64>,
    pub status: Option<String>,
}

#[derive(serde::Serialize)]
pub struct PaginatedClients {
    pub clients: Vec<ClientSummary>,
    pub total: i64,
    pub online_count: i64,
    pub offline_count: i64,
}

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
    pub started_at: Option<String>,
    pub remark: Option<String>,
    pub working_directory: Option<String>,
    pub display_ip: Option<String>,
}

pub async fn list_clients(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>
) -> Json<PaginatedClients> {
    let q_param = params.q.as_deref().unwrap_or("").to_lowercase();
    let page = params.page.unwrap_or(1).max(1);
    let limit = params.limit.unwrap_or(50).max(1);
    let offset = (page - 1) * limit;

    // Build WHERE clause
    let mut where_clause = "1=1".to_string();
    if !q_param.is_empty() {
        where_clause.push_str(" AND (lower(hostname) LIKE ? OR lower(alias) LIKE ? OR ip LIKE ? OR ips LIKE ?)");
    }
    
    // Status filter
    if let Some(status) = &params.status {
        if status == "online" {
            where_clause.push_str(" AND status = 'connected'");
        } else if status == "offline" {
             where_clause.push_str(" AND status != 'connected'");
        }
    }

    // Get Total Count and Offline Count
    // Note: Online/Offline status is dynamic in memory (state.clients), but we persist 'status' in DB too.
    // However, DB status might be stale if server crashed. 
    // Ideally, we should sync memory status to DB periodically or on connect/disconnect.
    // We already do update status on connect/disconnect.
    // So DB status should be mostly accurate for "offline" clients.
    // For online clients, they are in DB as 'connected'.
    
    // Let's rely on DB for total count and filtering, but enrich with memory state.
    
    let total: i64 = if !q_param.is_empty() {
        let q_like = format!("%{}%", q_param);
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM clients WHERE {}", where_clause))
            .bind(&q_like)
            .bind(&q_like)
            .bind(&q_like)
            .bind(&q_like)
            .fetch_one(&state.db)
            .await
            .unwrap_or(0)
    } else if params.status.is_some() {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM clients WHERE {}", where_clause))
            .fetch_one(&state.db)
            .await
            .unwrap_or(0)
    } else {
        sqlx::query_scalar("SELECT COUNT(*) FROM clients")
            .fetch_one(&state.db)
            .await
            .unwrap_or(0)
    };

    // For total offline count, we want global count regardless of filter
    let global_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM clients").fetch_one(&state.db).await.unwrap_or(0);
    let online_count = state.clients.len() as i64;
    let offline_count = if global_total >= online_count { global_total - online_count } else { 0 };
    
    // Note: 'total' returned in PaginatedClients is the filtered total for pagination.
    // 'online_count' and 'offline_count' are global stats for the dashboard counters.
    // Better approximation: count where status != 'connected' in DB? 
    // But 'connected' in DB might be stale if server restarted. 
    // On server start, we should probably reset all 'connected' to 'disconnected'.
    // Assuming we did that or don't care about extreme precision:
    // Memory is truth for Online. 
    // DB Total - Memory Online = Offline.
    
    // Query Data
    // Sort by status ASC (connected < disconnected) to put online clients first, then by hostname ASC for stability
    // Note: status in DB is 'connected' or 'disconnected'.
    // If we want 'connected' first, 'connected' < 'disconnected', so ASC is correct.
    let query = format!(
        "SELECT id, hostname, os, alias, ip, ips, version, status, last_seen, started_at, remark, working_directory, display_ip 
         FROM clients 
         WHERE {} 
         ORDER BY status ASC, hostname ASC 
         LIMIT ? OFFSET ?", 
        where_clause
    );

    let rows = if !q_param.is_empty() {
        let q_like = format!("%{}%", q_param);
        sqlx::query(&query)
            .bind(&q_like)
            .bind(&q_like)
            .bind(&q_like)
            .bind(&q_like)
            .bind(limit)
            .bind(offset)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default()
    } else {
        sqlx::query(&query)
            .bind(limit)
            .bind(offset)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default()
    };

    let mut clients: Vec<ClientSummary> = rows.into_iter().map(|r| {
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
        let db_started_at: Option<chrono::NaiveDateTime> = r.get("started_at");
        let db_remark: Option<String> = r.get("remark");
        let db_working_directory: Option<String> = r.get("working_directory");
        let db_display_ip: Option<String> = r.get("display_ip");
        
        let last_seen = db_last_seen.map(|d| format!("{}Z", d.format("%Y-%m-%dT%H:%M:%S")));
        let parsed_db_ips: Vec<String> = db_ips.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();

        let (hostname, os, alias, ip, ips, version, status, started_at) = if is_connected {
            if let Some(conn) = state.clients.get(&id) {
                (
                    conn.hostname.clone(),
                    conn.os.clone(),
                    conn.alias.clone(),
                    conn.ip.clone(),
                    conn.ips.clone(),
                    conn.version.clone(),
                    "online".to_string(),
                    conn.started_at.map(|d| format!("{}", d.format("%Y-%m-%dT%H:%M:%SZ")))
                )
            } else {
                (
                    db_hostname,
                    db_os,
                    db_alias,
                    db_ip.unwrap_or_default(),
                    parsed_db_ips,
                    db_version.unwrap_or_default(),
                    "online".to_string(),
                    db_started_at.map(|d| format!("{}Z", d.format("%Y-%m-%dT%H:%M:%S")))
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
                "offline".to_string(),
                db_started_at.map(|d| format!("{}Z", d.format("%Y-%m-%dT%H:%M:%S")))
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
            started_at,
            remark: db_remark,
            working_directory: db_working_directory,
            display_ip: db_display_ip,
        }
    }).collect();

    // Sort: Online first, then Hostname ASC (Applied to current page only, which is acceptable but not perfect. 
    // Ideally we sort in DB, but 'online' status is in memory.
    // For strict sorting, we'd need to sync status to DB perfectly or fetch all IDs and paginate in memory (bad for scalability).
    // Given the hybrid nature, sorting the page is a reasonable compromise, or we accept DB sorting by last_seen.)
    // Current DB query sorts by last_seen DESC.
    // We can re-sort the page in memory.
    
    clients.sort_by(|a, b| {
        let status_order = |s: &str| match s {
            "online" => 0,
            _ => 1,
        };
        let sa = status_order(&a.status);
        let sb = status_order(&b.status);
        if sa != sb {
            return sa.cmp(&sb);
        }
        // If status same, keep DB order (implied by stability) or sort by hostname
        a.hostname.cmp(&b.hostname)
    });

    Json(PaginatedClients {
        clients,
        total,
        online_count,
        offline_count,
    })
}

// API: Update Client Remark
#[derive(serde::Deserialize)]
pub struct UpdateClientRemarkRequest {
    pub remark: String,
}

pub async fn update_client_remark(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateClientRemarkRequest>,
) -> impl IntoResponse {
    let id_str = id.to_string();
    let remark = &payload.remark;
    
    if let Err(e) = sqlx::query("UPDATE clients SET remark = ? WHERE id = ?")
        .bind(remark)
        .bind(id_str)
        .execute(&state.db).await {
         return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to update client remark: {}", e)).into_response();
    }
    
    (StatusCode::OK, "Client remark updated").into_response()
}

// API: Update Client Display IP
#[derive(serde::Deserialize)]
pub struct UpdateClientDisplayIpRequest {
    pub display_ip: Option<String>,
}

pub async fn update_client_display_ip(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateClientDisplayIpRequest>,
) -> impl IntoResponse {
    let id_str = id.to_string();
    let display_ip = payload.display_ip;
    
    if let Err(e) = sqlx::query("UPDATE clients SET display_ip = ? WHERE id = ?")
        .bind(display_ip)
        .bind(id_str)
        .execute(&state.db).await {
         return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to update client display IP: {}", e)).into_response();
    }
    
    (StatusCode::OK, "Client display IP updated").into_response()
}

// API: Update Client Working Directory
#[derive(serde::Deserialize)]
pub struct UpdateClientCwdRequest {
    pub working_directory: String,
}

pub async fn update_client_working_directory(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateClientCwdRequest>,
) -> impl IntoResponse {
    let id_str = id.to_string();
    let cwd = &payload.working_directory;
    
    if let Err(e) = sqlx::query("UPDATE clients SET working_directory = ? WHERE id = ?")
        .bind(cwd)
        .bind(id_str)
        .execute(&state.db).await {
         return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to update client working directory: {}", e)).into_response();
    }
    
    (StatusCode::OK, "Client working directory updated").into_response()
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

#[derive(serde::Serialize)]
pub struct PaginatedUpdates {
    pub updates: Vec<ClientUpdateItem>,
    pub total: i64,
}

#[derive(serde::Deserialize)]
pub struct UpdateParams {
    pub page: Option<i64>,
    pub limit: Option<i64>,
}

pub async fn list_updates(
    State(state): State<Arc<AppState>>,
    Query(params): Query<UpdateParams>,
) -> Json<PaginatedUpdates> {
    let page = params.page.unwrap_or(1).max(1);
    let limit = params.limit.unwrap_or(50).max(1);
    let offset = (page - 1) * limit;

    let total = sqlx::query("SELECT COUNT(*) as count FROM client_updates")
        .fetch_one(&state.db)
        .await
        .map(|r| r.try_get::<i64, _>("count").unwrap_or(0))
        .unwrap_or(0);

    let rows = sqlx::query("SELECT id, version, filename, platform, strftime('%Y-%m-%dT%H:%M:%SZ', uploaded_at) as uploaded_at FROM client_updates ORDER BY uploaded_at DESC LIMIT ? OFFSET ?")
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    
    let items = rows.into_iter().map(|r| {
        let id_str: String = r.get("id");
        ClientUpdateItem {
            id: Uuid::parse_str(&id_str).unwrap_or_default(),
            version: r.get("version"),
            filename: r.get("filename"),
            platform: r.get("platform"),
            uploaded_at: r.get::<Option<String>, _>("uploaded_at").unwrap_or_default(),
        }
    }).collect();
    
    Json(PaginatedUpdates { updates: items, total })
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
            
            // Use unique filename to prevent overwriting history versions
            let unique_filename = format!("{}_{}", Uuid::new_v4(), file_name);
            let path = format!("{}/{}", dir_path, unique_filename);
            if let Err(e) = tokio::fs::write(&path, &data).await {
                 return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write file: {}", e)).into_response();
            }
            saved_filename = unique_filename;
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

    let host_header = headers.get("host").and_then(|h| h.to_str().ok());

    let mut targets: Vec<(mpsc::Sender<Message>, String)> = Vec::new();
    for client_id in payload.client_ids {
        if let Some(client) = state.clients.get(&client_id) {
             let base_url = get_download_base_url(&state, Some(client_id), host_header);
             let download_url = format!("{}/api/files/download/updates/{}", base_url, update.filename);
             targets.push((client.tx.clone(), download_url));
        }
    }

    let semaphore = Arc::new(tokio::sync::Semaphore::new(10));
    let mut handles = Vec::with_capacity(targets.len());

    for (tx, download_url) in targets {
        let sem = semaphore.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.map_err(|_| "Semaphore error".to_string())?;
            let msg = Message::Command {
                id: Uuid::new_v4(),
                cmd: CommandPayload::UpdateClient { url: download_url },
            };
            match tokio::time::timeout(Duration::from_secs(10), tx.send(msg)).await {
                Ok(Ok(())) => Ok::<_, String>(()),
                Ok(Err(_)) => Err("Channel closed".to_string()),
                Err(_) => Err("Timeout".to_string()),
            }
        }));
    }

    let total = handles.len();
    let mut success = 0;
    let mut errors: Vec<String> = Vec::new();
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => success += 1,
            Ok(Err(e)) => errors.push(e),
            Err(e) => errors.push(format!("Join error: {}", e)),
        }
    }

    let failed = total - success;
    if errors.is_empty() {
        (StatusCode::OK, format!("Update triggered for {} clients", success)).into_response()
    } else {
        (StatusCode::OK, format!("Update triggered: {} success, {} failed. Details: {}", success, failed, errors.join("; "))).into_response()
    }
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let host = headers.get("host")
        .and_then(|h| h.to_str().ok())
        .map(|h| h.to_string())
        .unwrap_or_else(|| format!("{}:{}", state.config.host, state.config.port));

    ws.on_upgrade(move |socket| handle_socket(socket, state, addr, host))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, addr: SocketAddr, server_host: String) {
    let (mut sender, mut receiver) = socket.split();

    // Authenticate first
    // Wait for the first message which MUST be Register
    let client_id: Uuid;
    let hostname: String;
    let os: String;
    let alias: Option<String>;
    let version: String;
    let ips: Vec<String>;
    let started_at: Option<chrono::DateTime<chrono::Utc>>;

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
        Ok(Message::Register { client_id: id, token, hostname: h, os: o, alias: a, version: v, ips: i, started_at: s }) => {
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
            started_at = s;
            
            info!("Client registered: {} ({}) - {} [Alias: {:?}] [IP: {}] [Ver: {}]", client_id, hostname, os, alias, addr, version);
            
            // Persist client to DB for history joins
            let client_id_str = client_id.to_string();
            let ip_str = addr.ip().to_string();
            let ips_json = serde_json::to_string(&ips).unwrap_or("[]".to_string());
            let started_at_naive = started_at.map(|d| d.naive_utc());
            
            if let Err(e) = sqlx::query(
                "INSERT INTO clients (id, hostname, os, last_seen, status, alias, ip, ips, version, started_at) VALUES (?, ?, ?, CURRENT_TIMESTAMP, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET hostname = excluded.hostname, os = excluded.os, last_seen = CURRENT_TIMESTAMP, status = excluded.status, alias = excluded.alias, ip = excluded.ip, ips = excluded.ips, version = excluded.version, started_at = excluded.started_at"
            )
            .bind(&client_id_str)
            .bind(&hostname)
            .bind(&os)
            .bind("connected")
            .bind(&alias)
            .bind(&ip_str)
            .bind(&ips_json)
            .bind(&version)
            .bind(started_at_naive)
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
        started_at,
        server_host,
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
                                let _ = sqlx::query("UPDATE clients SET last_seen = CURRENT_TIMESTAMP, status = 'connected' WHERE id = ?")
                                    .bind(&client_id_str)
                                    .execute(&state.db).await;
                            }
                            Message::Response { id, result } => {
                                info!("Received response for command {}: {:?}", id, result);
                                state.results.insert(id, result.clone());
                                if let Some((_, waiter)) = state.waiters.remove(&id) {
                                    let _ = waiter.send(result);
                                }
                                state.cmd_history_map.remove(&id);
                            }
                            Message::Progress { id, message } => {
                                if let Some(entry) = state.cmd_history_map.get(&id) {
                                    let history_id = *entry;
                                    if let Some(mut progress) = state.active_executions.get_mut(&history_id) {
                                        progress.logs.push(message);
                                    }
                                }
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
            let _ = sqlx::query("UPDATE clients SET status = 'disconnected' WHERE id = ?")
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

fn get_download_base_url(state: &AppState, client_id: Option<Uuid>, request_host: Option<&str>) -> String {
    if let Some(prefix) = &state.config.download_url_prefix {
        return if prefix.starts_with("http") {
             prefix.trim_end_matches('/').to_string()
        } else {
             let scheme = if state.config.tls_cert_path.is_some() { "https" } else { "http" };
             format!("{}://{}", scheme, prefix.trim_end_matches('/'))
        };
    }
    
    let scheme = if state.config.tls_cert_path.is_some() { "https" } else { "http" };

    // Fallback to client's registration host if client_id provided
    if let Some(cid) = client_id {
        if let Some(c) = state.clients.get(&cid) {
             return format!("{}://{}", scheme, c.server_host);
        }
    }
    
    // Fallback to request host
    if let Some(host) = request_host {
        return format!("{}://{}", scheme, host);
    }
    
    // Fallback to config host
    format!("{}://{}:{}", scheme, state.config.host, state.config.port)
}

// ---------------------------------------------------------------------------
// Scheduled Tasks API
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
pub struct ScheduledTaskResponse {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub cron_expression: String,
    pub task_type: String,
    pub group_id: Option<String>,
    pub script_ids: Vec<String>,
    pub client_ids: Vec<String>,
    pub steps: serde_json::Value,
    pub enabled: bool,
    pub created_at: Option<String>,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub last_status: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct CreateScheduledTaskRequest {
    pub name: String,
    pub description: Option<String>,
    pub cron_expression: String,
    pub task_type: String, // "group" or "custom"
    pub group_id: Option<String>,
    pub script_ids: Option<Vec<String>>,
    pub client_ids: Option<Vec<String>>,
    pub steps: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
pub struct UpdateScheduledTaskRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub cron_expression: Option<String>,
    pub task_type: Option<String>,
    pub group_id: Option<String>,
    pub script_ids: Option<Vec<String>>,
    pub client_ids: Option<Vec<String>>,
    pub steps: Option<serde_json::Value>,
    pub enabled: Option<bool>,
}

pub async fn list_scheduled_tasks(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<ScheduledTaskResponse>> {
    let rows = sqlx::query(
        "SELECT id, name, description, cron_expression, task_type, group_id, script_ids, client_ids, steps, enabled, created_at, last_run_at, next_run_at, last_status \
         FROM scheduled_tasks ORDER BY created_at DESC"
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let tasks = rows.into_iter().map(|r| {
        let id_str: String = r.get("id");
        let steps_str: String = r.get("steps");
        let steps_val: serde_json::Value = serde_json::from_str(&steps_str).unwrap_or(serde_json::Value::Array(vec![]));
        let script_ids_str: String = r.get("script_ids");
        let client_ids_str: String = r.get("client_ids");
        let script_ids: Vec<String> = serde_json::from_str(&script_ids_str).unwrap_or_default();
        let client_ids: Vec<String> = serde_json::from_str(&client_ids_str).unwrap_or_default();
        let enabled: i32 = r.get("enabled");

        ScheduledTaskResponse {
            id: Uuid::parse_str(&id_str).unwrap_or_default(),
            name: r.get("name"),
            description: r.get("description"),
            cron_expression: r.get("cron_expression"),
            task_type: r.get("task_type"),
            group_id: r.get("group_id"),
            script_ids,
            client_ids,
            steps: steps_val,
            enabled: enabled != 0,
            created_at: r.get("created_at"),
            last_run_at: r.get("last_run_at"),
            next_run_at: r.get("next_run_at"),
            last_status: r.get::<Option<String>, _>("last_status"),
        }
    }).collect();

    Json(tasks)
}

fn compute_next_run(cron_expr: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    crate::scheduler::CronExpr::parse(cron_expr).ok()?.next_occurrence(chrono::Utc::now())
}

pub async fn create_scheduled_task(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateScheduledTaskRequest>,
) -> impl IntoResponse {
    // Validate cron expression
    if compute_next_run(&payload.cron_expression).is_none() {
        return (StatusCode::BAD_REQUEST, "Invalid cron expression").into_response();
    }

    let id = Uuid::new_v4();
    let next_run = compute_next_run(&payload.cron_expression);

    let script_ids = serde_json::to_string(&payload.script_ids.unwrap_or_default()).unwrap_or("[]".to_string());
    let client_ids = serde_json::to_string(&payload.client_ids.unwrap_or_default()).unwrap_or("[]".to_string());
    let steps = serde_json::to_string(&payload.steps.unwrap_or(serde_json::Value::Array(vec![]))).unwrap_or("[]".to_string());
    let description = payload.description.unwrap_or_default();

    let result = sqlx::query(
        "INSERT INTO scheduled_tasks (id, name, description, cron_expression, task_type, group_id, script_ids, client_ids, steps, enabled, next_run_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?)"
    )
    .bind(id.to_string())
    .bind(&payload.name)
    .bind(&description)
    .bind(&payload.cron_expression)
    .bind(&payload.task_type)
    .bind(&payload.group_id)
    .bind(&script_ids)
    .bind(&client_ids)
    .bind(&steps)
    .bind(next_run)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => (StatusCode::CREATED, "Scheduled task created").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create: {}", e)).into_response(),
    }
}

pub async fn update_scheduled_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateScheduledTaskRequest>,
) -> impl IntoResponse {
    let id_str = id.to_string();

    // Fetch existing
    let existing = sqlx::query("SELECT name, description, cron_expression, task_type, group_id, script_ids, client_ids, steps, enabled FROM scheduled_tasks WHERE id = ?")
        .bind(&id_str)
        .fetch_optional(&state.db)
        .await;

    let row = match existing {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "Scheduled task not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {}", e)).into_response(),
    };

    let cron_expr: String = payload.cron_expression.clone().unwrap_or_else(|| row.get("cron_expression"));

    // Validate cron if changed
    if payload.cron_expression.is_some() && crate::scheduler::CronExpr::parse(&cron_expr).is_err() {
        return (StatusCode::BAD_REQUEST, "Invalid cron expression").into_response();
    }

    let next_run = compute_next_run(&cron_expr);

    let name: String = payload.name.unwrap_or_else(|| row.get("name"));
    let description: String = payload.description.unwrap_or_else(|| row.get("description"));
    let task_type: String = payload.task_type.unwrap_or_else(|| row.get("task_type"));
    let group_id: Option<String> = if payload.group_id.is_some() { payload.group_id } else { row.get("group_id") };
    let enabled_val: bool = payload.enabled.unwrap_or_else(|| {
        let v: i32 = row.get("enabled");
        v != 0
    });
    let enabled: i32 = if enabled_val { 1 } else { 0 };

    let script_ids: String = if let Some(ref ids) = payload.script_ids {
        serde_json::to_string(ids).unwrap_or("[]".to_string())
    } else {
        row.get("script_ids")
    };
    let client_ids: String = if let Some(ref ids) = payload.client_ids {
        serde_json::to_string(ids).unwrap_or("[]".to_string())
    } else {
        row.get("client_ids")
    };
    let steps: String = if let Some(ref s) = payload.steps {
        serde_json::to_string(s).unwrap_or("[]".to_string())
    } else {
        row.get("steps")
    };

    match sqlx::query(
        "UPDATE scheduled_tasks SET name = ?, description = ?, cron_expression = ?, task_type = ?, group_id = ?, script_ids = ?, client_ids = ?, steps = ?, enabled = ?, next_run_at = ? WHERE id = ?"
    )
    .bind(&name)
    .bind(&description)
    .bind(&cron_expr)
    .bind(&task_type)
    .bind(&group_id)
    .bind(&script_ids)
    .bind(&client_ids)
    .bind(&steps)
    .bind(enabled)
    .bind(next_run)
    .bind(&id_str)
    .execute(&state.db)
    .await
    {
        Ok(_) => (StatusCode::OK, "Scheduled task updated").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to update: {}", e)).into_response(),
    }
}

pub async fn delete_scheduled_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let id_str = id.to_string();
    match sqlx::query("DELETE FROM scheduled_tasks WHERE id = ?")
        .bind(&id_str)
        .execute(&state.db)
        .await
    {
        Ok(_) => (StatusCode::OK, "Scheduled task deleted").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete: {}", e)).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct ToggleScheduledTaskRequest {
    pub enabled: bool,
}

pub async fn toggle_scheduled_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(payload): Json<ToggleScheduledTaskRequest>,
) -> impl IntoResponse {
    let id_str = id.to_string();
    let v: i32 = if payload.enabled { 1 } else { 0 };

    // If enabling, recompute next_run_at from cron
    let next_run = if payload.enabled {
        let cron_expr: Option<String> = sqlx::query_scalar("SELECT cron_expression FROM scheduled_tasks WHERE id = ?")
            .bind(&id_str)
            .fetch_optional(&state.db)
            .await
            .unwrap_or(None);

        cron_expr.and_then(|expr| compute_next_run(&expr))
    } else {
        None
    };

    match sqlx::query("UPDATE scheduled_tasks SET enabled = ?, next_run_at = ? WHERE id = ?")
        .bind(v)
        .bind(next_run)
        .bind(&id_str)
        .execute(&state.db)
        .await
    {
        Ok(_) => (StatusCode::OK, "Scheduled task updated").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to toggle: {}", e)).into_response(),
    }
}
