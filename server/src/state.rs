use dashmap::DashMap;
use sqlx::{Pool, Sqlite};
use tokio::sync::mpsc;
use uuid::Uuid;
use serde::{Deserialize, Serialize};

use common::{Message, CommandResult};
use crate::config::ServerConfig;

pub struct AppState {
    pub db: Pool<Sqlite>,
    pub clients: DashMap<Uuid, ClientConnection>,
    pub results: DashMap<Uuid, CommandResult>,
    pub active_executions: DashMap<Uuid, ExecutionProgress>,
    pub web_sessions: DashMap<String, String>, // token -> username
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
    Shell { cmd: String, args: Vec<String> },
    Upload { local_path: String, remote_path: String },
    Download { remote_path: String, browser_download: Option<bool> },
    UploadDir { local_path: String, remote_path: String },
    DownloadDir { remote_path: String, browser_download: Option<bool> },
}

impl AppState {
    pub fn new(db: Pool<Sqlite>, config: ServerConfig) -> Self {
        Self {
            db,
            clients: DashMap::new(),
            results: DashMap::new(),
            active_executions: DashMap::new(),
            web_sessions: DashMap::new(),
            config,
        }
    }
}
