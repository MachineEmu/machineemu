use super::*;
pub(super) async fn get_operation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<_, RuntimeError> {
        let operation_id = Id::new("operation", id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.operation(&operation_id)
    })
    .await;
    match result {
        Ok(operation) => axum::Json(operation).into_response(),
        Err(error) => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn reconcile(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || {
        let instances = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?
            .instances()?;
        let locks = instances
            .iter()
            .map(|instance| instance_lock(&state, instance.instance_id.as_str()))
            .collect::<Result<Vec<_>, _>>()?;
        let _guards = locks
            .iter()
            .map(|lock| lock.blocking_lock())
            .collect::<Vec<_>>();
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let runs = workspace.reconcile_active_runs()?;
        for operation in workspace.reconcile_operations()? {
            events::publish_operation(&state, &operation, None);
        }
        for before in instances {
            let after = workspace.instance(&before.instance_id)?;
            if after.revision != before.revision {
                let run = runs
                    .iter()
                    .find(|run| run.instance_id == before.instance_id);
                events::publish_state(&state, &after, run, "reconciliation");
            }
        }
        Ok::<_, RuntimeError>(runs)
    })
    .await;
    match result {
        Ok(runs) => axum::Json(runs).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}
