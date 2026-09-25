use super::*;
use serde_json::json;

#[derive(Deserialize, utoipa::ToSchema)]
pub(super) struct ImportEngine {
    #[schema(value_type = String)]
    source: PathBuf,
}

pub(super) async fn start_engine_import(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(input): axum::Json<ImportEngine>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let id = images::image_import_id();
    let root = match state.workspace.lock() {
        Ok(workspace) => workspace.root().to_owned(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match state.image_imports.lock() {
        Ok(mut jobs) => {
            jobs.insert(
                id.clone(),
                images::ImageImportJob::engine(input.source.clone()),
            );
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    let worker_id = id.clone();
    tokio::spawn(async move {
        // Serialize registry publication while allowing unrelated VM and image work.
        static IMPORT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _guard = IMPORT_LOCK.lock().await;
        let worker_state = state.clone();
        let job_id = worker_id.clone();
        let result = blocking(move || {
            let mut last_update = std::time::Instant::now();
            let mut bytes_total = 0;
            crate::engine_import::import(&root, &input.source, |phase, done, total| {
                bytes_total = total;
                if last_update.elapsed() < std::time::Duration::from_millis(100) {
                    return;
                }
                last_update = std::time::Instant::now();
                let _ = images::set_import_status(
                    &worker_state,
                    &job_id,
                    "running",
                    phase,
                    None,
                    done,
                    total,
                );
                let _ = images::publish_import_event(
                    &worker_state,
                    &job_id,
                    "progress",
                    json!({"phase":phase,"bytes_done":done,"bytes_total":total}),
                );
            })
            .map(|manifest| (manifest, bytes_total))
            .map_err(|error| RuntimeError::Process(error.to_string()))
        })
        .await;
        match result {
            Ok((manifest, total)) => {
                let _ = images::set_import_status(
                    &state, &worker_id, "complete", "complete", None, total, total,
                );
                let _ = images::finish_import(&state, &worker_id, &manifest);
                let _ = images::publish_import_event(
                    &state,
                    &worker_id,
                    "complete",
                    json!({"phase":"complete","bytes_done":total,"bytes_total":total,"manifest":manifest}),
                );
            }
            Err(error) => {
                images::fail_import(&state, &worker_id, error.to_string());
                let _ = images::publish_import_event(
                    &state,
                    &worker_id,
                    "failed",
                    json!({"error":error.to_string()}),
                );
            }
        }
    });
    (
        StatusCode::ACCEPTED,
        axum::Json(json!({
            "import_id":id,
            "status_url":format!("/api/v2/engine-imports/{id}"),
            "events_url":format!("/api/v2/engine-imports/{id}/events")
        })),
    )
        .into_response()
}
