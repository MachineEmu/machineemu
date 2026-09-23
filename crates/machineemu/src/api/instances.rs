use super::*;
use futures_util::{StreamExt, stream};

pub(super) fn remove_if_disposable(
    state: &AppState,
    workspace: &Workspace,
    instance_id: &Id,
    run_id: Option<&Id>,
    reason: &str,
) -> Result<bool, RuntimeError> {
    if !workspace
        .instance_launch(instance_id)?
        .is_some_and(|(_, auto_remove)| auto_remove)
    {
        return Ok(false);
    }
    workspace.remove_instance(instance_id)?;
    workspace.record_instance_tombstone(instance_id, run_id, reason)?;
    state
        .stream_tickets
        .lock()
        .map_err(|_| RuntimeError::Process("stream ticket lock poisoned".into()))?
        .retain(|_, ticket| ticket.instance_id != instance_id.as_str());
    state
        .events
        .lock()
        .map_err(|_| RuntimeError::Process("event hub lock poisoned".into()))?
        .close_instance(instance_id.as_str());
    Ok(true)
}
pub(super) async fn create_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(input): axum::Json<CreateInstance>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<_, RuntimeError> {
        let instance_id = Id::new("instance", input.instance_id)?;
        let lock = instance_lock(&state, instance_id.as_str())?;
        let _guard = lock.blocking_lock();
        let image_id = Id::new("image", input.image_id)?;
        let profile_id = Id::new("profile", input.profile_id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        if input.auto_remove && input.launch_plan.is_none() {
            return Err(RuntimeError::Process(
                "auto_remove requires a launch_plan".into(),
            ));
        }
        if input.profile.is_some() && input.launch_plan.is_none() {
            return Err(RuntimeError::Process(
                "profile requires a launch_plan".into(),
            ));
        }
        if let Some(profile) = &input.profile
            && profile.get("id").and_then(serde_json::Value::as_str) != Some(profile_id.as_str())
        {
            return Err(RuntimeError::Process(
                "profile.id must match profile_id".into(),
            ));
        }
        if let Some(plan) = &input.launch_plan {
            super::launch::plan_paths(workspace.root(), plan)?;
            if let Some(preparation) = &plan.preparation {
                super::launch::prepare_paths(workspace.root(), preparation)?;
            }
        }
        if let Some(plan) = &input.launch_plan {
            let staging_root = workspace
                .root()
                .join("staging/instances")
                .join(instance_id.as_str());
            std::fs::create_dir_all(&staging_root).map_err(|source| RuntimeError::Io {
                path: staging_root.clone(),
                source,
            })?;
            for entry in std::fs::read_dir(&staging_root).map_err(|source| RuntimeError::Io {
                path: staging_root.clone(),
                source,
            })? {
                let entry = entry.map_err(|source| RuntimeError::Io {
                    path: staging_root.clone(),
                    source,
                })?;
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    std::fs::remove_dir_all(entry.path()).map_err(|source| RuntimeError::Io {
                        path: entry.path(),
                        source,
                    })?;
                }
            }
            let suffix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let staged = staging_root.join(format!("{}-{suffix}", std::process::id()));
            std::fs::create_dir(&staged).map_err(|source| RuntimeError::Io {
                path: staged.clone(),
                source,
            })?;
            let prepared = (|| -> Result<_, RuntimeError> {
                if let Some(preparation) = &plan.preparation {
                    let (backing, nvram, tpm) =
                        super::launch::prepare_paths(workspace.root(), preparation)?;
                    workspace.prepare_instance_files_at(
                        &staged,
                        &backing,
                        &preparation.backing_format,
                        preparation.disk_size.as_deref(),
                        nvram.as_deref(),
                        tpm.as_deref(),
                    )?;
                }
                if let Some(profile) = &input.profile {
                    let path = staged.join("profile.json");
                    std::fs::write(&path, serde_json::to_vec_pretty(profile)?)
                        .map_err(|source| RuntimeError::Io { path, source })?;
                }
                workspace.publish_prepared_instance(
                    instance_id.clone(),
                    image_id,
                    profile_id,
                    &staged,
                    &serde_json::to_string(plan)?,
                    input.auto_remove,
                )
            })();
            if prepared.is_err() {
                let _ = std::fs::remove_dir_all(&staged);
            }
            return prepared;
        }
        workspace.create_instance(instance_id, image_id, profile_id)
    })
    .await;
    match result {
        Ok(instance) => (StatusCode::CREATED, axum::Json(instance)).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn list_instances(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || {
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let instances = workspace.instances()?;
        let entries = instances
            .into_iter()
            .map(|instance| {
                let launch = workspace.instance_launch(&instance.instance_id)?;
                Ok((
                    instance,
                    launch.is_some(),
                    launch.is_some_and(|(_, auto_remove)| auto_remove),
                ))
            })
            .collect::<Result<Vec<_>, RuntimeError>>()?;
        Ok((entries, workspace.root().to_owned()))
    })
    .await;
    match result {
        Ok((instances, root)) => {
            let statuses: Vec<InstanceStatus> = stream::iter(instances)
                .map(|(instance, configured, auto_remove)| {
                    let root = root.clone();
                    async move {
                        let socket = root
                            .join("instances")
                            .join(instance.instance_id.as_str())
                            .join("qga.sock");
                        let ip = guest_agent::guest_ipv4(&socket).await;
                        InstanceStatus {
                            instance,
                            ip,
                            configured,
                            auto_remove,
                        }
                    }
                })
                .buffered(8)
                .collect()
                .await;
            axum::Json(statuses).into_response()
        }
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn get_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<_, RuntimeError> {
        let instance_id = Id::new("instance", id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let instance = workspace.instance(&instance_id)?;
        let launch = workspace.instance_launch(&instance_id)?;
        let mut value = serde_json::to_value(instance)?;
        value["configured"] = serde_json::Value::Bool(launch.is_some());
        value["auto_remove"] =
            serde_json::Value::Bool(launch.is_some_and(|(_, auto_remove)| auto_remove));
        Ok(value)
    })
    .await;
    match result {
        Ok(instance) => axum::Json(instance).into_response(),
        Err(error) => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn get_instance_tombstone(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || {
        let instance_id = Id::new("instance", id)?;
        let workspace = state.workspace.lock().map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        Ok(workspace.instance_tombstone(&instance_id)?.map(|(last_run_id, reason, removed_at)| serde_json::json!({"instance_id":instance_id.as_str(),"last_run_id":last_run_id,"reason":reason,"removed_at":removed_at})))
    }).await;
    match result {
        Ok(Some(value)) => axum::Json(value).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn remove_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<_, RuntimeError> {
        let instance_id = Id::new("instance", id)?;
        let lock = instance_lock(&state, instance_id.as_str())?;
        let _guard = lock.blocking_lock();
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.remove_instance(&instance_id)?;
        state
            .events
            .lock()
            .map_err(|_| RuntimeError::Process("event hub lock poisoned".into()))?
            .close_instance(instance_id.as_str());
        Ok(())
    })
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(RuntimeError::NotFound { .. }) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}
