use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", content = "payload")]
pub enum Message {
    // Auth
    Register { 
        client_id: Uuid, 
        token: String,
        hostname: String, 
        os: String,
        alias: Option<String>,
        version: String,
    },
    AuthSuccess,
    AuthFailed(String),

    // Heartbeat
    Heartbeat,
    
    // Commands (Server -> Client)
    Command {
        id: Uuid, // Command ID to correlate response
        cmd: CommandPayload,
    },
    
    // Responses (Client -> Server)
    Response {
        id: Uuid, // Correlates to Command ID
        result: CommandResult,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "cmd_type", content = "args")]
pub enum CommandPayload {
    ShellExec { cmd: String, args: Vec<String> },
    ChangeDir { path: String },
    // Server provides a URL for the client to download file FROM
    DownloadFile { url: String, dest_path: String }, 
    // Server provides a URL for the client to upload file TO
    UploadFile { src_path: String, upload_url: String }, 
    ListDir { path: String },
    GetHardwareInfo,
    UpdateClient { url: String },
    ReadFile { path: String },
    WriteFile { path: String, content: String },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "status", content = "data")]
pub enum CommandResult {
    ShellOutput { stdout: String, stderr: String, exit_code: i32 },
    DirChanged { new_path: String },
    FileList { files: Vec<FileInfo> },
    FileContent { content: String },
    HardwareInfo(HardwareInfo),
    Success(String),
    Error(String),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileInfo {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HardwareInfo {
    pub cpu_usage: f32,
    pub total_memory: u64,
    pub used_memory: u64,
    pub platform: String,
}
