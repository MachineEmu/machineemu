use super::launch::{plan_paths, prepare_paths};
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_LIFECYCLE_ID: AtomicU64 = AtomicU64::new(1);

fn lifecycle_id(prefix: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    format!(
        "{prefix}-{now:x}-{:x}",
        NEXT_LIFECYCLE_ID.fetch_add(1, Ordering::Relaxed)
    )
}

async fn refresh_vnc_port(argv: &mut [String], auto: bool) -> Result<Option<u16>, RuntimeError> {
    let display = argv
        .windows(2)
        .position(|pair| pair[0] == "-display" && pair[1].starts_with("vnc=127.0.0.1:"));
    let Some(index) = display else {
        if auto {
            return Err(RuntimeError::Process(
                "automatic VNC port requested without a local VNC display".into(),
            ));
        }
        return Ok(None);
    };
    let setting = &argv[index + 1];
    let prefix = "vnc=127.0.0.1:";
    let tail = &setting[prefix.len()..];
    let (display_number, suffix) = tail.split_once(',').unwrap_or((tail, ""));
    let number: u16 = display_number
        .parse()
        .map_err(|_| RuntimeError::Process("invalid local VNC display number".into()))?;
    let port = number
        .checked_add(5900)
        .ok_or_else(|| RuntimeError::Process("invalid local VNC port".into()))?;
    if !(5900..=5999).contains(&port) {
        return Err(RuntimeError::Process(
            "local VNC port must be 5900–5999".into(),
        ));
    }
    let candidates = if auto { 5900..=5999 } else { port..=port };
    for candidate in candidates {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", candidate)).await {
            drop(listener);
            if auto {
                argv[index + 1] = format!(
                    "{prefix}{}{}",
                    candidate - 5900,
                    if suffix.is_empty() {
                        String::new()
                    } else {
                        format!(",{suffix}")
                    }
                );
            }
            return Ok(Some(candidate));
        }
    }
    Err(RuntimeError::Process(if auto {
        "no free VNC port in 5900–5999".into()
    } else {
        format!("VNC port {port} is already in use")
    }))
}

#[allow(clippy::too_many_arguments)]
async fn rollback_failed_start(
    state: &AppState,
    workspace: &mut Workspace,
    instance_id: &Id,
    run_id: &Id,
    operation_id: &Id,
    running: &mut machineemu_core::runtime::AsyncRunningInstance,
    helpers: &mut Vec<ManagedProcess>,
    error: &RuntimeError,
) {
    if workspace
        .stop_instance_async(instance_id, running)
        .await
        .is_err()
    {
        let _ = running.abort_owned_child();
        let _ = workspace.finish_run(run_id, "failed");
        if let Ok(instance) = workspace.instance(instance_id)
            && matches!(
                instance.state.as_str(),
                "starting" | "running" | "paused" | "stopping"
            )
        {
            let _ = workspace.transition_instance(instance_id, "error");
        }
    }
    super::helpers::stop_all(helpers);
    if let Ok(failed) = workspace.fail_operation(operation_id, &error.to_string()) {
        events::publish_operation(state, &failed, Some(run_id.as_str()));
    }
    if let Ok(instance) = workspace.instance(instance_id) {
        let run = workspace.run(run_id).ok();
        events::publish_state(state, &instance, run.as_ref(), "start_failed");
    }
}

pub(super) async fn start_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(input): axum::Json<StartInstance>,
) -> impl IntoResponse {
    start_instance_with_lock(state, headers, id, input, false).await
}

async fn start_instance_with_lock(
    state: AppState,
    headers: HeaderMap,
    id: String,
    input: StartInstance,
    lock_held: bool,
) -> axum::response::Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let watch_state = state.clone();
    let cleanup_state = state.clone();
    let cleanup_id = id.clone();
    let future = async move {
        let input_json = serde_json::to_string(&input)
            .map_err(|error| RuntimeError::Process(error.to_string()))?;
        let instance_id = Id::new("instance", id)?;
        let instance_lock = instance_lock(&state, instance_id.as_str())?;
        let _instance_guard = if lock_held {
            None
        } else {
            Some(instance_lock.lock_owned().await)
        };
        let operation_id = Id::new(
            "operation",
            input
                .operation_id
                .clone()
                .unwrap_or_else(|| lifecycle_id("op")),
        )?;
        let run_id = Id::new(
            "run",
            input.run_id.clone().unwrap_or_else(|| lifecycle_id("run")),
        )?;
        let idempotency_key = input
            .idempotency_key
            .clone()
            .unwrap_or_else(|| format!("start-{}", run_id.as_str()));
        let mut workspace = {
            let owner = state
                .workspace
                .lock()
                .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
            owner.attach()?
        };
        let instance = workspace.instance(&instance_id)?;
        let plan = if let Some(plan) = &input.launch_plan {
            plan.clone()
        } else if let Some((saved, _)) = workspace.instance_launch(&instance_id)? {
            serde_json::from_str::<LaunchSpec>(&saved)?
        } else {
            state
                .launch_plans
                .get(instance.profile_id.as_str())
                .cloned()
                .ok_or_else(|| {
                    RuntimeError::Process(format!(
                        "no saved launch plan for instance {}",
                        instance_id.as_str()
                    ))
                })?
        };
        let (qmp, stdout, stderr) = plan_paths(workspace.root(), &plan)?;
        let workspace_root = workspace.root().to_owned();
        if let Some(preparation) = &plan.preparation {
            let (backing, nvram, tpm) = prepare_paths(workspace.root(), preparation)?;
            workspace.prepare_instance_files_sized(
                &instance_id,
                &backing,
                &preparation.backing_format,
                preparation.disk_size.as_deref(),
                nvram.as_deref(),
                tpm.as_deref(),
            )?;
        }
        let mut argv = plan.argv.clone();
        let vnc_port = refresh_vnc_port(&mut argv, plan.vnc_auto).await?;
        let mut helpers = Vec::new();
        if let Some(helper_argv) = &plan.helper_argv {
            let helper_id = Id::new("run", format!("{}-helper", run_id.as_str()))?;
            helpers.push(ManagedProcess::spawn(helper_id, helper_argv, None, None)?);
        }
        for spec in plan.helpers.iter().filter(|spec| !spec.after_qemu) {
            match super::helpers::spawn(&workspace_root, &instance_id, &run_id, spec).await {
                Ok(process) => helpers.push(process),
                Err(error) => {
                    super::helpers::stop_all(&mut helpers);
                    return Err(error);
                }
            }
        }
        let post_helpers = plan.helpers.iter().any(|spec| spec.after_qemu);
        if post_helpers && !argv.iter().any(|value| value == "-S") {
            argv.push("-S".into());
        }
        let on_operation = |operation: &machineemu_core::domain::Operation| {
            events::publish_operation(&state, operation, Some(run_id.as_str()));
        };
        let on_state = |instance: &machineemu_core::domain::Instance| {
            events::publish_state_fields(
                &state,
                instance,
                Some(run_id.as_str()),
                matches!(instance.state.as_str(), "running" | "paused").then_some("running"),
                if instance.state == "error" {
                    "start_failed"
                } else {
                    "start"
                },
            );
        };
        let running = workspace
            .start_instance_async(machineemu_core::runtime::StartRequest {
                operation_id: operation_id.clone(),
                run_id: run_id.clone(),
                instance_id: instance_id.clone(),
                idempotency_key: &idempotency_key,
                input_json: &input_json,
                argv: &argv,
                qmp_socket: &qmp,
                stdout: stdout.as_deref(),
                stderr: stderr.as_deref(),
                qmp_timeout: std::time::Duration::from_secs(10),
                on_operation: Some(&on_operation),
                on_state: Some(&on_state),
                complete_operation: !post_helpers,
            })
            .await;
        let mut running = match running {
            Ok(running) => running,
            Err(error) => {
                super::helpers::stop_all(&mut helpers);
                return Err(error);
            }
        };
        for spec in plan.helpers.iter().filter(|spec| spec.after_qemu) {
            match super::helpers::spawn(&workspace_root, &instance_id, &run_id, spec).await {
                Ok(mut process) => {
                    if spec.name == "bluetooth"
                        && let Err(error) = super::helpers::wait_bluetooth_attached(
                            &state,
                            &instance_id,
                            &mut process,
                        )
                        .await
                    {
                        let _ = process.terminate_gracefully();
                        rollback_failed_start(
                            &state,
                            &mut workspace,
                            &instance_id,
                            &run_id,
                            &operation_id,
                            &mut running,
                            &mut helpers,
                            &error,
                        )
                        .await;
                        return Err(error);
                    }
                    helpers.push(process);
                }
                Err(error) => {
                    rollback_failed_start(
                        &state,
                        &mut workspace,
                        &instance_id,
                        &run_id,
                        &operation_id,
                        &mut running,
                        &mut helpers,
                        &error,
                    )
                    .await;
                    return Err(error);
                }
            }
        }
        if post_helpers {
            match workspace
                .resume_instance_async(&instance_id, &mut running)
                .await
            {
                Ok(instance) => events::publish_state_fields(
                    &state,
                    &instance,
                    Some(run_id.as_str()),
                    Some("running"),
                    "resume",
                ),
                Err(error) => {
                    rollback_failed_start(
                        &state,
                        &mut workspace,
                        &instance_id,
                        &run_id,
                        &operation_id,
                        &mut running,
                        &mut helpers,
                        &error,
                    )
                    .await;
                    return Err(error);
                }
            }
        }
        if input.launch_plan.is_some() {
            workspace.save_instance_launch(
                &instance_id,
                &serde_json::to_string(&plan)?,
                workspace
                    .instance_launch(&instance_id)?
                    .is_some_and(|(_, auto_remove)| auto_remove),
            )?;
        }
        let result = workspace.instance(&instance_id)?;
        if post_helpers {
            let completed = workspace.complete_operation(
                &operation_id,
                &serde_json::json!({"state": result.state}).to_string(),
            )?;
            events::publish_operation(&state, &completed, Some(run_id.as_str()));
        }
        drop(workspace);
        state
            .running
            .lock()
            .map_err(|_| RuntimeError::Process("running map lock poisoned".into()))?
            .insert(
                instance_id.as_str().into(),
                Arc::new(tokio::sync::Mutex::new(running)),
            );
        if !helpers.is_empty() {
            state
                .helpers
                .lock()
                .map_err(|_| RuntimeError::Process("helper map lock poisoned".into()))?
                .insert(instance_id.as_str().into(), helpers);
        }
        Ok((result, run_id, operation_id, vnc_port))
    };
    let result = tokio::spawn(future)
        .await
        .map_err(|error| RuntimeError::Process(format!("start task failed: {error}")))
        .and_then(|result| result);
    match result {
        Ok((instance, run_id, operation_id, vnc_port)) => {
            if let Ok(workspace) = watch_state.workspace.lock()
                && let Ok(Some(run)) = workspace.active_run(&instance.instance_id)
            {
                run_events::spawn(&watch_state, run);
            }
            let mut value = serde_json::to_value(&instance).unwrap_or_default();
            value["run_id"] = serde_json::Value::String(run_id.as_str().into());
            value["operation_id"] = serde_json::Value::String(operation_id.as_str().into());
            if let Some(port) = vnc_port {
                value["vnc_port"] = serde_json::Value::from(port);
            }
            axum::Json(value).into_response()
        }
        Err(error) => {
            if !lock_held {
                let _ = blocking(move || -> Result<(), RuntimeError> {
                    let instance_id = Id::new("instance", cleanup_id)?;
                    let lock = instance_lock(&cleanup_state, instance_id.as_str())?;
                    let _guard = lock.blocking_lock();
                    let workspace = cleanup_state
                        .workspace
                        .lock()
                        .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
                    if workspace.active_run(&instance_id)?.is_none()
                        && matches!(
                            workspace.instance(&instance_id)?.state.as_str(),
                            "created" | "stopped" | "error"
                        )
                    {
                        instances::remove_if_disposable(
                            &cleanup_state,
                            &workspace,
                            &instance_id,
                            None,
                            "start_failed",
                        )?;
                    }
                    Ok(())
                })
                .await;
            }
            (
                StatusCode::CONFLICT,
                axum::Json(ErrorBody {
                    error: error.to_string(),
                }),
            )
                .into_response()
        }
    }
}

pub(super) async fn stop_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    lifecycle_action(state, headers, id, "stop").await
}

pub(super) async fn restart_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let lock = match instance_lock(&state, &id) {
        Ok(lock) => lock,
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                axum::Json(ErrorBody {
                    error: error.to_string(),
                }),
            )
                .into_response();
        }
    };
    let _restart_guard = lock.lock_owned().await;
    let check_state = state.clone();
    let check_id = id.clone();
    let prepared = blocking(move || {
        let instance_id = Id::new("instance", check_id)?;
        let workspace = check_state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let instance = workspace.instance(&instance_id)?;
        let Some((_, auto_remove)) = workspace.instance_launch(&instance_id)? else {
            return Err(RuntimeError::Process(
                "instance has no saved launch plan".into(),
            ));
        };
        if auto_remove {
            return Err(RuntimeError::Process(
                "disposable instances cannot restart".into(),
            ));
        }
        Ok(instance.state)
    })
    .await;
    let current = match prepared {
        Ok(current) => current,
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                axum::Json(ErrorBody {
                    error: error.to_string(),
                }),
            )
                .into_response();
        }
    };
    if matches!(current.as_str(), "running" | "paused") {
        let response =
            lifecycle_action_with_lock(state.clone(), headers.clone(), id.clone(), "stop", true)
                .await;
        if !response.status().is_success() {
            return response;
        }
    } else if !matches!(current.as_str(), "created" | "stopped" | "error") {
        return (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: format!("cannot restart instance in state {current}"),
            }),
        )
            .into_response();
    }
    start_instance_with_lock(
        state,
        headers,
        id,
        StartInstance {
            operation_id: None,
            run_id: None,
            idempotency_key: None,
            launch_plan: None,
        },
        true,
    )
    .await
}

pub(super) async fn pause_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    lifecycle_action(state, headers, id, "pause").await
}

pub(super) async fn resume_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    lifecycle_action(state, headers, id, "resume").await
}

pub(super) async fn reset_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    lifecycle_action(state, headers, id, "reset").await
}

pub(super) async fn lifecycle_action(
    state: AppState,
    headers: HeaderMap,
    id: String,
    action: &str,
) -> axum::response::Response {
    lifecycle_action_with_lock(state, headers, id, action, false).await
}

async fn lifecycle_action_with_lock(
    state: AppState,
    headers: HeaderMap,
    id: String,
    action: &str,
    lock_held: bool,
) -> axum::response::Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let action = action.to_owned();
    let future = async move {
        let instance_id = Id::new("instance", id.clone())?;
        let instance_lock = instance_lock(&state, &id)?;
        let _instance_guard = if lock_held {
            None
        } else {
            Some(instance_lock.lock_owned().await)
        };
        let mut workspace = {
            let owner = state
                .workspace
                .lock()
                .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
            owner.attach()?
        };
        let running = state
            .running
            .lock()
            .map_err(|_| RuntimeError::Process("running map lock poisoned".into()))?
            .get(&id)
            .cloned();
        let running = if running.is_some() {
            running
        } else if let Some(recovered) = workspace.recover_instance_run_async(&instance_id).await? {
            let recovered = Arc::new(tokio::sync::Mutex::new(recovered));
            state
                .running
                .lock()
                .map_err(|_| RuntimeError::Process("running map lock poisoned".into()))?
                .insert(id.clone(), recovered.clone());
            Some(recovered)
        } else {
            None
        };
        let Some(running) = running else {
            let instance = workspace.instance(&instance_id)?;
            if action == "stop" && matches!(instance.state.as_str(), "stopped" | "error") {
                return Ok(instance);
            }
            return Err(RuntimeError::Process("instance has no live run".into()));
        };
        let mut running = running.lock().await;
        let instance = match action.as_str() {
            "stop" => {
                workspace
                    .stop_instance_async(&instance_id, &mut running)
                    .await
            }
            "pause" => {
                workspace
                    .pause_instance_async(&instance_id, &mut running)
                    .await
            }
            "resume" => {
                workspace
                    .resume_instance_async(&instance_id, &mut running)
                    .await
            }
            "reset" => workspace
                .reset_instance_async(&instance_id, &mut running)
                .await
                .map(|_| workspace.instance(&instance_id))
                .and_then(|result| result),
            _ => Err(RuntimeError::Process("unknown lifecycle action".into())),
        };
        if action == "stop" && instance.is_ok() {
            streams::revoke_display(&state, &id, running.run_id.as_str());
            state
                .running
                .lock()
                .map_err(|_| RuntimeError::Process("running map lock poisoned".into()))?
                .remove(&id);
        } else if instance.is_err() && running.is_recovered() {
            // Reconnect on the next request if this recovered QMP stream failed.
            state
                .running
                .lock()
                .map_err(|_| RuntimeError::Process("running map lock poisoned".into()))?
                .remove(&id);
        }
        if action == "stop"
            && instance.is_ok()
            && let Some(mut helpers) = state
                .helpers
                .lock()
                .map_err(|_| RuntimeError::Process("helper map lock poisoned".into()))?
                .remove(&id)
        {
            super::helpers::stop_all_checked(&mut helpers)?;
        }
        if action == "stop" && instance.is_ok() {
            state
                .display_streams
                .lock()
                .map_err(|_| RuntimeError::Process("display stream map lock poisoned".into()))?
                .remove(&id);
            state
                .audio_sessions
                .lock()
                .map_err(|_| RuntimeError::Process("audio session map lock poisoned".into()))?
                .retain(|_, session| session.instance_id != id);
        }
        if let Ok(ref instance) = instance {
            let run = workspace.run(&running.run_id).ok();
            events::publish_state(&state, instance, run.as_ref(), &action);
        }
        if action == "stop" && instance.is_ok() {
            instances::remove_if_disposable(
                &state,
                &workspace,
                &instance_id,
                Some(&running.run_id),
                "operator_stop",
            )?;
        }
        instance
    };
    let result = tokio::spawn(future)
        .await
        .map_err(|error| RuntimeError::Process(format!("lifecycle task failed: {error}")))
        .and_then(|result| result);
    match result {
        Ok(instance) => axum::Json(instance).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod vnc_start_tests {
    use super::*;

    #[tokio::test]
    async fn start_rechecks_explicit_port_and_reselects_auto_port() {
        let (busy_port, _listener) = (5900..=5999)
            .find_map(|port| {
                std::net::TcpListener::bind(("127.0.0.1", port))
                    .ok()
                    .map(|listener| (port, listener))
            })
            .unwrap();
        let mut explicit = vec![
            "-display".into(),
            format!("vnc=127.0.0.1:{},password-secret=secret", busy_port - 5900),
        ];
        assert!(refresh_vnc_port(&mut explicit, false).await.is_err());
        let mut automatic = explicit.clone();
        let selected = refresh_vnc_port(&mut automatic, true)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(selected, busy_port);
        assert_eq!(
            automatic[1],
            format!("vnc=127.0.0.1:{},password-secret=secret", selected - 5900)
        );
    }
}
