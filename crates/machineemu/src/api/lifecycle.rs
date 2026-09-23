use super::launch::{plan_paths, prepare_paths};
use super::*;
pub(super) async fn start_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(input): axum::Json<StartInstance>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<_, RuntimeError> {
        let input_json = serde_json::to_string(&input)
            .map_err(|error| RuntimeError::Process(error.to_string()))?;
        let instance_id = Id::new("instance", id)?;
        let instance_lock = instance_lock(&state, instance_id.as_str())?;
        let _instance_guard = instance_lock
            .lock()
            .map_err(|_| RuntimeError::Process("instance lock poisoned".into()))?;
        let operation_id = Id::new("operation", input.operation_id)?;
        let run_id = Id::new("run", input.run_id)?;
        let workspace = {
            let owner = state
                .workspace
                .lock()
                .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
            owner.attach()?
        };
        let instance = workspace.instance(&instance_id)?;
        let inline_plan = input.launch_plan.as_ref();
        let stored_plan = state.launch_plans.get(instance.profile_id.as_str());
        let plan = inline_plan.or(stored_plan).ok_or_else(|| {
            RuntimeError::Process(format!(
                "no launch plan for profile {}",
                instance.profile_id.as_str()
            ))
        })?;
        let (qmp, stdout, stderr) = plan_paths(workspace.root(), plan)?;
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
        let mut helpers = Vec::new();
        if let Some(helper_argv) = &plan.helper_argv {
            let helper_id = Id::new("run", format!("{}-helper", run_id.as_str()))?;
            helpers.push(ManagedProcess::spawn(helper_id, helper_argv, None, None)?);
        }
        for spec in plan.helpers.iter().filter(|spec| !spec.after_qemu) {
            match super::helpers::spawn(workspace.root(), &instance_id, &run_id, spec) {
                Ok(process) => helpers.push(process),
                Err(error) => {
                    super::helpers::stop_all(&mut helpers);
                    return Err(error);
                }
            }
        }
        let mut argv = plan.argv.clone();
        let post_helpers = plan.helpers.iter().any(|spec| spec.after_qemu);
        if post_helpers && !argv.iter().any(|value| value == "-S") {
            argv.push("-S".into());
        }
        let running = workspace.start_instance(machineemu_core::runtime::StartRequest {
            operation_id,
            run_id: run_id.clone(),
            instance_id: instance_id.clone(),
            idempotency_key: &input.idempotency_key,
            input_json: &input_json,
            argv: &argv,
            qmp_socket: &qmp,
            stdout: stdout.as_deref(),
            stderr: stderr.as_deref(),
            qmp_timeout: std::time::Duration::from_secs(10),
        });
        let mut running = match running {
            Ok(running) => running,
            Err(error) => {
                super::helpers::stop_all(&mut helpers);
                return Err(error);
            }
        };
        for spec in plan.helpers.iter().filter(|spec| spec.after_qemu) {
            match super::helpers::spawn(workspace.root(), &instance_id, &run_id, spec) {
                Ok(mut process) => {
                    if spec.name == "bluetooth"
                        && let Err(error) = super::helpers::wait_bluetooth_attached(
                            &state,
                            &instance_id,
                            &mut process,
                        )
                    {
                        let _ = process.terminate_gracefully();
                        let _ = workspace.stop_instance(&instance_id, &mut running);
                        super::helpers::stop_all(&mut helpers);
                        return Err(error);
                    }
                    helpers.push(process);
                }
                Err(error) => {
                    let _ = workspace.stop_instance(&instance_id, &mut running);
                    super::helpers::stop_all(&mut helpers);
                    return Err(error);
                }
            }
        }
        if post_helpers && let Err(error) = running.resume() {
            let _ = workspace.stop_instance(&instance_id, &mut running);
            super::helpers::stop_all(&mut helpers);
            return Err(error);
        }
        let result = workspace.instance(&instance_id)?;
        drop(workspace);
        state
            .running
            .lock()
            .map_err(|_| RuntimeError::Process("running map lock poisoned".into()))?
            .insert(instance_id.as_str().into(), Arc::new(Mutex::new(running)));
        if !helpers.is_empty() {
            state
                .helpers
                .lock()
                .map_err(|_| RuntimeError::Process("helper map lock poisoned".into()))?
                .insert(instance_id.as_str().into(), helpers);
        }
        Ok(result)
    })
    .await;
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

pub(super) async fn stop_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    lifecycle_action(state, headers, id, "stop").await
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
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let action = action.to_owned();
    let result = blocking(move || -> Result<_, RuntimeError> {
        let instance_id = Id::new("instance", id.clone())?;
        let instance_lock = instance_lock(&state, &id)?;
        let _instance_guard = instance_lock
            .lock()
            .map_err(|_| RuntimeError::Process("instance lock poisoned".into()))?;
        let workspace = {
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
        } else if let Some(recovered) = workspace.recover_instance_run(&instance_id)? {
            let recovered = Arc::new(Mutex::new(recovered));
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
        let mut running = running
            .lock()
            .map_err(|_| RuntimeError::Process("run lock poisoned".into()))?;
        let instance = match action.as_str() {
            "stop" => workspace.stop_instance(&instance_id, &mut running),
            "pause" => workspace.pause_instance(&instance_id, &mut running),
            "resume" => workspace.resume_instance(&instance_id, &mut running),
            "reset" => workspace
                .reset_instance(&instance_id, &mut running)
                .map(|_| workspace.instance(&instance_id))
                .and_then(|result| result),
            _ => Err(RuntimeError::Process("unknown lifecycle action".into())),
        };
        if action == "stop" && instance.is_ok() {
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
            super::helpers::stop_all(&mut helpers);
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
        instance
    })
    .await;
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
