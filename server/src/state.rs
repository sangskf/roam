use dashmap::DashMap;
use sqlx::{Pool, Sqlite};
use tokio::sync::mpsc;
use uuid::Uuid;

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
