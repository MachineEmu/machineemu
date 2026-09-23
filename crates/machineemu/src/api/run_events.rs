use super::*;
use machineemu_core::domain::Run;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

pub(super) fn spawn(state: &AppState, run: Run) {
    let key = format!("{}:{}", run.instance_id.as_str(), run.run_id.as_str());
    let Ok(mut watchers) = state.run_watchers.lock() else {
        return;
    };
    if !watchers.insert(key.clone()) {
        return;
    }
    drop(watchers);
    let state = state.clone();
    tokio::spawn(async move {
        let qmp = qmp_reader(state.clone(), run.clone());
        let process = process_watcher(state.clone(), run.clone());
        tokio::join!(qmp, process);
        if let Ok(mut watchers) = state.run_watchers.lock() {
            watchers.remove(&key);
        }
    });
}

async fn process_watcher(state: AppState, run: Run) {
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    loop {
        ticker.tick().await;
        let observed = blocking({
            let state = state.clone();
            let run = run.clone();
            move || -> Result<bool, RuntimeError> {
                let lock = instance_lock(&state, run.instance_id.as_str())?;
                let _guard = lock.blocking_lock();
                let workspace = state
                    .workspace
                    .lock()
                    .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
                let recorded = workspace.run(&run.run_id)?;
                if !matches!(
                    recorded.status.as_str(),
                    "running" | "starting" | "uncertain"
                ) {
                    return Ok(true);
                }
                let running = state
                    .running
                    .lock()
                    .map_err(|_| RuntimeError::Process("running map lock poisoned".into()))?
                    .get(run.instance_id.as_str())
                    .cloned();
                let exit = if let Some(running) = &running {
                    let mut running = running.blocking_lock();
                    if running.run_id != run.run_id {
                        return Ok(true);
                    }
                    running.poll_exit()?
                } else if workspace.reconcile_run(&run.run_id)?.status == "uncertain" {
                    Some(machineemu_core::runtime::ProcessExit {
                        code: None,
                        success: false,
                    })
                } else {
                    None
                };
                let Some(exit) = exit else { return Ok(false) };
                streams::revoke_display(&state, run.instance_id.as_str(), run.run_id.as_str());
                let helper_cleanup = if let Some(mut helpers) = state
                    .helpers
                    .lock()
                    .map_err(|_| RuntimeError::Process("helper map lock poisoned".into()))?
                    .remove(run.instance_id.as_str())
                {
                    super::helpers::stop_all_checked(&mut helpers)
                } else {
                    Ok(())
                };
                if let Err(error) = helper_cleanup {
                    let final_run = workspace.finish_run(&run.run_id, "uncertain")?;
                    let instance = workspace.instance(&run.instance_id)?;
                    if matches!(
                        instance.state.as_str(),
                        "running" | "paused" | "starting" | "stopping"
                    ) {
                        let failed = workspace.transition_instance(&run.instance_id, "error")?;
                        events::publish_state(
                            &state,
                            &failed,
                            Some(&final_run),
                            "helper_cleanup_failed",
                        );
                    }
                    eprintln!(
                        "helper cleanup failed for {}: {error}",
                        run.instance_id.as_str()
                    );
                    return Ok(true);
                }
                state
                    .display_streams
                    .lock()
                    .map_err(|_| RuntimeError::Process("display stream map lock poisoned".into()))?
                    .remove(run.instance_id.as_str());
                state
                    .audio_sessions
                    .lock()
                    .map_err(|_| RuntimeError::Process("audio session map lock poisoned".into()))?
                    .retain(|_, session| session.instance_id != run.instance_id.as_str());
                state
                    .running
                    .lock()
                    .map_err(|_| RuntimeError::Process("running map lock poisoned".into()))?
                    .remove(run.instance_id.as_str());
                let status = if exit.success { "exited" } else { "failed" };
                let final_run = workspace.finish_run(&run.run_id, status)?;
                let mut instance = workspace.instance(&run.instance_id)?;
                if matches!(instance.state.as_str(), "running" | "paused") && exit.success {
                    instance = workspace.transition_instance(&run.instance_id, "stopping")?;
                    events::publish_state(&state, &instance, Some(&final_run), "process_exit");
                    instance = workspace.transition_instance(&run.instance_id, "stopped")?;
                } else if matches!(
                    instance.state.as_str(),
                    "running" | "paused" | "starting" | "stopping"
                ) {
                    instance = workspace.transition_instance(&run.instance_id, "error")?;
                }
                events::publish_state(
                    &state,
                    &instance,
                    Some(&final_run),
                    if exit.success {
                        "guest_shutdown"
                    } else {
                        "qemu_exit_failure"
                    },
                );
                instances::remove_if_disposable(
                    &state,
                    &workspace,
                    &run.instance_id,
                    Some(&run.run_id),
                    if exit.success {
                        "guest_shutdown"
                    } else {
                        "qemu_exit_failure"
                    },
                )?;
                Ok(true)
            }
        })
        .await;
        match observed {
            Ok(true) => return,
            Ok(false) => {}
            Err(_) => return,
        }
    }
}

async fn is_current(state: &AppState, run: &Run) -> bool {
    blocking({
        let state = state.clone();
        let run = run.clone();
        move || {
            let workspace = state
                .workspace
                .lock()
                .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
            Ok(workspace
                .active_run(&run.instance_id)?
                .is_some_and(|active| active.run_id == run.run_id))
        }
    })
    .await
    .unwrap_or(false)
}

fn run_is_alive(state: &AppState, workspace: &Workspace, run: &Run) -> Result<bool, RuntimeError> {
    let owned = state
        .running
        .lock()
        .map_err(|_| RuntimeError::Process("running map lock poisoned".into()))?
        .get(run.instance_id.as_str())
        .cloned();
    if let Some(owned) = owned {
        let mut owned = owned.blocking_lock();
        return Ok(owned.run_id == run.run_id && owned.poll_exit()?.is_none());
    }
    Ok(workspace.reconcile_run(&run.run_id)?.status == "running")
}

async fn read_frame(reader: &mut BufReader<tokio::net::UnixStream>) -> std::io::Result<Value> {
    let mut bytes = Vec::new();
    loop {
        let byte = reader.read_u8().await?;
        if byte == b'\n' {
            break;
        }
        if bytes.len() >= 1024 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "QMP frame too large",
            ));
        }
        bytes.push(byte);
    }
    serde_json::from_slice(&bytes).map_err(std::io::Error::other)
}

async fn command(
    reader: &mut BufReader<tokio::net::UnixStream>,
    name: &str,
    id: u64,
) -> std::io::Result<Value> {
    reader
        .get_mut()
        .write_all(format!("{{\"execute\":\"{name}\",\"id\":{id}}}\r\n").as_bytes())
        .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "QMP command timed out",
            ));
        }
        let message = tokio::time::timeout(remaining, read_frame(reader)).await??;
        if message.get("id") == Some(&Value::from(id)) {
            return message.get("return").cloned().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "QMP command failed")
            });
        }
    }
}

async fn qmp_reader(state: AppState, run: Run) {
    let mut delay = Duration::from_millis(250);
    while is_current(&state, &run).await {
        let attempt = async {
            let socket = tokio::time::timeout(
                Duration::from_secs(2),
                tokio::net::UnixStream::connect(&run.qmp_socket),
            )
            .await??;
            let mut reader = BufReader::new(socket);
            let greeting =
                tokio::time::timeout(Duration::from_secs(2), read_frame(&mut reader)).await??;
            if greeting.get("QMP").is_none() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "QMP greeting missing",
                ));
            }
            command(&mut reader, "qmp_capabilities", 1).await?;
            let status = command(&mut reader, "query-status", 2).await?;
            let observed = status
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            apply_status(&state, &run, &observed).await;
            delay = Duration::from_millis(250);
            let mut frame = Vec::new();
            loop {
                let byte =
                    match tokio::time::timeout(Duration::from_secs(1), reader.read_u8()).await {
                        Ok(result) => result?,
                        Err(_) => {
                            if !is_current(&state, &run).await {
                                return Ok(());
                            }
                            continue;
                        }
                    };
                if byte == b'\n' {
                    let message: Value =
                        serde_json::from_slice(&frame).map_err(std::io::Error::other)?;
                    frame.clear();
                    if message.get("event").is_some() {
                        apply_qmp(&state, &run, message).await;
                    }
                } else if frame.len() >= 1024 * 1024 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "QMP frame too large",
                    ));
                } else {
                    frame.push(byte);
                }
            }
        };
        let _ = attempt.await;
        if !is_current(&state, &run).await {
            return;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

async fn apply_status(state: &AppState, run: &Run, status: &str) {
    let target = match status {
        "running" => "running",
        "paused" | "prelaunch" => "paused",
        _ => return,
    };
    let state = state.clone();
    let run = run.clone();
    let _ = blocking(move || -> Result<_, RuntimeError> {
        let lock = instance_lock(&state, run.instance_id.as_str())?;
        let _guard = lock.blocking_lock();
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        if !workspace
            .active_run(&run.instance_id)?
            .is_some_and(|active| active.run_id == run.run_id)
            || !run_is_alive(&state, &workspace, &run)?
        {
            return Ok(());
        }
        let current = workspace.instance(&run.instance_id)?;
        if current.state != target
            && matches!(
                (current.state.as_str(), target),
                ("running", "paused") | ("paused", "running")
            )
        {
            let changed = workspace.transition_instance(&run.instance_id, target)?;
            events::publish_state(&state, &changed, Some(&run), "qmp_status");
        }
        Ok(())
    })
    .await;
}

fn filtered_fields(name: &str, data: &Value) -> Option<Value> {
    let field = |key: &str| {
        data.get(key).and_then(Value::as_str).filter(|value| {
            value.len() <= 128
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
                })
        })
    };
    match name {
        "SHUTDOWN" | "RESET" | "STOP" | "RESUME" | "POWERDOWN" => Some(json!({})),
        "DEVICE_DELETED" => Some(json!({"device": field("device")})),
        "BLOCK_IO_ERROR" => Some(
            json!({"device": field("device"), "action": field("action"), "operation": field("operation")}),
        ),
        _ => None,
    }
}

async fn apply_qmp(state: &AppState, run: &Run, message: Value) {
    let Some(name) = message.get("event").and_then(Value::as_str) else {
        return;
    };
    let Some(fields) = filtered_fields(name, &message["data"]) else {
        return;
    };
    let name = name.to_owned();
    let state = state.clone();
    let run = run.clone();
    let _ = blocking(move || -> Result<_, RuntimeError> {
        let lock = instance_lock(&state, run.instance_id.as_str())?;
        let _guard = lock.blocking_lock();
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        if !workspace
            .active_run(&run.instance_id)?
            .is_some_and(|active| active.run_id == run.run_id)
            || !run_is_alive(&state, &workspace, &run)?
        {
            return Ok(());
        }
        let current = workspace.instance(&run.instance_id)?;
        events::publish_qmp(
            &state,
            run.instance_id.as_str(),
            run.run_id.as_str(),
            &name,
            fields,
        );
        let target = match name.as_str() {
            "STOP" => "paused",
            "RESUME" => "running",
            _ => return Ok(()),
        };
        if current.state != target
            && matches!(
                (current.state.as_str(), target),
                ("running", "paused") | ("paused", "running")
            )
        {
            let changed = workspace.transition_instance(&run.instance_id, target)?;
            events::publish_state(&state, &changed, Some(&run), "qmp_event");
        }
        Ok(())
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use machineemu_core::domain::ImageManifest;

    fn fixture(name: &str) -> (std::path::PathBuf, AppState, Run, ManagedProcess) {
        let root =
            std::env::temp_dir().join(format!("machineemu-qmp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = Workspace::open(&root).unwrap();
        let image = ImageManifest {
            image_id: Id::new("image", "image01").unwrap(),
            engine_track: Id::new("track", "track01").unwrap(),
            supported_engine_tracks: vec![],
            target: "x86_64-softmmu".into(),
            disk_sha256: "a".repeat(64),
            firmware_sha256: None,
            tpm_state_sha256: None,
        };
        workspace.register_image(&image).unwrap();
        let id = Id::new("instance", "lab01").unwrap();
        workspace
            .create_instance(
                id.clone(),
                image.image_id,
                Id::new("profile", "profile01").unwrap(),
            )
            .unwrap();
        workspace.transition_instance(&id, "starting").unwrap();
        workspace.transition_instance(&id, "running").unwrap();
        let process = ManagedProcess::spawn(
            Id::new("run", "run01").unwrap(),
            &["sleep".into(), "10".into()],
            None,
            None,
        )
        .unwrap();
        let run = workspace
            .record_run(
                process.run_id.clone(),
                id,
                process.pid,
                process.process_start().unwrap(),
                root.join("qmp.sock"),
            )
            .unwrap();
        let state = AppState {
            workspace: Arc::new(Mutex::new(workspace)),
            bearer_token: Arc::from("secret"),
            launch_plans: Arc::new(BTreeMap::new()),
            running: Arc::new(Mutex::new(BTreeMap::new())),
            instance_locks: Arc::new(Mutex::new(BTreeMap::new())),
            helpers: Arc::new(Mutex::new(BTreeMap::new())),
            display_streams: Arc::new(Mutex::new(BTreeMap::new())),
            display_stream: Arc::new(PathBuf::from("display-stream")),
            stream_tickets: Arc::new(Mutex::new(BTreeMap::new())),
            audio_sessions: Arc::new(Mutex::new(BTreeMap::new())),
            control_streams: Arc::new(Mutex::new(BTreeMap::new())),
            events: Arc::new(Mutex::new(events::EventHub::new().unwrap())),
            run_watchers: Arc::new(Mutex::new(std::collections::BTreeSet::new())),
            guest_executions: Arc::new(Mutex::new(BTreeMap::new())),
            local_unix: false,
        };
        (root, state, run, process)
    }

    #[tokio::test]
    async fn guest_qmp_observations_update_state_and_ignore_stale_runs() {
        let (root, state, run, mut process) = fixture("events");
        let id = Id::new("instance", "lab01").unwrap();
        apply_qmp(
            &state,
            &run,
            json!({"event":"STOP","data":{"secret":"discard"}}),
        )
        .await;
        assert_eq!(
            state.workspace.lock().unwrap().instance(&id).unwrap().state,
            "paused"
        );
        apply_qmp(&state, &run, json!({"event":"RESUME"})).await;
        assert_eq!(
            state.workspace.lock().unwrap().instance(&id).unwrap().state,
            "running"
        );
        assert_eq!(
            filtered_fields("STOP", &json!({"secret":"discard"})),
            Some(json!({}))
        );
        state
            .workspace
            .lock()
            .unwrap()
            .finish_run(&run.run_id, "failed")
            .unwrap();
        apply_qmp(&state, &run, json!({"event":"STOP"})).await;
        assert_eq!(
            state.workspace.lock().unwrap().instance(&id).unwrap().state,
            "running"
        );
        process.terminate().unwrap();
        process.wait().unwrap();
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn qmp_reader_reconnects_and_reconciles_before_later_events() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let (root, state, run, mut process) = fixture("reconnect");
        let listener = UnixListener::bind(&run.qmp_socket).unwrap();
        let server = std::thread::spawn(move || {
            for connection in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream.write_all(b"{\"QMP\":{}}\r\n").unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                for _ in 0..2 {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let request: Value = serde_json::from_str(line.trim()).unwrap();
                    let result = if request["execute"] == "query-status" {
                        json!({"status":"running"})
                    } else {
                        json!({})
                    };
                    stream
                        .write_all(
                            format!("{{\"return\":{result},\"id\":{}}}\r\n", request["id"])
                                .as_bytes(),
                        )
                        .unwrap();
                }
                stream.write_all(b"{\"event\":\"STOP\"}\r\n").unwrap();
                std::thread::sleep(Duration::from_millis(100));
                if connection == 1 {
                    stream.write_all(b"{\"event\":\"RESUME\"}\r\n").unwrap();
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        let reader = tokio::spawn(qmp_reader(state.clone(), run.clone()));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let instance = state
                    .workspace
                    .lock()
                    .unwrap()
                    .instance(&run.instance_id)
                    .unwrap();
                if instance.revision >= 7 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            state
                .workspace
                .lock()
                .unwrap()
                .instance(&run.instance_id)
                .unwrap()
                .state,
            "running"
        );
        reader.abort();
        server.join().unwrap();
        process.terminate().unwrap();
        process.wait().unwrap();
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn process_exit_publishes_terminal_state_after_cleanup() {
        let (root, state, run, mut process) = fixture("exit");
        let watcher = tokio::spawn(process_watcher(state.clone(), run.clone()));
        process.terminate().unwrap();
        process.wait().unwrap();
        tokio::time::timeout(Duration::from_secs(3), watcher)
            .await
            .unwrap()
            .unwrap();
        let workspace = state.workspace.lock().unwrap();
        assert_eq!(workspace.run(&run.run_id).unwrap().status, "failed");
        assert_eq!(workspace.instance(&run.instance_id).unwrap().state, "error");
        drop(workspace);
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn disposable_instance_is_removed_after_unexpected_exit() {
        let (root, state, run, mut process) = fixture("disposable-exit");
        state
            .workspace
            .lock()
            .unwrap()
            .save_instance_launch(
                &run.instance_id,
                r#"{"argv":[],"qmp_socket":"qmp.sock"}"#,
                true,
            )
            .unwrap();
        let watcher = tokio::spawn(process_watcher(state.clone(), run.clone()));
        process.terminate().unwrap();
        process.wait().unwrap();
        tokio::time::timeout(Duration::from_secs(3), watcher)
            .await
            .unwrap()
            .unwrap();
        let workspace = state.workspace.lock().unwrap();
        assert!(workspace.instance(&run.instance_id).is_err());
        assert_eq!(
            workspace
                .instance_tombstone(&run.instance_id)
                .unwrap()
                .unwrap()
                .1,
            "qemu_exit_failure"
        );
        drop(workspace);
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }
}
