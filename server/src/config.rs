use serde::Deserialize;
use config::{Config, File};

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub database_url: String,
    pub auth_token: String,
}

impl ServerConfig {
    pub fn new() -> anyhow::Result<Self> {
        let builder = Config::builder()
            .set_default("host", "0.0.0.0")?
            .set_default("port", 3000)?
            .set_default("database_url", "sqlite:server.db")?
            .set_default("auth_token", "secret-token")?
            .add_source(File::with_name("server_config").required(false))
            .add_source(config::Environment::with_prefix("APP"));

        let config = builder.build()?;
        Ok(config.try_deserialize()?)
    }
}
