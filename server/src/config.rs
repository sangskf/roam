use serde::Deserialize;
use config::{Config, File};

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub database_url: String,
    pub auth_token: String,
    pub web_auth_enabled: bool,
    pub web_jwt_secret: String,
}

impl ServerConfig {
    pub fn new() -> anyhow::Result<Self> {
        let builder = Config::builder()
            .set_default("host", "0.0.0.0")?
            .set_default("port", 3000)?
            .set_default("database_url", "sqlite:roam.db")?
            .set_default("auth_token", "secret-token")?
            .set_default("web_auth_enabled", true)?
            .set_default("web_jwt_secret", "roam-secret-key")?
            .add_source(File::with_name("server_config").required(false))
            .add_source(config::Environment::with_prefix("APP"));

        let config = builder.build()?;
        Ok(config.try_deserialize()?)
    }
}
