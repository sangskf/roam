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
    pub tls_cert_path: Option<String>,
    pub tls_key_path: Option<String>,
}

impl ServerConfig {
    pub fn new() -> anyhow::Result<Self> {
        let builder = Config::builder()
            .set_default("host", "0.0.0.0")?
            .set_default("port", 3333)?
            .set_default("database_url", "sqlite:roam.db")?
            .set_default("auth_token", "secret-token")?
            .set_default("web_auth_enabled", true)?
            .set_default("web_jwt_secret", "roam-secret-key")?
            .set_default("tls_cert_path", None::<String>)?
            .set_default("tls_key_path", None::<String>)?
            .add_source(File::with_name("server_config").required(false))
            .add_source(config::Environment::with_prefix("APP"));

        let config = builder.build()?;
        let mut server_config: ServerConfig = config.try_deserialize()?;

        // Auto-detect certificates if not configured
        if server_config.tls_cert_path.is_none() && server_config.tls_key_path.is_none() {
            if std::path::Path::new("cert.pem").exists() && std::path::Path::new("key.pem").exists() {
                server_config.tls_cert_path = Some("cert.pem".to_string());
                server_config.tls_key_path = Some("key.pem".to_string());
            }
        }

        Ok(server_config)
    }
}
