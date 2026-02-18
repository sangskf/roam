use serde::Deserialize;
use config::{Config, File};

#[derive(Debug, Deserialize, Clone)]
pub struct ClientConfig {
    pub server_url: String,
    pub auth_token: String,
    pub heartbeat_interval_sec: u64,
    pub alias: Option<String>,
}

impl ClientConfig {
    pub fn new() -> anyhow::Result<Self> {
        let builder = Config::builder()
            .set_default("server_url", "ws://127.0.0.1:3333/ws")?
            .set_default("auth_token", "secret-token")?
            .set_default("heartbeat_interval_sec", 10)?
            .set_default("alias", None::<String>)?
            .add_source(File::with_name("client_config").required(false))
            .add_source(config::Environment::with_prefix("APP"));

        let config = builder.build()?;
        Ok(config.try_deserialize()?)
    }
}
