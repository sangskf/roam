use std::process::Stdio;
use sysinfo::System;
use tokio::process::Command;
use std::fs;
use std::path::PathBuf;
use tracing::{info, error};

use common::{CommandPayload, CommandResult, HardwareInfo, FileInfo};

pub async fn handle_command(cmd: CommandPayload) -> CommandResult {
    match cmd {
        CommandPayload::ShellExec { cmd, args } => {
            info!("Executing shell command: {} {:?}", cmd, args);
            // Trim command just in case
            let cmd_trimmed = cmd.trim();
            
            if cmd_trimmed == "cd" {
                let default_path = if cfg!(target_os = "windows") {
                    std::env::var("USERPROFILE").unwrap_or("C:\\".to_string())
                } else {
                    std::env::var("HOME").unwrap_or("/".to_string())
                };
                
                let target_path = args.get(0).cloned().unwrap_or(default_path);
                
                match std::env::set_current_dir(&target_path) {
                    Ok(_) => CommandResult::ShellOutput {
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: 0,
                    },
                    Err(e) => CommandResult::ShellOutput {
                        stdout: String::new(),
                        stderr: format!("cd: failed to change directory to {}: {}\n", target_path, e),
                        exit_code: 1,
                    },
                }
            } else {
                match Command::new(cmd_trimmed)
                    .args(args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                {
                    Ok(child) => {
                        match child.wait_with_output().await {
                            Ok(output) => CommandResult::ShellOutput {
                                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                                exit_code: output.status.code().unwrap_or(-1),
                            },
                            Err(e) => CommandResult::Error(format!("Failed to wait on child: {}", e)),
                        }
                    }
                    Err(e) => CommandResult::Error(format!("Failed to spawn command: {}", e)),
                }
            }
        }
        CommandPayload::ChangeDir { path } => {
            info!("Changing directory to: {}", path);
            match std::env::set_current_dir(&path) {
                Ok(_) => CommandResult::DirChanged { new_path: path },
                Err(e) => {
                    error!("Failed to change dir: {}", e);
                    CommandResult::Error(format!("Failed to change dir: {}", e))
                },
            }
        }
        CommandPayload::GetHardwareInfo => {
            info!("Getting hardware info");
            let mut sys = System::new_all();
            sys.refresh_all();
            
            let total_memory = sys.total_memory();
            let used_memory = sys.used_memory();
            let cpu_usage = sys.global_cpu_usage();
            let platform = std::env::consts::OS.to_string();

            CommandResult::HardwareInfo(HardwareInfo {
                cpu_usage,
                total_memory,
                used_memory,
                platform,
            })
        }
        CommandPayload::ListDir { path } => {
             info!("Listing directory: {}", path);
             match std::fs::read_dir(path) {
                 Ok(entries) => {
                     let mut files = Vec::new();
                     for entry in entries {
                         if let Ok(entry) = entry {
                             let metadata = entry.metadata().ok();
                             let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                             let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
                             files.push(FileInfo {
                                 name: entry.file_name().to_string_lossy().to_string(),
                                 is_dir,
                                 size,
                             });
                         }
                     }
                     CommandResult::FileList { files }
                 }
                 Err(e) => {
                     error!("Failed to read dir: {}", e);
                     CommandResult::Error(format!("Failed to read dir: {}", e))
                 },
             }
        }
        CommandPayload::DownloadFile { url, dest_path } => {
            info!("Downloading file from {} to {}", url, dest_path);
            let client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3600)) // 1 hour timeout for large files
                .build() {
                    Ok(c) => c,
                    Err(e) => return CommandResult::Error(format!("Failed to build http client: {}", e)),
                };

            match client.get(&url).send().await {
                Ok(resp) => {
                    if resp.status().is_success() {
                        match resp.bytes().await {
                            Ok(bytes) => {
                                match tokio::fs::write(&dest_path, bytes.clone()).await {
                                    Ok(_) => {
                                        info!("Download successful: {}", dest_path);
                                        CommandResult::Success(format!("File downloaded to {}", dest_path))
                                    },
                                    Err(e) => {
                                        error!("Failed to write file: {}", e);
                                        // Try to create parent directories if they don't exist
                                        if let Some(parent) = std::path::Path::new(&dest_path).parent() {
                                            if let Err(dir_err) = tokio::fs::create_dir_all(parent).await {
                                                error!("Failed to create directories: {}", dir_err);
                                                return CommandResult::Error(format!("Failed to create directories: {} (Original error: {})", dir_err, e));
                                            }
                                            // Retry write
                                            match tokio::fs::write(&dest_path, bytes).await {
                                                Ok(_) => {
                                                    info!("Download successful after creating dirs: {}", dest_path);
                                                    CommandResult::Success(format!("File downloaded to {}", dest_path))
                                                },
                                                Err(retry_err) => {
                                                    error!("Failed to write file after creating dirs: {}", retry_err);
                                                    CommandResult::Error(format!("Failed to write file after creating dirs: {}", retry_err))
                                                },
                                            }
                                        } else {
                                            CommandResult::Error(format!("Failed to write file: {}", e))
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Failed to read bytes: {}", e);
                                CommandResult::Error(format!("Failed to read bytes: {}", e))
                            },
                        }
                    } else {
                        error!("Download failed with status: {}", resp.status());
                        CommandResult::Error(format!("Download failed with status: {}", resp.status()))
                    }
                }
                Err(e) => {
                    error!("Request failed: {}", e);
                    CommandResult::Error(format!("Request failed: {}", e))
                },
            }
        }
        CommandPayload::UploadFile { src_path, upload_url } => {
            info!("Uploading file {} to {}", src_path, upload_url);
            match tokio::fs::read(&src_path).await {
                Ok(data) => {
                    let client = match reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(3600))
                        .build() {
                            Ok(c) => c,
                            Err(e) => return CommandResult::Error(format!("Failed to build http client: {}", e)),
                        };
                    let file_name = std::path::Path::new(&src_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or("unknown".to_string());
                        
                    let form = reqwest::multipart::Form::new()
                        .part("file", reqwest::multipart::Part::bytes(data).file_name(file_name));
                        
                    match client.post(&upload_url).multipart(form).send().await {
                        Ok(resp) => {
                            if resp.status().is_success() {
                                info!("Upload successful");
                                CommandResult::Success("File uploaded successfully".to_string())
                            } else {
                                error!("Upload failed with status: {}", resp.status());
                                CommandResult::Error(format!("Upload failed with status: {}", resp.status()))
                            }
                        }
                        Err(e) => {
                            error!("Failed to send file: {}", e);
                            CommandResult::Error(format!("Failed to send file: {}", e))
                        },
                    }
                }
                Err(e) => {
                    error!("Failed to read file: {}", e);
                    CommandResult::Error(format!("Failed to read file: {}", e))
                },
            }
        }
        CommandPayload::UpdateClient { url } => {
            info!("Updating client from {}", url);
            match download_and_replace(&url).await {
                Ok(_) => {
                    // This line might not be reached if replacement kills the process immediately,
                    // but usually self-replace allows graceful exit or we should exit manually.
                    info!("Client updated, restarting...");
                    std::process::exit(0);
                    // CommandResult::Success("Client updated and restarting...".to_string())
                }
                Err(e) => {
                    error!("Update failed: {}", e);
                    CommandResult::Error(format!("Update failed: {}", e))
                },
            }
        }
        CommandPayload::ReadFile { path } => {
            info!("Reading file: {}", path);
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => CommandResult::FileContent { content },
                Err(e) => {
                    error!("Failed to read file: {}", e);
                    CommandResult::Error(format!("Failed to read file: {}", e))
                },
            }
        }
        CommandPayload::WriteFile { path, content } => {
            info!("Writing file: {}", path);
            match tokio::fs::write(&path, content).await {
                Ok(_) => {
                    info!("File written successfully");
                    CommandResult::Success("File saved successfully".to_string())
                },
                Err(e) => {
                    error!("Failed to write file: {}", e);
                    CommandResult::Error(format!("Failed to write file: {}", e))
                },
            }
        }
    }
}

async fn download_and_replace(url: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3600))
        .build()?;
    let response = client.get(url).send().await?;
    let bytes = response.bytes().await?;
    
    let mut temp_file = std::env::temp_dir();
    temp_file.push("roam_client_update");
    // Append random string to avoid conflicts? 
    // Ideally use tempfile crate but we want simplicity.
    // Let's just overwrite.
    
    fs::write(&temp_file, bytes)?;
    
    // Make executable on unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&temp_file)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&temp_file, perms)?;
    }

    self_replace::self_replace(&temp_file)?;
    
    // Cleanup temp file
    let _ = fs::remove_file(&temp_file);
    
    Ok(())
}
