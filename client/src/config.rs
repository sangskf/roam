use serde::Deserialize;
use config::{Config, File};

#[derive(Debug, Deserialize, Clone)]
pub struct ClientConfig {
    pub server_url: String,
    pub auth_token: String,
    pub heartbeat_interval_sec: u64,
    pub alias: Option<String>,
    pub tls_insecure: bool,
}

impl ClientConfig {
    pub fn new() -> anyhow::Result<Self> {
        let mut builder = Config::builder()
            .set_default("server_url", "ws://127.0.0.1:3333/ws")?
            .set_default("auth_token", "secret-token")?
            .set_default("heartbeat_interval_sec", 10)?
            .set_default("alias", None::<String>)?
            .set_default("tls_insecure", false)?;

        // 1. Prioritize loading config from executable directory (Production/Service)
        let mut config_found = false;
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let config_path = exe_dir.join("client_config");
                if let Some(path_str) = config_path.to_str() {
                    // This will look for client_config.toml, .json, etc. in exe_dir
                    builder = builder.add_source(File::with_name(path_str).required(false));
                    
                    if exe_dir.join("client_config.toml").exists() || 
                       exe_dir.join("client_config.json").exists() ||
                       exe_dir.join("client_config.yaml").exists() {
                        config_found = true;
                    }
                }
            }
        }

        // 2. Fallback to current working directory (Development) if not found in exe dir
        // Or always add it with lower priority?
        // User said: "runtime should use config file under binary directory".
        // This implies if binary dir has config, use it. If not (dev), use CWD.
        if !config_found {
             builder = builder.add_source(File::with_name("client_config").required(false));
        }

        builder = builder.add_source(config::Environment::with_prefix("APP"));

        let config = builder.build()?;
        Ok(config.try_deserialize()?)
    }
}
