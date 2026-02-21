use service_manager::*;

pub fn install_service() -> anyhow::Result<()> {
    let label: ServiceLabel = "roam-server".parse()?;
    let manager = <dyn ServiceManager>::native()?;

    let exec_path = std::env::current_exe()?;
    // Use the directory of the executable as the working directory
    let working_dir = exec_path.parent()
        .ok_or_else(|| anyhow::anyhow!("Failed to get executable directory"))?
        .to_path_buf();

    // Copy .env, cert.pem, key.pem if they exist in current directory
    let current_dir = std::env::current_dir()?;
    let files_to_copy = vec![".env", "cert.pem", "key.pem"];
    
    for filename in files_to_copy {
        let src = current_dir.join(filename);
        let dst = working_dir.join(filename);
        
        // Don't copy if src and dst are the same file
        if src == dst {
            continue;
        }

        if src.exists() {
            match std::fs::copy(&src, &dst) {
                Ok(_) => println!("Copied {} to service directory: {}", filename, dst.display()),
                Err(e) => eprintln!("Warning: Failed to copy {} to {}: {}", filename, dst.display(), e),
            }
        }
    }

    #[cfg(windows)]
    let args = vec!["run-service".into()];
    #[cfg(not(windows))]
    let args = vec![];

    manager.install(ServiceInstallCtx {
        label: label.clone(),
        program: exec_path,
        args,
        contents: None,
        username: None, 
        working_directory: Some(working_dir),
        environment: None,
        autostart: true,
        restart_policy: service_manager::RestartPolicy::Always { delay_secs: Some(10) },
    })?;
    
    println!("Service 'roam-server' installed successfully.");
    println!("You can now start it with: roam-server start (or systemctl start roam-server / net start roam-server)");
    Ok(())
}

pub fn uninstall_service() -> anyhow::Result<()> {
    let label: ServiceLabel = "roam-server".parse()?;
    let manager = <dyn ServiceManager>::native()?;

    manager.uninstall(ServiceUninstallCtx {
        label: label.clone(),
    })?;

    println!("Service 'roam-server' uninstalled successfully.");
    Ok(())
}

pub fn start_service() -> anyhow::Result<()> {
     let label: ServiceLabel = "roam-server".parse()?;
     let manager = <dyn ServiceManager>::native()?;
     
     manager.start(ServiceStartCtx {
         label: label.clone(),
     })?;
     println!("Service 'roam-server' started.");
     Ok(())
}

pub fn stop_service() -> anyhow::Result<()> {
     let label: ServiceLabel = "roam-server".parse()?;
     let manager = <dyn ServiceManager>::native()?;
     
     manager.stop(ServiceStopCtx {
         label: label.clone(),
     })?;
     println!("Service 'roam-server' stopped.");
     Ok(())
}

#[cfg(windows)]
windows_service::define_windows_service!(ffi_service_main, my_service_main);

#[cfg(windows)]
fn my_service_main(_arguments: Vec<std::ffi::OsString>) {
    if let Err(e) = run_service_logic() {
         eprintln!("Service error: {:?}", e);
    }
}

#[cfg(windows)]
pub fn run_windows_service() -> anyhow::Result<()> {
    windows_service::service_dispatcher::start("roam-server", ffi_service_main)
        .map_err(|e| anyhow::anyhow!("Service dispatcher failed: {}", e))
}

#[cfg(windows)]
fn run_service_logic() -> anyhow::Result<()> {
    use windows_service::{
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
    };
    use std::time::Duration;

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop => {
                // Signal stop
                // For now, we can just exit process as it's simple
                std::process::exit(0);
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register("roam-server", event_handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: std::time::Duration::default(),
        process_id: None,
    })?;

    let rt = tokio::runtime::Runtime::new()?;
    // This blocks until app::run returns (or process exits via stop handler)
    let result = rt.block_on(crate::app::run());

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: std::time::Duration::default(),
        process_id: None,
    })?;

    result
}
