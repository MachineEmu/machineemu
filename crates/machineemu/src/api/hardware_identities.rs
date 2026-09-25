use super::*;
use axum::{extract::Path, response::IntoResponse};
use machineemu_core::domain::HardwareIdentityDocument;
use std::path::PathBuf;

fn path(root: &std::path::Path, id: &str) -> Result<PathBuf, RuntimeError> {
    let id = Id::new("hardware-identity", id)?;
    Ok(root
        .join("hardware-identities")
        .join(format!("{}.yaml", id.as_str())))
}

fn write_document(
    root: &std::path::Path,
    document: &HardwareIdentityDocument,
) -> Result<(), RuntimeError> {
    if document.api_version != "machineemu.io/v1" || document.kind != "HardwareIdentity" {
        return Err(RuntimeError::Process(
            "invalid hardware identity document kind".into(),
        ));
    }
    let path = path(root, &document.metadata.name)?;
    let parent = path.parent().expect("identity path has parent");
    std::fs::create_dir_all(parent).map_err(|source| RuntimeError::Io {
        path: parent.to_owned(),
        source,
    })?;
    let temporary = parent.join(format!(".{}.tmp", std::process::id()));
    let bytes =
        serde_yaml::to_string(document).map_err(|e| RuntimeError::Process(e.to_string()))?;
    std::fs::write(&temporary, bytes).map_err(|source| RuntimeError::Io {
        path: temporary.clone(),
        source,
    })?;
    std::fs::rename(&temporary, &path).map_err(|source| RuntimeError::Io { path, source })
}

pub(super) async fn list(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    match blocking(move || {
        let root = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?
            .root()
            .to_owned();
        let directory = root.join("hardware-identities");
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&directory) {
            for entry in entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("yaml"))
            {
                let value = machineemu_core::engine::load_document(&entry.path())
                    .map_err(|e| RuntimeError::Process(e.to_string()))?;
                out.push(value);
            }
        }
        Ok::<_, RuntimeError>(out)
    })
    .await
    {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    match blocking(move || {
        let root = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?
            .root()
            .to_owned();
        let path = path(&root, &id)?;
        machineemu_core::engine::load_document(&path)
            .map_err(|e| RuntimeError::Process(e.to_string()))
    })
    .await
    {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(document): axum::Json<HardwareIdentityDocument>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    match blocking(move || {
        if document.metadata.name != id {
            return Err(RuntimeError::Process(
                "path ID must match metadata.name".into(),
            ));
        }
        let root = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?
            .root()
            .to_owned();
        write_document(&root, &document)?;
        Ok::<_, RuntimeError>(document)
    })
    .await
    {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(document): axum::Json<HardwareIdentityDocument>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    match blocking(move || {
        let root = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?
            .root()
            .to_owned();
        let path = path(&root, &document.metadata.name)?;
        if path.exists() {
            return Err(RuntimeError::Process(
                "hardware identity already exists".into(),
            ));
        }
        write_document(&root, &document)?;
        Ok::<_, RuntimeError>(document)
    })
    .await
    {
        Ok(value) => (StatusCode::CREATED, axum::Json(value)).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}
