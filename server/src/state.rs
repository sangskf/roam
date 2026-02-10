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
    pub config: ServerConfig,
}

pub struct ClientConnection {
    pub tx: mpsc::Sender<Message>,
    pub hostname: String,
    pub os: String,
    pub alias: Option<String>,
    pub ip: String,
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
    Download { remote_path: String },
}

impl AppState {
    pub fn new(db: Pool<Sqlite>, config: ServerConfig) -> Self {
        Self {
            db,
            clients: DashMap::new(),
            results: DashMap::new(),
            config,
        }
    }
}
