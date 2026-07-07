use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KeyValuePair {
    pub key: String,
    pub value: String,
}

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
        ips: Vec<String>,
        started_at: Option<chrono::DateTime<chrono::Utc>>,
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

    // Progress updates (Client -> Server during long operations like file transfers)
    Progress {
        id: Uuid, // Correlates to Command ID
        message: String,
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
    UploadFile { src_path: String, upload_url: String, #[serde(default)] compress: Option<bool> },
    ListDir { path: String },
    GetHardwareInfo,
    UpdateClient { url: String },
    ReadFile { path: String },
    WriteFile { path: String, content: String },
    // Download zip from URL and unzip to dest_path
    DownloadAndUnzip { url: String, dest_path: String },
    // Zip directory at src_path and upload to upload_url
    ZipAndUpload { src_path: String, upload_url: String },
    CopyFile { src_path: String, dest_path: String },
    MoveFile { src_path: String, dest_path: String },
    DeleteFile { path: String },
    Compress { src_path: String, dest_path: String },
    Decompress { src_path: String, dest_path: String },
    HttpRequest {
        method: String,
        url: String,
        headers: Vec<KeyValuePair>,
        query_params: Vec<KeyValuePair>,
        body: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "status", content = "data")]
pub enum CommandResult {
    ShellOutput { stdout: String, stderr: String, exit_code: i32, cwd: String },
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
    pub modified: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HardwareInfo {
    pub cpu_usage: f32,
    pub total_memory: u64,
    pub used_memory: u64,
    pub platform: String,
}
