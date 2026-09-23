use super::*;
pub(super) async fn create_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(input): axum::Json<CreateSnapshot>,
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
        let snapshot_id = Id::new("snapshot", input.snapshot_id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.create_instance_snapshot(snapshot_id, instance_id)
    })
    .await;
    match result {
        Ok(snapshot) => (StatusCode::CREATED, axum::Json(snapshot)).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn get_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<_, RuntimeError> {
        let snapshot_id = Id::new("snapshot", id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.snapshot(&snapshot_id)
    })
    .await;
    match result {
        Ok(snapshot) => axum::Json(snapshot).into_response(),
        Err(error) => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn clone_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(input): axum::Json<CloneSnapshot>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<_, RuntimeError> {
        let snapshot_id = Id::new("snapshot", id)?;
        let instance_id = Id::new("instance", input.instance_id)?;
        let lock = instance_lock(&state, instance_id.as_str())?;
        let _guard = lock
            .lock()
            .map_err(|_| RuntimeError::Process("instance lock poisoned".into()))?;
        let profile_id = Id::new("profile", input.profile_id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let destination = workspace
            .root()
            .join("instances")
            .join(instance_id.as_str());
        workspace.clone_snapshot(&snapshot_id, instance_id, profile_id, &destination)
    })
    .await;
    match result {
        Ok(instance) => (StatusCode::CREATED, axum::Json(instance)).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}
