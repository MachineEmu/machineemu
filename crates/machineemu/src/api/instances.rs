use super::*;
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
        let _guard = lock
            .lock()
            .map_err(|_| RuntimeError::Process("instance lock poisoned".into()))?;
        let image_id = Id::new("image", input.image_id)?;
        let profile_id = Id::new("profile", input.profile_id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
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
        let (instances, root) = {
            let workspace = state
                .workspace
                .lock()
                .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
            (workspace.instances()?, workspace.root().to_owned())
        };
        Ok(instances
            .into_iter()
            .map(|instance| {
                let socket = root
                    .join("instances")
                    .join(instance.instance_id.as_str())
                    .join("qga.sock");
                let ip = if socket.exists() {
                    #[cfg(unix)]
                    {
                        machineemu_core::protocols::guest_agent::guest_ipv4(&socket)
                            .ok()
                            .flatten()
                    }
                    #[cfg(not(unix))]
                    {
                        None
                    }
                } else {
                    None
                };
                InstanceStatus { instance, ip }
            })
            .collect::<Vec<_>>())
    })
    .await;
    match result {
        Ok(instances) => axum::Json(instances).into_response(),
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
        workspace.instance(&instance_id)
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
        let _guard = lock
            .lock()
            .map_err(|_| RuntimeError::Process("instance lock poisoned".into()))?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.remove_instance(&instance_id)
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
