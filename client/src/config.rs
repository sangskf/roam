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
            .set_default("tls_insecure", false)?
            .add_source(File::with_name("client_config").required(false));

        // Also try loading config from executable directory (for Windows Service)
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let config_path = exe_dir.join("client_config");
                // Don't duplicate if CWD is same as exe dir, but config-rs handles overrides fine
                builder = builder.add_source(File::from(config_path).required(false));
            }
        }

        builder = builder.add_source(config::Environment::with_prefix("APP"));

        let config = builder.build()?;
        Ok(config.try_deserialize()?)
    }
}
