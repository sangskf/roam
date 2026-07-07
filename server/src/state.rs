use dashmap::DashMap;
use sqlx::{Pool, Sqlite};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use serde::{Deserialize, Serialize};

use common::{Message, CommandResult, KeyValuePair};
use crate::config::ServerConfig;

pub struct AppState {
    pub db: Pool<Sqlite>,
    pub clients: DashMap<Uuid, ClientConnection>,
    pub results: DashMap<Uuid, CommandResult>,
    pub waiters: DashMap<Uuid, oneshot::Sender<CommandResult>>,
    pub active_executions: DashMap<Uuid, ExecutionProgress>,
    pub web_sessions: DashMap<String, String>, // token -> username
    pub cmd_history_map: DashMap<Uuid, Uuid>, // command_id -> history_id (for progress correlation)
    pub config: ServerConfig,
}

#[derive(Debug, Serialize, Clone)]
pub struct ExecutionProgress {
    pub execution_id: Uuid,
    pub script_name: String,
    pub client_hostname: String,
    pub status: String, // "running", "completed", "failed"
    pub logs: Vec<String>,
    pub current_step: usize,
    pub total_steps: usize,
}

pub struct ClientConnection {
    pub tx: mpsc::Sender<Message>,
    pub hostname: String,
    pub os: String,
    pub alias: Option<String>,
    pub ip: String,
    pub ips: Vec<String>,
    pub version: String,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub server_host: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ScriptGroup {
    pub id: Uuid,
    pub name: String,
    pub steps: Vec<ScriptStep>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", content = "payload")]
pub enum ScriptStep {
    Shell {
        cmd: String, args: Vec<String>,
        #[serde(default)] run_on_server: Option<bool>,
    },
    Upload {
        local_path: String, remote_path: String,
        #[serde(default)] local_path_is_absolute: Option<bool>,
        #[serde(default)] remote_path_is_absolute: Option<bool>,
        #[serde(default)] run_on_server: Option<bool>,
        #[serde(default)] compress: Option<bool>,
    },
    Download {
        remote_path: String, browser_download: Option<bool>,
        #[serde(default)] remote_path_is_absolute: Option<bool>,
        #[serde(default)] run_on_server: Option<bool>,
        #[serde(default)] server_save_path: Option<String>,
        #[serde(default)] compress: Option<bool>,
    },
    UploadDir {
        local_path: String, remote_path: String,
        #[serde(default)] local_path_is_absolute: Option<bool>,
        #[serde(default)] remote_path_is_absolute: Option<bool>,
        #[serde(default)] run_on_server: Option<bool>,
    },
    DownloadDir {
        remote_path: String, browser_download: Option<bool>,
        #[serde(default)] remote_path_is_absolute: Option<bool>,
        #[serde(default)] run_on_server: Option<bool>,
        #[serde(default)] server_save_path: Option<String>,
    },
    Copy {
        src_path: String, dest_path: String,
        #[serde(default)] src_path_is_absolute: Option<bool>,
        #[serde(default)] dest_path_is_absolute: Option<bool>,
        #[serde(default)] run_on_server: Option<bool>,
    },
    Move {
        src_path: String, dest_path: String,
        #[serde(default)] src_path_is_absolute: Option<bool>,
        #[serde(default)] dest_path_is_absolute: Option<bool>,
        #[serde(default)] run_on_server: Option<bool>,
    },
    Delete {
        path: String,
        #[serde(default)] path_is_absolute: Option<bool>,
        #[serde(default)] run_on_server: Option<bool>,
    },
    Compress {
        src_path: String,
        dest_path: String,
        #[serde(default)] run_on_server: Option<bool>,
    },
    Decompress {
        src_path: String,
        dest_path: String,
        #[serde(default)] run_on_server: Option<bool>,
    },
    HttpRequest {
        url: String,
        #[serde(default)] method: Option<String>,
        #[serde(default)] headers: Option<Vec<KeyValuePair>>,
        #[serde(default)] query_params: Option<Vec<KeyValuePair>>,
        #[serde(default)] body: Option<String>,
        #[serde(default)] run_on_server: Option<bool>,
    },
}

impl AppState {
    pub fn new(db: Pool<Sqlite>, config: ServerConfig) -> Self {
        Self {
            db,
            clients: DashMap::new(),
            results: DashMap::new(),
            waiters: DashMap::new(),
            active_executions: DashMap::new(),
            web_sessions: DashMap::new(),
            cmd_history_map: DashMap::new(),
            config,
        }
    }
}

impl ScriptStep {
    pub fn is_run_on_server(&self) -> bool {
        match self {
            ScriptStep::Shell { run_on_server, .. }
            | ScriptStep::Upload { run_on_server, .. }
            | ScriptStep::Download { run_on_server, .. }
            | ScriptStep::UploadDir { run_on_server, .. }
            | ScriptStep::DownloadDir { run_on_server, .. }
            | ScriptStep::Copy { run_on_server, .. }
            | ScriptStep::Move { run_on_server, .. }
            | ScriptStep::Delete { run_on_server, .. }
            | ScriptStep::Compress { run_on_server, .. }
            | ScriptStep::Decompress { run_on_server, .. }
            | ScriptStep::HttpRequest { run_on_server, .. } => run_on_server.unwrap_or(false),
        }
    }
}
