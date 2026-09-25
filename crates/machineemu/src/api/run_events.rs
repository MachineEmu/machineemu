use super::*;
use machineemu_core::domain::Run;
use serde_json::{Value, json};
use std::time::Duration;

pub(super) fn spawn(state: &AppState, run: Run) {
    let Ok(mut owners) = state.supervisors.lock() else {
        return;
    };
    let owner = owners
        .entry(run.instance_id.as_str().into())
        .or_insert_with(|| supervisor::RunSupervisor::new(run.run_id.clone()));
    if owner.run_id != run.run_id || owner.watching {
        return;
    }
    owner.watching = true;
    drop(owners);
    let state = state.clone();
    tokio::spawn(async move {
        tokio::join!(
            qmp_reader(state.clone(), run.clone()),
            process_watcher(state.clone(), run.clone())
        );
        if let Ok(mut owners) = state.supervisors.lock()
            && let Some(owner) = owners.get_mut(run.instance_id.as_str())
            && owner.run_id == run.run_id
        {
            owner.watching = false;
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
                let running = supervisor::connection(&state, run.instance_id.as_str())?;
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
                let helper_cleanup =
                    supervisor::teardown(&state, run.instance_id.as_str(), &run.run_id);
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
                    tracing::warn!(
                        instance = run.instance_id.as_str(),
                        %error,
                        "helper cleanup failed"
                    );
                    return Ok(true);
                }
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
    let owned = supervisor::connection(state, run.instance_id.as_str())?;
    if let Some(owned) = owned {
        let mut owned = owned.blocking_lock();
        return Ok(owned.run_id == run.run_id && owned.poll_exit()?.is_none());
    }
    Ok(workspace.reconcile_run(&run.run_id)?.status == "running")
}

async fn qmp_reader(state: AppState, run: Run) {
    let mut delay = Duration::from_millis(250);
    while is_current(&state, &run).await {
        let observed = async {
            let gate = instance_lock(&state, run.instance_id.as_str())?;
            let guard = gate.lock_owned().await;
            let mut qmp = supervisor::qmp(&state, &run).await?;
            let reply = qmp.execute("query-status", Value::Null).await;
            let events = qmp.drain_events().collect::<Vec<_>>();
            drop(qmp);
            let status = match reply {
                Ok(status) => status,
                Err(error) => {
                    supervisor::disconnect(&state, run.instance_id.as_str(), &run.run_id)?;
                    return Err(error);
                }
            };
            let status = status
                .get("status")
                .and_then(Value::as_str)
                .ok_or_else(|| RuntimeError::Qmp("query-status response has no status".into()))?
                .to_owned();
            let state = state.clone();
            let run = run.clone();
            blocking(move || {
                // Keep the instance gate through publication so a stale query
                // cannot overwrite a concurrent pause/resume response.
                let _guard = guard;
                for event in events {
                    apply_qmp_locked(&state, &run, event)?;
                }
                apply_status_locked(&state, &run, &status)
            })
            .await
        }
        .await;
        delay = if observed.is_ok() {
            Duration::from_millis(250)
        } else {
            (delay * 2).min(Duration::from_secs(5))
        };
        tokio::time::sleep(delay).await;
    }
}

fn apply_status_locked(state: &AppState, run: &Run, status: &str) -> Result<(), RuntimeError> {
    let target = match status {
        "running" => "running",
        "paused" | "prelaunch" => "paused",
        _ => return Ok(()),
    };
    let workspace = state
        .workspace
        .lock()
        .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
    if !workspace
        .active_run(&run.instance_id)?
        .is_some_and(|active| active.run_id == run.run_id)
        || !run_is_alive(state, &workspace, run)?
    {
        return Ok(());
    }
    let current = workspace.instance(&run.instance_id)?;
    let changed = workspace.observe_run_status(run, target)?;
    if changed.revision != current.revision {
        events::publish_state(state, &changed, Some(run), "qmp_status");
    }
    Ok(())
}

#[cfg(test)]
async fn apply_status(state: &AppState, run: &Run, status: &str) {
    let state = state.clone();
    let run = run.clone();
    let status = status.to_owned();
    blocking(move || {
        let gate = instance_lock(&state, run.instance_id.as_str())?;
        let _guard = gate.blocking_lock();
        apply_status_locked(&state, &run, &status)
    })
    .await
    .unwrap();
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

fn apply_qmp_locked(state: &AppState, run: &Run, message: Value) -> Result<(), RuntimeError> {
    let Some(name) = message.get("event").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(fields) = filtered_fields(name, &message["data"]) else {
        return Ok(());
    };
    let name = name.to_owned();
    let workspace = state
        .workspace
        .lock()
        .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
    if !workspace
        .active_run(&run.instance_id)?
        .is_some_and(|active| active.run_id == run.run_id)
        || !run_is_alive(state, &workspace, run)?
    {
        return Ok(());
    }
    let current = workspace.instance(&run.instance_id)?;
    events::publish_qmp(
        state,
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
        events::publish_state(state, &changed, Some(run), "qmp_event");
    }
    Ok(())
}

#[cfg(test)]
async fn apply_qmp(state: &AppState, run: &Run, message: Value) {
    let state = state.clone();
    let run = run.clone();
    blocking(move || {
        let gate = instance_lock(&state, run.instance_id.as_str())?;
        let _guard = gate.blocking_lock();
        apply_qmp_locked(&state, &run, message)
    })
    .await
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use machineemu_core::domain::ImageManifest;

    pub(super) fn fixture(name: &str) -> (std::path::PathBuf, AppState, Run, ManagedProcess) {
        let root =
            std::env::temp_dir().join(format!("machineemu-qmp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = Workspace::open(&root).unwrap();
        let image = ImageManifest {
            image_id: Id::new("image", "image01").unwrap(),
            engine_track: Id::new("track", "track01").unwrap(),
            supported_engine_tracks: vec![],
            target: "x86_64-softmmu".into(),
            components: std::collections::BTreeMap::new(),
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
            supervisors: Arc::new(Mutex::new(BTreeMap::new())),
            instance_locks: Arc::new(Mutex::new(BTreeMap::new())),

            display_stream: Arc::new(PathBuf::from("display-stream")),
            stream_tickets: Arc::new(Mutex::new(BTreeMap::new())),
            audio_sessions: Arc::new(Mutex::new(BTreeMap::new())),
            control_streams: Arc::new(Mutex::new(BTreeMap::new())),
            events: Arc::new(Mutex::new(events::EventHub::new().unwrap())),
            image_imports: Arc::new(Mutex::new(BTreeMap::new())),

            guest_executions: Arc::new(Mutex::new(BTreeMap::new())),
            helpers: Arc::new(HelperConfig::default()),
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
                        {
                            let event = if connection == 0 { "STOP" } else { "RESUME" };
                            stream
                                .write_all(format!("{}\r\n", json!({"event":event})).as_bytes())
                                .unwrap();
                            json!({"status":if connection == 0 { "paused" } else { "running" }})
                        }
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
                if instance.revision >= 5 {
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
            .set_domain_document(
                &run.instance_id,
                serde_json::json!({
                    "api_version": "machineemu.io/v1",
                    "kind": "Instance",
                    "metadata": {"name": run.instance_id.as_str(), "revision": 1},
                    "spec": {"engine": {"track": "track01", "build_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "executable": "qemu"}}
                }),
            )
            .unwrap();
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

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[tokio::test]
    async fn qmp_observation_repairs_error_and_starting_but_rejects_stale_runs() {
        let (root, state, run, mut process) = super::tests::fixture("recovery-state");
        state
            .workspace
            .lock()
            .unwrap()
            .transition_instance(&run.instance_id, "error")
            .unwrap();
        apply_status(&state, &run, "running").await;
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
        state
            .workspace
            .lock()
            .unwrap()
            .transition_instance(&run.instance_id, "error")
            .unwrap();
        apply_status(&state, &run, "prelaunch").await;
        assert_eq!(
            state
                .workspace
                .lock()
                .unwrap()
                .instance(&run.instance_id)
                .unwrap()
                .state,
            "paused"
        );
        let mut stale = run.clone();
        stale.run_id = Id::new("run", "old-run").unwrap();
        apply_status(&state, &stale, "running").await;
        assert_eq!(
            state
                .workspace
                .lock()
                .unwrap()
                .instance(&run.instance_id)
                .unwrap()
                .state,
            "paused"
        );
        process.terminate().unwrap();
        process.wait().unwrap();
        apply_status(&state, &run, "running").await;
        assert_eq!(
            state
                .workspace
                .lock()
                .unwrap()
                .instance(&run.instance_id)
                .unwrap()
                .state,
            "paused"
        );
        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }
}
