use service_manager::*;

pub fn install_service() -> anyhow::Result<()> {
    let label: ServiceLabel = "roam-client".parse()?;
    let manager = <dyn ServiceManager>::native()?;

    let exec_path = std::env::current_exe()?;
    // Use the directory of the executable as the working directory
    let working_dir = exec_path.parent()
        .ok_or_else(|| anyhow::anyhow!("Failed to get executable directory"))?
        .to_path_buf();

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

    #[cfg(windows)]
    {
        // Explicitly set recovery options using sc.exe because service-manager might not set them fully
        // reset= 86400 (reset fail count after 1 day)
        // actions= restart/10000/restart/10000/restart/10000 (restart after 10s for 1st, 2nd, and subsequent failures)
        let status = std::process::Command::new("sc")
            .args(&["failure", "roam-client", "reset=", "86400", "actions=", "restart/10000/restart/10000/restart/10000"])
            .status();
            
        match status {
            Ok(s) if s.success() => println!("Windows service recovery options configured."),
            Ok(s) => eprintln!("Failed to configure recovery options: exit code {}", s),
            Err(e) => eprintln!("Failed to execute sc command: {}", e),
        }
    }
    
    println!("Service 'roam-client' installed successfully.");
    println!("You can now start it with: roam-client start (or systemctl start roam-client / net start roam-client)");
    Ok(())
}

pub fn uninstall_service() -> anyhow::Result<()> {
    let label: ServiceLabel = "roam-client".parse()?;
    let manager = <dyn ServiceManager>::native()?;

    manager.uninstall(ServiceUninstallCtx {
        label: label.clone(),
    })?;

    println!("Service 'roam-client' uninstalled successfully.");
    Ok(())
}

pub fn start_service() -> anyhow::Result<()> {
     let label: ServiceLabel = "roam-client".parse()?;
     let manager = <dyn ServiceManager>::native()?;
     
     manager.start(ServiceStartCtx {
         label: label.clone(),
     })?;
     println!("Service 'roam-client' started.");
     Ok(())
}

pub fn stop_service() -> anyhow::Result<()> {
     let label: ServiceLabel = "roam-client".parse()?;
     let manager = <dyn ServiceManager>::native()?;
     
     manager.stop(ServiceStopCtx {
         label: label.clone(),
     })?;
     println!("Service 'roam-client' stopped.");
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
    windows_service::service_dispatcher::start("roam-client", ffi_service_main)
        .map_err(|e| anyhow::anyhow!("Service dispatcher failed: {}", e))
}

#[cfg(windows)]
fn run_service_logic() -> anyhow::Result<()> {
    // Mark as running as service so update handler knows how to restart
    std::env::set_var("ROAM_IS_SERVICE", "1");

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

    let status_handle = service_control_handler::register("roam-client", event_handler)?;

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
