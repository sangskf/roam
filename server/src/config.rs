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
        let mut builder = Config::builder()
            .set_default("host", "0.0.0.0")?
            .set_default("port", 3333)?
            .set_default("database_url", "sqlite:roam.db")?
            .set_default("auth_token", "secret-token")?
            .set_default("web_auth_enabled", true)?
            .set_default("web_jwt_secret", "roam-secret-key")?
            .set_default("tls_cert_path", None::<String>)?
            .set_default("tls_key_path", None::<String>)?;

        // 1. Prioritize loading config from executable directory (Production/Service)
        let mut config_found = false;
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let config_path = exe_dir.join("server_config");
                if let Some(path_str) = config_path.to_str() {
                     builder = builder.add_source(File::with_name(path_str).required(false));
                     
                     if exe_dir.join("server_config.toml").exists() || 
                        exe_dir.join("server_config.json").exists() ||
                        exe_dir.join("server_config.yaml").exists() {
                         config_found = true;
                     }
                }
            }
        }
        
        // 2. Fallback to CWD (Development) if not found in exe dir
        if !config_found {
             builder = builder.add_source(File::with_name("server_config").required(false));
        }

        builder = builder.add_source(config::Environment::with_prefix("APP"));

        let config = builder.build()?;
        let mut server_config: ServerConfig = config.try_deserialize()?;

        // Fix database path if it's relative
        if server_config.database_url.starts_with("sqlite:") {
            let path_str = server_config.database_url.trim_start_matches("sqlite:");
            let path = std::path::Path::new(path_str);
            if !path.is_absolute() {
                if let Ok(exe_path) = std::env::current_exe() {
                    if let Some(exe_dir) = exe_path.parent() {
                        let new_path = exe_dir.join(path);
                        server_config.database_url = format!("sqlite:{}", new_path.display());
                    }
                }
            }
        }

        // Auto-detect certificates if not configured (check exe dir too)
        if server_config.tls_cert_path.is_none() && server_config.tls_key_path.is_none() {
            // Check current dir
            if std::path::Path::new("cert.pem").exists() && std::path::Path::new("key.pem").exists() {
                server_config.tls_cert_path = Some("cert.pem".to_string());
                server_config.tls_key_path = Some("key.pem".to_string());
            } else if let Ok(exe_path) = std::env::current_exe() {
                // Check exe dir
                if let Some(exe_dir) = exe_path.parent() {
                    let cert_path = exe_dir.join("cert.pem");
                    let key_path = exe_dir.join("key.pem");
                    if cert_path.exists() && key_path.exists() {
                        server_config.tls_cert_path = Some(cert_path.to_string_lossy().to_string());
                        server_config.tls_key_path = Some(key_path.to_string_lossy().to_string());
                    }
                }
            }
        }

        Ok(server_config)
    }
}
