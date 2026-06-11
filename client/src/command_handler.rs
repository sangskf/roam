use std::process::Stdio;
use sysinfo::System;
use tokio::process::Command;
use tokio::io::{AsyncWriteExt, AsyncSeekExt, AsyncReadExt};
use std::fs;
use std::io::{SeekFrom, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, error, warn};
use walkdir::WalkDir;
use zip::write::FileOptions;
use flate2::read::GzEncoder;
use flate2::write::GzDecoder;
use flate2::Compression;

use common::{CommandPayload, CommandResult, HardwareInfo, FileInfo, KeyValuePair};

fn expand_path(path: &str) -> PathBuf {
    if path == "~" {
        let home = if cfg!(target_os = "windows") {
            std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string())
        } else {
            std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
        };
        return PathBuf::from(home);
    }
    
    if path.starts_with("~/") || (cfg!(target_os = "windows") && path.starts_with("~\\")) {
        let home = if cfg!(target_os = "windows") {
            std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string())
        } else {
            std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
        };
        return PathBuf::from(home).join(&path[2..]);
    }
    
    PathBuf::from(path)
}

fn zip_directory(src_dir: &std::path::Path, dst_file: &std::path::Path) -> anyhow::Result<()> {
    if !src_dir.is_dir() {
        return Err(anyhow::anyhow!("Source is not a directory"));
    }

    let file = std::fs::File::create(dst_file)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    let walkdir = WalkDir::new(src_dir);
    let it = walkdir.into_iter();

    for entry in it {
        let entry = entry?;
        let path = entry.path();
        let name = path.strip_prefix(src_dir)?;
        let path_as_string = name
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("Invalid path"))?;

        if path.is_file() {
            zip.start_file(path_as_string, options)?;
            let mut f = std::fs::File::open(path)?;
            std::io::copy(&mut f, &mut zip)?;
        } else if !name.as_os_str().is_empty() {
            zip.add_directory(path_as_string, options)?;
        }
    }
    zip.finish()?;
    Ok(())
}

fn unzip_file(zip_path: &std::path::Path, dest_dir: &std::path::Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let outpath = match file.enclosed_name() {
            Some(path) => dest_dir.join(path),
            None => continue,
        };

        if (*file.name()).ends_with('/') {
            std::fs::create_dir_all(&outpath)?;
        } else {
            if let Some(p) = outpath.parent() {
                if !p.exists() {
                    std::fs::create_dir_all(p)?;
                }
            }
            let mut outfile = std::fs::File::create(&outpath)?;
            std::io::copy(&mut file, &mut outfile)?;
        }
        
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = file.unix_mode() {
                std::fs::set_permissions(&outpath, std::fs::Permissions::from_mode(mode))?;
            }
        }
    }
    Ok(())
}

fn build_http_client(tls_insecure: bool) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(tls_insecure)
        .timeout(Duration::from_secs(3600))
        .build()
        .map_err(|e| format!("Failed to build http client: {}", e))
}

/// Gzip a file to a temporary file and return the temp path.
/// The temp file is placed alongside the source to avoid cross-filesystem moves.
fn gzip_file_to_temp(src: &Path) -> Result<PathBuf, String> {
    let src_file = std::fs::File::open(src).map_err(|e| format!("Failed to open {} for compression: {}", src.display(), e))?;
    let reader = std::io::BufReader::new(src_file);
    let mut encoder = GzEncoder::new(reader, Compression::default());

    let temp_path = {
        let _stem = src.file_name().unwrap_or_default().to_string_lossy();
        let dir = src.parent().unwrap_or_else(|| Path::new("."));
        let mut p = dir.join(format!(".roam_gzip_{}", uuid::Uuid::new_v4()));
        p.set_extension("gz");
        p
    };
    let mut dst_file = std::fs::File::create(&temp_path).map_err(|e| format!("Failed to create temp gzip file: {}", e))?;
    std::io::copy(&mut encoder, &mut dst_file).map_err(|e| format!("Failed to gzip file: {}", e))?;
    dst_file.flush().map_err(|e| format!("Failed to flush temp gzip file: {}", e))?;
    drop(dst_file);

    let orig_size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    let gz_size = std::fs::metadata(&temp_path).map(|m| m.len()).unwrap_or(0);
    info!("Compressed {} ({} bytes → {} bytes, {:.1}%)",
        src.display(), orig_size, gz_size,
        if orig_size > 0 { (gz_size as f64 / orig_size as f64) * 100.0 } else { 0.0 });

    Ok(temp_path)
}

/// Check if a file starts with gzip magic bytes and decompress it in-place if so.
/// Uses a temp file for atomic replacement.
fn decompress_gzip_in_place(path: &Path) -> Result<(), String> {
    // Read first 2 bytes to check gzip magic
    let mut magic = [0u8; 2];
    let mut file = std::fs::File::open(path).map_err(|e| format!("Failed to open {} for decompression check: {}", path.display(), e))?;
    if file.read_exact(&mut magic).is_err() || magic != [0x1f, 0x8b] {
        return Ok(()); // Not gzip compressed, nothing to do
    }
    drop(file);

    info!("Decompressing gzip file: {}", path.display());
    let temp_path = path.with_extension(format!(".roam_gunzip_{}", uuid::Uuid::new_v4()));

    let gz_file = std::fs::File::open(path).map_err(|e| format!("Failed to open gzip file: {}", e))?;
    let mut decoder = GzDecoder::new(std::fs::File::create(&temp_path).map_err(|e| format!("Failed to create temp file: {}", e))?);
    std::io::copy(&mut std::io::BufReader::new(gz_file), &mut decoder).map_err(|e| format!("Failed to decompress: {}", e))?;
    let _ = decoder.finish().map_err(|e| format!("Failed to finish decompression: {}", e))?;

    std::fs::rename(&temp_path, path).map_err(|e| format!("Failed to replace with decompressed file: {}", e))?;
    info!("Decompressed: {}", path.display());
    Ok(())
}

async fn single_download(url: &str, dest_path: &Path, client: &reqwest::Client, progress_tx: &Option<tokio::sync::mpsc::Sender<String>>) -> Result<(), String> {
    if let Some(ref tx) = progress_tx {
        let _ = tx.try_send(format!("Downloading: {} → {}", url, dest_path.display())).ok();
    }
    info!("Downloading {} to {} via single download", url, dest_path.display());
    let resp = client.get(url).send().await.map_err(|e| format!("Download failed: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("Download failed with status: {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("Failed to read response: {}", e))?;

    match tokio::fs::write(dest_path, &bytes).await {
        Ok(_) => {
            if let Some(ref tx) = progress_tx {
                let _ = tx.try_send(format!("Download complete: {} bytes to {}", bytes.len(), dest_path.display())).ok();
            }
            info!("Single download completed: {} bytes written to {}", bytes.len(), dest_path.display());
            Ok(())
        },
        Err(e) => {
            if let Some(parent) = dest_path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
                tokio::fs::write(dest_path, &bytes).await.map_err(|e| format!("Failed to write file after creating dirs: {}", e))
            } else {
                Err(format!("Failed to write file: {}", e))
            }
        }
    }
}

async fn chunked_download(url: &str, dest_path: &Path, client: &reqwest::Client, chunk_size: usize, max_concurrent: usize, progress_tx: &Option<tokio::sync::mpsc::Sender<String>>) -> Result<(), String> {
    // HEAD request to get file size and check range support
    let head_resp = client.head(url).send().await.map_err(|e| format!("HEAD request failed: {}", e))?;

    let file_size: usize = head_resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| "Missing Content-Length header".to_string())?;

    let accept_ranges = head_resp
        .headers()
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if file_size <= chunk_size || !accept_ranges.contains("bytes") {
        return single_download(url, dest_path, client, progress_tx).await;
    }

    let total_chunks = file_size.div_ceil(chunk_size);

    if let Some(ref tx) = progress_tx {
        let _ = tx.try_send(format!("Downloading: {} ({} bytes, {} chunks)", url, file_size, total_chunks)).ok();
    }
    info!("Chunked download of {} ({} bytes, {} chunks) to {}", url, file_size, total_chunks, dest_path.display());

    if let Some(parent) = dest_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let mut handles = Vec::with_capacity(total_chunks);
    let temp_dir = std::env::temp_dir();
    let session_id = uuid::Uuid::new_v4();

    for i in 0..total_chunks {
        let start = i * chunk_size;
        let end = std::cmp::min(start + chunk_size, file_size) - 1;
        let range_header = format!("bytes={}-{}", start, end);

        let client = client.clone();
        let url_str = url.to_string();
        let chunk_path = temp_dir.join(format!("roam_dl_{}_{}", session_id, i));
        let sem = semaphore.clone();
        let tx = progress_tx.clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.map_err(|e| format!("Semaphore error: {}", e))?;
            let resp = client.get(&url_str)
                .header(reqwest::header::RANGE, &range_header)
                .send()
                .await
                .map_err(|e| format!("Chunk {} download failed: {}", i, e))?;

            if !resp.status().is_success() {
                return Err::<(usize, PathBuf), String>(format!("Chunk {} download failed with status: {}", i, resp.status()));
            }

            let bytes = resp.bytes().await.map_err(|e| format!("Chunk {} read failed: {}", i, e))?;
            tokio::fs::write(&chunk_path, &bytes).await.map_err(|e| format!("Chunk {} write failed: {}", i, e))?;
            if let Some(ref tx) = tx {
                let _ = tx.try_send(format!("Downloaded chunk {}/{} (bytes {}-{})", i + 1, total_chunks, start, end)).ok();
            }
            info!("Chunk {}/{} downloaded (bytes {}-{})", i + 1, total_chunks, start, end);
            Ok::<(usize, PathBuf), String>((i, chunk_path))
        }));
    }

    let mut chunk_files: Vec<Option<PathBuf>> = vec![None; total_chunks];
    for handle in handles {
        match handle.await {
            Ok(Ok((idx, path))) => chunk_files[idx] = Some(path),
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(format!("Task join error: {}", e)),
        }
    }

    let mut out_file = tokio::fs::File::create(dest_path).await.map_err(|e| format!("Failed to create output file: {}", e))?;

    for (i, chunk_file) in chunk_files.into_iter().enumerate() {
        if let Some(chunk_path) = chunk_file {
            let data = tokio::fs::read(&chunk_path).await.map_err(|e| format!("Failed to read chunk {}: {}", i, e))?;
            out_file.write_all(&data).await.map_err(|e| format!("Failed to write chunk {} to output: {}", i, e))?;
            let _ = tokio::fs::remove_file(&chunk_path).await;
        }
    }

    out_file.flush().await.map_err(|e| format!("Failed to flush output: {}", e))?;
    if let Some(ref tx) = progress_tx {
        let _ = tx.try_send(format!("Download complete: {} chunks assembled, {} bytes to {}", total_chunks, file_size, dest_path.display())).ok();
    }
    info!("Chunked download completed: {} chunks assembled to {}, {} bytes", total_chunks, dest_path.display(), file_size);
    Ok(())
}

async fn single_upload(file_path: &Path, upload_url: &str, client: &reqwest::Client, progress_tx: &Option<tokio::sync::mpsc::Sender<String>>) -> Result<(), String> {
    let metadata = tokio::fs::metadata(file_path).await.map_err(|e| format!("Failed to get file metadata: {}", e))?;
    let file_size = metadata.len();
    let file_name = file_path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    if let Some(ref tx) = progress_tx {
        let _ = tx.try_send(format!("Uploading: {} ({} bytes) to {}", file_name, file_size, upload_url)).ok();
    }
    info!("Uploading {} ({} bytes) to {} via single upload", file_name, file_size, upload_url);
    let data = tokio::fs::read(file_path).await.map_err(|e| format!("Failed to read file: {}", e))?;

    let form = reqwest::multipart::Form::new()
        .part("file", reqwest::multipart::Part::bytes(data).file_name(file_name.clone()));

    let resp = client.post(upload_url).multipart(form).send().await.map_err(|e| format!("Failed to send file: {}", e))?;

    if resp.status().is_success() {
        if let Some(ref tx) = progress_tx {
            let _ = tx.try_send(format!("Upload complete: {} ({} bytes)", file_name, file_size)).ok();
        }
        info!("Single upload of {} completed successfully ({} bytes)", file_name, file_size);
        Ok(())
    } else {
        Err(format!("Upload failed with status: {}", resp.status()))
    }
}

async fn chunked_upload(file_path: &Path, upload_url: &str, client: &reqwest::Client, chunk_size: usize, max_concurrent: usize, progress_tx: &Option<tokio::sync::mpsc::Sender<String>>) -> Result<(), String> {
    let metadata = tokio::fs::metadata(file_path).await.map_err(|e| format!("Failed to get file metadata: {}", e))?;
    let file_size = metadata.len() as usize;

    if file_size <= chunk_size {
        return single_upload(file_path, upload_url, client, progress_tx).await;
    }

    let total_chunks = file_size.div_ceil(chunk_size);
    let filename = file_path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    if let Some(ref tx) = progress_tx {
        let _ = tx.try_send(format!("Uploading: {} ({} bytes, {} chunks) to {}", filename, file_size, total_chunks, upload_url)).ok();
    }
    info!("Chunked upload of {} ({} bytes, {} chunks, chunk size {}) to {}", filename, file_size, total_chunks, chunk_size, upload_url);

    let chunked_base = upload_url.replace("/client-upload/", "/chunked-upload/");

    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let mut handles = Vec::with_capacity(total_chunks);

    for i in 0..total_chunks {
        let start = i * chunk_size;
        let end = std::cmp::min(start + chunk_size, file_size);
        let chunk_url = format!("{}/chunk/{}", chunked_base, i);

        let client = client.clone();
        let fp = file_path.to_path_buf();
        let sem = semaphore.clone();
        let tx = progress_tx.clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.map_err(|e| format!("Semaphore error: {}", e))?;

            let mut file = tokio::fs::File::open(&fp).await.map_err(|e| format!("Failed to open file for chunk {}: {}", i, e))?;
            file.seek(SeekFrom::Start(start as u64)).await.map_err(|e| format!("Failed to seek for chunk {}: {}", i, e))?;

            let chunk_size_bytes = end - start;
            let mut chunk_data = vec![0u8; chunk_size_bytes];
            file.read_exact(&mut chunk_data).await.map_err(|e| format!("Failed to read chunk {}: {}", i, e))?;

            let resp = client.put(&chunk_url)
                .header("Content-Type", "application/octet-stream")
                .body(chunk_data)
                .send()
                .await
                .map_err(|e| format!("Chunk {} upload failed: {}", i, e))?;

            if !resp.status().is_success() {
                return Err::<usize, String>(format!("Chunk {} upload failed with status: {}", i, resp.status()));
            }

            if let Some(ref tx) = tx {
                let _ = tx.try_send(format!("Uploaded chunk {}/{} (bytes {}-{})", i + 1, total_chunks, start, end)).ok();
            }
            info!("Chunk {}/{} uploaded ({}-{} bytes)", i + 1, total_chunks, start, end);
            Ok::<usize, String>(i)
        }));
    }

    for handle in handles {
        match handle.await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(format!("Task join error: {}", e)),
        }
    }

    let complete_url = format!("{}/complete", chunked_base);
    let complete_body = serde_json::json!({
        "filename": filename,
        "total_chunks": total_chunks,
    });

    let resp = client.post(&complete_url)
        .json(&complete_body)
        .send()
        .await
        .map_err(|e| format!("Failed to complete chunked upload: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("Complete chunked upload failed with status: {}", resp.status()));
    }

    if let Some(ref tx) = progress_tx {
        let _ = tx.try_send(format!("Upload complete: {} ({} chunks, {} bytes)", filename, total_chunks, file_size)).ok();
    }
    info!("Chunked upload of {} completed: {} chunks, {} bytes total", filename, total_chunks, file_size);
    Ok(())
}

pub async fn handle_command(cmd: CommandPayload, tls_insecure: bool, chunk_size: usize, max_concurrent: usize, progress_tx: Option<tokio::sync::mpsc::Sender<String>>, compress_threshold: u64) -> CommandResult {
    match cmd {
        CommandPayload::ShellExec { cmd, args } => {
            info!("Executing shell command: {} {:?}", cmd, args);
            // Trim command just in case
            let cmd_trimmed = cmd.trim();
            
            let is_cd = if cfg!(target_os = "windows") {
                cmd_trimmed.eq_ignore_ascii_case("cd")
            } else {
                cmd_trimmed == "cd"
            };
            
            if is_cd {
                let default_path = if cfg!(target_os = "windows") {
                    std::env::var("USERPROFILE").unwrap_or("C:\\".to_string())
                } else {
                    std::env::var("HOME").unwrap_or("/".to_string())
                };
                
                let target_path_str = args.get(0).cloned().unwrap_or(default_path);
                let target_path = expand_path(&target_path_str);
                
                match std::env::set_current_dir(&target_path) {
                    Ok(_) => CommandResult::ShellOutput {
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: 0,
                        cwd: std::env::current_dir().unwrap_or_else(|_| target_path).to_string_lossy().to_string(),
                    },
                    Err(e) => CommandResult::ShellOutput {
                        stdout: String::new(),
                        stderr: format!("cd: failed to change directory to {}: {}\n", target_path.display(), e),
                        exit_code: 1,
                        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).to_string_lossy().to_string(),
                    },
                }
            } else {
                // Append original args if any (Note: for shell execution, args might need to be part of the command string or handled differently.
                // But for simple "run this program with these args" via shell, we usually just pass the whole command string to sh -c.
                // If args are present, they are likely arguments to the command 'cmd'.
                // If the user sent cmd="ls" and args=["-la"], we want `sh -c "ls -la"`.
                // So we should construct the full command line.
                
                let full_cmd = if args.is_empty() {
                    cmd_trimmed.to_string()
                } else {
                    format!("{} {}", cmd_trimmed, args.join(" "))
                };

                let (shell, shell_args) = if cfg!(target_os = "windows") {
                    ("cmd", vec!["/C", &full_cmd])
                } else {
                    ("sh", vec!["-c", &full_cmd])
                };

                match Command::new(shell)
                    .args(&shell_args)
                    // If we've changed directory via `cd`, subsequent commands should run in that dir.
                    // But `std::env::set_current_dir` already affects the whole process, so `Command::new` inherits it.
                    // However, if we are on Windows and using `cmd /C`, it might need explicit cwd if it was lost?
                    // Actually, `std::env::set_current_dir` is process-global, so it should persist.
                    // But let's verify if `cmd` resets it. `cmd /C` starts a new shell. 
                    // The new shell should inherit the parent process (client)'s CWD.
                    // So `cd` handling logic above:
                    // 1. `if cmd == "cd"` -> `std::env::set_current_dir`. This updates client process CWD.
                    // 2. Next command -> `Command::new` -> inherits client process CWD.
                    // So this *should* work.
                    // If it's not working on Windows, maybe there's a specific issue.
                    // Let's explicitly set current_dir just in case.
                    .current_dir(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                {
                    Ok(child) => {
                        match child.wait_with_output().await {
                            Ok(output) => {
                                let stdout = if cfg!(target_os = "windows") {
                                    // Try GBK first, then fallback to lossy UTF-8
                                    let (cow, _, _) = encoding_rs::GBK.decode(&output.stdout);
                                    cow.to_string()
                                } else {
                                    String::from_utf8_lossy(&output.stdout).to_string()
                                };
                                
                                let stderr = if cfg!(target_os = "windows") {
                                    let (cow, _, _) = encoding_rs::GBK.decode(&output.stderr);
                                    cow.to_string()
                                } else {
                                    String::from_utf8_lossy(&output.stderr).to_string()
                                };

                                CommandResult::ShellOutput {
                                    stdout,
                                    stderr,
                                    exit_code: output.status.code().unwrap_or(-1),
                                    cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).to_string_lossy().to_string(),
                                }
                            },
                            Err(e) => CommandResult::Error(format!("Failed to wait on child: {}", e)),
                        }
                    }
                    Err(e) => CommandResult::Error(format!("Failed to spawn shell: {}", e)),
                }
            }
        }
        CommandPayload::ChangeDir { path } => {
            let expanded = expand_path(&path);
            info!("Changing directory to: {} (expanded: {:?})", path, expanded);
            match std::env::set_current_dir(&expanded) {
                Ok(_) => CommandResult::DirChanged { new_path: expanded.to_string_lossy().to_string() },
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
             let expanded = expand_path(&path);
             info!("Listing directory: {} (expanded: {:?})", path, expanded);
             match std::fs::read_dir(&expanded) {
                 Ok(entries) => {
                     let mut files = Vec::new();
                     for entry in entries {
                         if let Ok(entry) = entry {
                             let metadata = entry.metadata().ok();
                             let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                             let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
                             let modified = metadata.as_ref().and_then(|m| m.modified().ok())
                                 .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs());

                             files.push(FileInfo {
                                 name: entry.file_name().to_string_lossy().to_string(),
                                 is_dir,
                                 size,
                                 modified,
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
            let client = match build_http_client(tls_insecure) {
                Ok(c) => c,
                Err(e) => return CommandResult::Error(e),
            };
            let dest = expand_path(&dest_path);

            match chunked_download(&url, &dest, &client, chunk_size, max_concurrent, &progress_tx).await {
                Ok(_) => {
                    // Decompress in-place if the downloaded file is gzip compressed
                    if let Err(e) = decompress_gzip_in_place(&dest) {
                        warn!("Failed to decompress downloaded file: {}", e);
                    }
                    CommandResult::Success(format!("File downloaded to {}", dest_path))
                },
                Err(e) => CommandResult::Error(e),
            }
        }
        CommandPayload::UploadFile { src_path, upload_url, compress } => {
            let expanded = expand_path(&src_path);
            let abs_path = if expanded.is_absolute() {
                expanded
            } else {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(&expanded)
            };

            let client = match build_http_client(tls_insecure) {
                Ok(c) => c,
                Err(e) => return CommandResult::Error(e),
            };

            // Only compress if the step has compress enabled AND file exceeds threshold
            let should_compress = compress.unwrap_or(false);
            let gzip_temp = if should_compress {
                let file_size = std::fs::metadata(&abs_path).map(|m| m.len()).unwrap_or(0);
                if file_size >= compress_threshold {
                    match gzip_file_to_temp(&abs_path) {
                        Ok(p) => {
                            info!("Uploading compressed (gzip) version of {} ({} bytes)", abs_path.display(), file_size);
                            Some(p)
                        },
                        Err(e) => {
                            warn!("Failed to gzip file for upload (falling back to uncompressed): {}", e);
                            None
                        }
                    }
                } else {
                    info!("File {} ({} bytes) below compress threshold ({}), skipping compression", abs_path.display(), file_size, compress_threshold);
                    None
                }
            } else {
                None
            };

            let upload_path = gzip_temp.as_ref().unwrap_or(&abs_path);
            let result = match chunked_upload(upload_path, &upload_url, &client, chunk_size, max_concurrent, &progress_tx).await {
                Ok(_) => CommandResult::Success("File uploaded successfully".to_string()),
                Err(e) => CommandResult::Error(e),
            };

            // Clean up temp gzip file if we created one
            if let Some(temp) = gzip_temp {
                let _ = std::fs::remove_file(&temp);
            }

            result
        }
        CommandPayload::UpdateClient { url } => {
            info!("Updating client from {}", url);
            let tls = tls_insecure;
            let chunk = chunk_size;
            let max_conc = max_concurrent;
            match download_and_replace(&url, tls, chunk, max_conc, &progress_tx).await {
                Ok(_) => {
                    info!("Client updated, restarting...");
                    std::process::exit(0);
                }
                Err(e) => {
                    error!("Update failed: {}", e);
                    CommandResult::Error(format!("Update failed: {}", e))
                },
            }
        }
        CommandPayload::ReadFile { path } => {
            info!("Reading file: {}", path);
            let expanded = expand_path(&path);
            match tokio::fs::read(&expanded).await {
                Ok(bytes) => {
                    // Try UTF-8 first
                    let (cow, _, had_errors) = encoding_rs::UTF_8.decode(&bytes);
                    if had_errors {
                        // If UTF-8 fails, try GBK (common for Windows/Chinese)
                        let (gbk_cow, _, _) = encoding_rs::GBK.decode(&bytes);
                        CommandResult::FileContent { content: gbk_cow.into_owned() }
                    } else {
                        CommandResult::FileContent { content: cow.into_owned() }
                    }
                },
                Err(e) => {
                    error!("Failed to read file: {}", e);
                    CommandResult::Error(format!("Failed to read file: {}", e))
                },
            }
        }
        CommandPayload::WriteFile { path, content } => {
            info!("Writing file: {}", path);
            let expanded = expand_path(&path);
            match tokio::fs::write(&expanded, content).await {
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
        CommandPayload::DownloadAndUnzip { url, dest_path } => {
            info!("Downloading and unzipping from {} to {}", url, dest_path);
            let client = match build_http_client(tls_insecure) {
                Ok(c) => c,
                Err(e) => return CommandResult::Error(e),
            };

            let temp_dir = std::env::temp_dir();
            let temp_zip = temp_dir.join(format!("roam_download_{}.zip", uuid::Uuid::new_v4()));

            if let Err(e) = chunked_download(&url, &temp_zip, &client, chunk_size, max_concurrent, &progress_tx).await {
                return CommandResult::Error(format!("Download failed: {}", e));
            }

            let dest = expand_path(&dest_path);
            let temp_zip_clone = temp_zip.clone();

            let res = tokio::task::spawn_blocking(move || {
                unzip_file(&temp_zip_clone, &dest)
            }).await;

            let _ = tokio::fs::remove_file(&temp_zip).await;

            match res {
                Ok(Ok(_)) => CommandResult::Success(format!("Directory downloaded and unzipped to {}", dest_path)),
                Ok(Err(e)) => CommandResult::Error(format!("Failed to unzip: {}", e)),
                Err(e) => CommandResult::Error(format!("Join error: {}", e)),
            }
        }
        CommandPayload::ZipAndUpload { src_path, upload_url } => {
            info!("Zipping and uploading {} to {}", src_path, upload_url);
            let src = expand_path(&src_path);
            if !src.exists() || !src.is_dir() {
                return CommandResult::Error(format!("Source directory does not exist or is not a directory: {}", src_path));
            }

            let temp_dir = std::env::temp_dir();
            let temp_zip = temp_dir.join(format!("roam_upload_{}.zip", uuid::Uuid::new_v4()));
            let temp_zip_clone = temp_zip.clone();
            let src_clone = src.clone();

            let zip_res = tokio::task::spawn_blocking(move || {
                zip_directory(&src_clone, &temp_zip_clone)
            }).await;

            match zip_res {
                Ok(Ok(_)) => {
                    let client = match build_http_client(tls_insecure) {
                        Ok(c) => c,
                        Err(e) => return CommandResult::Error(e),
                    };

                    let result = match chunked_upload(&temp_zip, &upload_url, &client, chunk_size, max_concurrent, &progress_tx).await {
                        Ok(_) => CommandResult::Success("Directory zipped and uploaded successfully".to_string()),
                        Err(e) => CommandResult::Error(format!("Upload failed: {}", e)),
                    };

                    let _ = tokio::fs::remove_file(&temp_zip).await;
                    result
                }
                Ok(Err(e)) => CommandResult::Error(format!("Failed to zip directory: {}", e)),
                Err(e) => CommandResult::Error(format!("Join error: {}", e)),
            }
        }
        CommandPayload::CopyFile { src_path, dest_path } => {
            info!("Copying file from {} to {}", src_path, dest_path);
            let src = expand_path(&src_path);
            let dest = expand_path(&dest_path);
            
            if src.is_dir() {
                 // Copy dir recursively? std::fs::copy is only for files.
                 // For now, let's implement simple recursive copy or use fs_extra if available?
                 // Since we want to keep dependencies low, let's just use walkdir/std::fs.
                 // Or, if it's a directory, maybe we should return error "Copy directory not supported yet" or implement it.
                 // Let's implement basic dir copy.
                 
                 // However, the command name is CopyFile. But user said "copy, move, delete files and folders".
                 // So we should support folders.
                 
                 // Recursive copy function
                 fn copy_dir_recursive(src: &PathBuf, dst: &PathBuf) -> std::io::Result<()> {
                     if !dst.exists() {
                         std::fs::create_dir_all(dst)?;
                     }
                     
                     for entry in std::fs::read_dir(src)? {
                         let entry = entry?;
                         let ty = entry.file_type()?;
                         let src_path = entry.path();
                         let dst_path = dst.join(entry.file_name());
                         
                         if ty.is_dir() {
                             copy_dir_recursive(&src_path, &dst_path)?;
                         } else {
                             std::fs::copy(&src_path, &dst_path)?;
                         }
                     }
                     Ok(())
                 }
                 
                 match copy_dir_recursive(&src, &dest) {
                     Ok(_) => CommandResult::Success(format!("Directory copied from {} to {}", src_path, dest_path)),
                     Err(e) => CommandResult::Error(format!("Failed to copy directory: {}", e)),
                 }
            } else {
                match std::fs::copy(&src, &dest) {
                    Ok(_) => CommandResult::Success(format!("File copied from {} to {}", src_path, dest_path)),
                    Err(e) => CommandResult::Error(format!("Failed to copy file: {}", e)),
                }
            }
        }
        CommandPayload::MoveFile { src_path, dest_path } => {
            info!("Moving file from {} to {}", src_path, dest_path);
            let src_expanded = expand_path(&src_path);
            let dest_expanded = expand_path(&dest_path);
            match std::fs::rename(&src_expanded, &dest_expanded) {
                Ok(_) => CommandResult::Success(format!("Moved from {} to {}", src_path, dest_path)),
                Err(e) => CommandResult::Error(format!("Failed to move: {}", e)),
            }
        }
        CommandPayload::DeleteFile { path } => {
            info!("Deleting {}", path);
            let p = expand_path(&path);
            if p.is_dir() {
                match std::fs::remove_dir_all(&p) {
                    Ok(_) => CommandResult::Success(format!("Directory deleted: {}", path)),
                    Err(e) => CommandResult::Error(format!("Failed to delete directory: {}", e)),
                }
            } else {
                match std::fs::remove_file(&p) {
                    Ok(_) => CommandResult::Success(format!("File deleted: {}", path)),
                    Err(e) => CommandResult::Error(format!("Failed to delete file: {}", e)),
                }
            }
        }
        CommandPayload::HttpRequest { method, url, headers, query_params, body } => {
            info!("Sending HTTP request: {} {}", method, url);
            let client = match build_http_client(tls_insecure) {
                Ok(c) => c,
                Err(e) => return CommandResult::Error(e),
            };

            let http_method = reqwest::Method::from_bytes(method.as_bytes())
                .unwrap_or(reqwest::Method::GET);

            let mut req = client.request(http_method, &url);

            for h in &headers {
                req = req.header(&h.key, &h.value);
            }

            for qp in &query_params {
                req = req.query(&[(qp.key.clone(), qp.value.clone())]);
            }

            if let Some(b) = &body {
                req = req.body(b.clone());
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let resp_headers: Vec<KeyValuePair> = resp.headers().iter()
                        .map(|(k, v)| KeyValuePair {
                            key: k.to_string(),
                            value: v.to_str().unwrap_or("").to_string(),
                        })
                        .collect();
                    let body_text = resp.text().await.unwrap_or_default();
                    let resp_headers_str: Vec<String> = resp_headers.iter()
                        .map(|h| format!("{}: {}", h.key, h.value))
                        .collect();
                    CommandResult::Success(format!(
                        "HTTP {} {}\n\nStatus: {}\n\nResponse Headers:\n{}\n\nBody:\n{}",
                        method, url, status, resp_headers_str.join("\n"), body_text
                    ))
                }
                Err(e) => CommandResult::Error(format!("HTTP request failed: {}", e)),
            }
        }
    }
}

async fn download_and_replace(url: &str, tls_insecure: bool, chunk_size: usize, max_concurrent: usize, progress_tx: &Option<tokio::sync::mpsc::Sender<String>>) -> anyhow::Result<()> {
    let client = build_http_client(tls_insecure).map_err(|e| anyhow::anyhow!(e))?;

    let mut temp_file = std::env::temp_dir();
    temp_file.push("roam_client_update");

    chunked_download(url, &temp_file, &client, chunk_size, max_concurrent, progress_tx).await
        .map_err(|e| anyhow::anyhow!("Download failed: {}", e))?;

    // Make executable on unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&temp_file)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&temp_file, perms)?;
    }

    self_replace::self_replace(&temp_file)?;

    let _ = fs::remove_file(&temp_file);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let args: Vec<String> = std::env::args().collect();
        let mut command = std::process::Command::new(&args[0]);
        command.args(&args[1..]);
        let err = command.exec();
        anyhow::bail!("Failed to restart process: {}", err);
    }

    #[cfg(windows)]
    {
        if std::env::var("ROAM_IS_SERVICE").unwrap_or_default() == "1" {
            std::process::exit(1);
        }

        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;

        let args: Vec<String> = std::env::args().collect();
        let exe_path = std::env::current_exe()?;

        std::process::Command::new(exe_path)
            .args(&args[1..])
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()?;

        std::process::exit(0);
    }

    #[cfg(not(any(unix, windows)))]
    {
        anyhow::bail!("Automatic restart not supported on this platform");
    }
}
