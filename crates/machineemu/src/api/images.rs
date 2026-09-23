use super::*;
pub(super) async fn register_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(input): axum::Json<RegisterImage>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<_, RuntimeError> {
        let manifest = ImageManifest {
            image_id: Id::new("image", input.image_id)?,
            engine_track: Id::new("engine track", input.engine_track)?,
            supported_engine_tracks: input
                .supported_engine_tracks
                .into_iter()
                .map(|track| Id::new("engine track", track))
                .collect::<Result<_, _>>()?,
            target: input.target,
            disk_sha256: input.disk_sha256,
            firmware_sha256: input.firmware_sha256,
            tpm_state_sha256: input.tpm_state_sha256,
        };
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let digest = workspace.register_image(&manifest)?;
        Ok((manifest, digest))
    })
    .await;
    match result {
        Ok((manifest, digest)) => (
            StatusCode::CREATED,
            axum::Json(serde_json::json!({
                "manifest": manifest,
                "manifest_sha256": digest
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn get_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<_, RuntimeError> {
        let image_id = Id::new("image", id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.image(&image_id)
    })
    .await;
    match result {
        Ok(image) => documents::render_document(&headers, image),
        Err(error) => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}
