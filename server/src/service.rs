use service_manager::*;

pub fn install_service() -> anyhow::Result<()> {
    let label: ServiceLabel = "roam-server".parse()?;
    let manager = <dyn ServiceManager>::native()?;

    let exec_path = std::env::current_exe()?;
    // Use the directory of the executable as the working directory
    let working_dir = exec_path.parent()
        .ok_or_else(|| anyhow::anyhow!("Failed to get executable directory"))?
        .to_path_buf();

    manager.install(ServiceInstallCtx {
        label: label.clone(),
        program: exec_path,
        args: vec![],
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
