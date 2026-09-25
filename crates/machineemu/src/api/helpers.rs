use super::*;
use std::{
    os::unix::fs::FileTypeExt,
    time::{Duration, Instant},
};

pub(super) async fn spawn(
    root: &std::path::Path,
    instance: &Id,
    run: &Id,
    spec: &dto::HelperSpec,
) -> Result<ManagedProcess, RuntimeError> {
    let name = Id::new("helper", spec.name.clone())?;
    if spec.argv.is_empty() {
        return Err(RuntimeError::Process(format!(
            "helper {name:?} has no command"
        )));
    }
    let directory = root.join("instances").join(instance.as_str());
    let canonical = directory
        .canonicalize()
        .map_err(|source| RuntimeError::Io {
            path: directory.clone(),
            source,
        })?;
    let workspace = root.canonicalize().map_err(|source| RuntimeError::Io {
        path: root.to_owned(),
        source,
    })?;
    if !canonical.starts_with(&workspace) {
        return Err(RuntimeError::Process(
            "helper instance directory escapes the workspace".into(),
        ));
    }
    let ready = spec
        .ready_socket
        .as_ref()
        .map(|relative| {
            let prefix = PathBuf::from("instances").join(instance.as_str());
            if relative.parent() != Some(prefix.as_path()) {
                return Err(RuntimeError::Process(format!(
                    "helper {} readiness socket must belong to its instance",
                    name.as_str()
                )));
            }
            Ok(root.join(relative))
        })
        .transpose()?;
    if let Some(path) = &ready
        && path.exists()
    {
        return Err(RuntimeError::Process(format!(
            "helper {} readiness socket already exists: {}",
            name.as_str(),
            path.display()
        )));
    }
    let log = directory.join(format!("helper-{}.log", name.as_str()));
    let helper_id = Id::new("run", format!("{}-{}", run.as_str(), name.as_str()))?;
    let mut process = ManagedProcess::spawn_systemd_scope(helper_id, &spec.argv, None, Some(&log))?;
    if let Some(path) = ready {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(metadata) = std::fs::symlink_metadata(&path)
                && metadata.file_type().is_socket()
            {
                if process.try_wait()?.is_some() {
                    return Err(RuntimeError::Process(format!(
                        "helper {} exited during startup; inspect {}",
                        name.as_str(),
                        log.display()
                    )));
                }
                break;
            }
            if process.try_wait()?.is_some() || Instant::now() >= deadline {
                let _ = process.terminate_gracefully();
                return Err(RuntimeError::Process(format!(
                    "helper {} did not become ready; inspect {}",
                    name.as_str(),
                    log.display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    Ok(process)
}

pub(super) fn stop_all(processes: &mut Vec<ManagedProcess>) {
    let _ = stop_all_checked(processes);
}

pub(super) fn stop_all_checked(processes: &mut Vec<ManagedProcess>) -> Result<(), RuntimeError> {
    let mut first_error = None;
    while let Some(mut process) = processes.pop() {
        if let Err(error) = process.terminate_gracefully() {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

pub(super) async fn wait_bluetooth_attached(
    state: &AppState,
    instance: &Id,
    process: &mut ManagedProcess,
) -> Result<(), RuntimeError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if process.try_wait()?.is_some() {
            return Err(RuntimeError::Process(
                "Bluetooth simulator exited before attaching to QEMU".into(),
            ));
        }
        let state = state.clone();
        let instance_id = instance.as_str().to_owned();
        if blocking(move || {
            super::helper_control::request(
                &state,
                &instance_id,
                "bluetooth",
                serde_json::json!({"version":1,"type":"stats"}),
            )
        })
        .await
        .ok()
        .is_some_and(|value| value["attached"] == true)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(RuntimeError::Process(
                "Bluetooth simulator did not attach to QEMU".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn named_helper_waits_for_owned_socket_and_rejects_escape() {
        let root = std::env::temp_dir().join(format!("machineemu-helper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("instances/lab")).unwrap();
        let instance = Id::new("instance", "lab").unwrap();
        let run = Id::new("run", "run01").unwrap();
        let socket = root.join("instances/lab/ready.sock");
        let mut spec = dto::HelperSpec {
            name: "fixture".into(),
            argv: vec!["python3".into(), "-c".into(), "import socket,sys,time; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(); time.sleep(30)".into(), socket.to_string_lossy().into_owned()],
            after_qemu: true,
            ready_socket: Some(PathBuf::from("instances/lab/ready.sock")),
        };
        let process = spawn(&root, &instance, &run, &spec).await.unwrap();
        assert!(socket.exists());
        let mut processes = vec![process];
        stop_all(&mut processes);
        spec.ready_socket = Some(PathBuf::from("instances/lab/../other.sock"));
        assert!(spawn(&root, &instance, &run, &spec).await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
