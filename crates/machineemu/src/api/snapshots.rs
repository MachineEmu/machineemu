use super::*;
use sha2::{Digest, Sha256};
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
        let _guard = lock.blocking_lock();
        let snapshot_id = Id::new("snapshot", input.snapshot_id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.instance(&instance_id)?;
        if workspace
            .instance_launch(&instance_id)?
            .is_some_and(|(_, auto_remove)| auto_remove)
        {
            return Err(RuntimeError::Process(
                "snapshots are unavailable for auto-remove instances".into(),
            ));
        }
        let digest =
            Sha256::digest(format!("{}:{}", instance_id.as_str(), snapshot_id.as_str()).as_bytes());
        let operation_name = format!(
            "snapshot-{}",
            digest[..24]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let operation_id = Id::new("operation", operation_name)?;
        let existing = workspace.operation(&operation_id).is_ok();
        let operation = workspace.begin_operation(
            operation_id,
            instance_id.clone(),
            "snapshot",
            &format!("snapshot:{}", snapshot_id.as_str()),
            &serde_json::json!({"snapshot_id": snapshot_id.as_str()}).to_string(),
        )?;
        if operation.status == "completed" {
            return workspace.snapshot(&snapshot_id);
        }
        if operation.status != "accepted" {
            return Err(RuntimeError::Process(format!(
                "snapshot operation is {}",
                operation.status
            )));
        }
        if !existing {
            events::publish_operation(&state, &operation, None);
        }
        match workspace.create_instance_snapshot(snapshot_id.clone(), instance_id) {
            Ok(snapshot) => {
                let completed = workspace.complete_operation(
                    &operation.operation_id,
                    &serde_json::json!({"snapshot_id": snapshot_id.as_str()}).to_string(),
                )?;
                events::publish_operation(&state, &completed, None);
                let instance = workspace.instance(&completed.instance_id)?;
                let run = workspace.active_run(&completed.instance_id)?;
                events::publish_state(&state, &instance, run.as_ref(), "snapshot_created");
                Ok(snapshot)
            }
            Err(error) => {
                if let Ok(failed) =
                    workspace.fail_operation(&operation.operation_id, &error.to_string())
                {
                    events::publish_operation(&state, &failed, None);
                }
                Err(error)
            }
        }
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
        let _guard = lock.blocking_lock();
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
