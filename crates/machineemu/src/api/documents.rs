use super::*;
use axum::{body::Bytes, http::header, response::Response};
use serde_json::Value;
use std::{fs, io::Write, path::Path as FsPath};

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct InstanceConfig {
    schema_version: u32,
    instance_id: String,
    image_id: String,
    profile_id: String,
    revision: i64,
    auto_remove: bool,
    profile: Option<Value>,
    launch_plan: LaunchSpec,
}

fn parse_document<T: serde::de::DeserializeOwned>(
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<T, RuntimeError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json");
    if content_type.starts_with("application/yaml")
        || content_type.starts_with("text/yaml")
        || content_type.starts_with("application/x-yaml")
    {
        serde_yaml::from_slice(body)
            .map_err(|error| RuntimeError::Process(format!("invalid YAML document: {error}")))
    } else if content_type.starts_with("application/json") {
        serde_json::from_slice(body)
            .map_err(|error| RuntimeError::Process(format!("invalid JSON document: {error}")))
    } else {
        Err(RuntimeError::Process(format!(
            "unsupported document content type: {content_type}"
        )))
    }
}

pub(super) fn render_document<T: Serialize>(headers: &HeaderMap, document: T) -> Response {
    if headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/yaml") || value.contains("text/yaml"))
    {
        match serde_yaml::to_string(&document) {
            Ok(body) => ([(header::CONTENT_TYPE, "application/yaml")], body).into_response(),
            Err(error) => bad_request(RuntimeError::Process(error.to_string())),
        }
    } else {
        axum::Json(document).into_response()
    }
}

fn bad_request(error: RuntimeError) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(ErrorBody {
            error: error.to_string(),
        }),
    )
        .into_response()
}

fn read_workspace_file(root: &FsPath, path: &FsPath) -> Result<Vec<u8>, RuntimeError> {
    let canonical_root = fs::canonicalize(root).map_err(|source| RuntimeError::Io {
        path: root.to_owned(),
        source,
    })?;
    let canonical_path = fs::canonicalize(path).map_err(|source| RuntimeError::Io {
        path: path.to_owned(),
        source,
    })?;
    if !canonical_path.starts_with(canonical_root) {
        return Err(RuntimeError::Process(
            "document path escapes the workspace".into(),
        ));
    }
    fs::read(path).map_err(|source| RuntimeError::Io {
        path: path.to_owned(),
        source,
    })
}

fn atomic_json(root: &FsPath, path: &FsPath, value: &impl Serialize) -> Result<(), RuntimeError> {
    let parent = path
        .parent()
        .ok_or_else(|| RuntimeError::Process("document has no parent directory".into()))?;
    fs::create_dir_all(parent).map_err(|source| RuntimeError::Io {
        path: parent.to_owned(),
        source,
    })?;
    let canonical_root = fs::canonicalize(root).map_err(|source| RuntimeError::Io {
        path: root.to_owned(),
        source,
    })?;
    let canonical_parent = fs::canonicalize(parent).map_err(|source| RuntimeError::Io {
        path: parent.to_owned(),
        source,
    })?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(RuntimeError::Process(
            "document path escapes the workspace".into(),
        ));
    }
    let temporary = parent.join(format!(
        ".document-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let result = (|| -> Result<(), RuntimeError> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| RuntimeError::Io {
                path: temporary.clone(),
                source,
            })?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n").map_err(|source| RuntimeError::Io {
            path: temporary.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| RuntimeError::Io {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, path).map_err(|source| RuntimeError::Io {
            path: path.to_owned(),
            source,
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(super) async fn get_instance_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<_, RuntimeError> {
        let instance_id = Id::new("instance", id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let document = workspace.instance_document(&instance_id)?;
        let revision = document.revision()?;
        Ok(InstanceConfig {
            schema_version: document.schema_version,
            instance_id: document.instance_id,
            image_id: document.image_id,
            profile_id: document.profile_id,
            revision,
            auto_remove: document.auto_remove,
            profile: document.profile,
            launch_plan: serde_json::from_value(document.launch_plan.ok_or_else(|| {
                RuntimeError::Process("instance has no saved launch plan".into())
            })?)?,
        })
    })
    .await;
    match result {
        Ok(document) => render_document(&headers, document),
        Err(error) => bad_request(error),
    }
}

pub(super) async fn put_instance_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let mut input: InstanceConfig = match parse_document(&headers, &body) {
        Ok(input) => input,
        Err(error) => return bad_request(error),
    };
    let result = blocking(move || -> Result<_, RuntimeError> {
        if input.schema_version != 1 {
            return Err(RuntimeError::Process(
                "instance document schema_version must be 1".into(),
            ));
        }
        let instance_id = Id::new("instance", id)?;
        let lock = instance_lock(&state, instance_id.as_str())?;
        let _guard = lock.blocking_lock();
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let instance = workspace.instance(&instance_id)?;
        if !matches!(instance.state.as_str(), "created" | "stopped" | "error")
            || workspace.active_run(&instance_id)?.is_some()
        {
            return Err(RuntimeError::Process(
                "stop the instance before editing its configuration".into(),
            ));
        }
        let current_revision = workspace.instance_document(&instance_id)?.revision()?;
        if input.revision != current_revision {
            return Err(RuntimeError::Process(format!(
                "instance configuration changed since revision {}; current revision is {}",
                input.revision, current_revision
            )));
        }
        let (old_plan, auto_remove) = workspace
            .instance_launch(&instance_id)?
            .ok_or_else(|| RuntimeError::Process("instance has no saved launch plan".into()))?;
        let old_plan: LaunchSpec = serde_json::from_str(&old_plan)?;
        if input.instance_id != instance_id.as_str()
            || input.image_id != instance.image_id.as_str()
            || input.profile_id != instance.profile_id.as_str()
            || input.auto_remove != auto_remove
        {
            return Err(RuntimeError::Process(
                "instance, image, profile IDs and auto_remove cannot be changed here".into(),
            ));
        }
        if input.launch_plan.preparation != old_plan.preparation {
            return Err(RuntimeError::Process(
                "disk, NVRAM and TPM preparation cannot change on an existing instance".into(),
            ));
        }
        if let Some(profile) = &input.profile
            && profile.get("id").and_then(Value::as_str) != Some(input.profile_id.as_str())
        {
            return Err(RuntimeError::Process(
                "profile.id must match profile_id".into(),
            ));
        }
        super::launch::validate_plan_paths(workspace.root(), &input.launch_plan)?;
        let updated = workspace.replace_instance_configuration(
            &instance_id,
            input.revision,
            &serde_json::to_string(&input.launch_plan)?,
            auto_remove,
            input.profile.as_ref(),
        )?;
        input.revision = updated;
        Ok(input)
    })
    .await;
    match result {
        Ok(document) => render_document(&headers, document),
        Err(error) => bad_request(error),
    }
}

fn profile_path(root: &FsPath, id: &Id) -> (std::path::PathBuf, std::path::PathBuf) {
    let workspace = root.join("profiles").join(format!("{}.json", id.as_str()));
    let catalog_root = FsPath::new("catalog/profiles");
    let catalog_root = if catalog_root.is_dir() {
        catalog_root.to_owned()
    } else {
        FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../../catalog/profiles")
    };
    let catalog = catalog_root.join(format!("{}.json", id.as_str()));
    (workspace, catalog)
}

pub(super) async fn get_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<Value, RuntimeError> {
        let profile_id = Id::new("profile", id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let (workspace_path, catalog_path) = profile_path(workspace.root(), &profile_id);
        let (path, from_workspace) = if workspace_path.is_file() {
            (workspace_path, true)
        } else {
            (catalog_path, false)
        };
        if !path.is_file() {
            return Err(RuntimeError::NotFound {
                kind: "profile",
                id: profile_id.as_str().to_owned(),
            });
        }
        let bytes = if from_workspace {
            read_workspace_file(workspace.root(), &path)?
        } else {
            fs::read(&path).map_err(|source| RuntimeError::Io {
                path: path.clone(),
                source,
            })?
        };
        serde_json::from_slice(&bytes).map_err(RuntimeError::from)
    })
    .await;
    match result {
        Ok(document) => render_document(&headers, document),
        Err(error @ RuntimeError::NotFound { .. }) => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
        Err(error) => bad_request(error),
    }
}

pub(super) async fn put_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let document: Value = match parse_document(&headers, &body) {
        Ok(document) => document,
        Err(error) => return bad_request(error),
    };
    let result = blocking(move || -> Result<Value, RuntimeError> {
        let profile_id = Id::new("profile", id)?;
        let declared_id = document
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::Process("profile.id is required".into()))?;
        Id::new("profile", declared_id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let (path, _) = profile_path(workspace.root(), &profile_id);
        atomic_json(workspace.root(), &path, &document)?;
        Ok(document)
    })
    .await;
    match result {
        Ok(document) => render_document(&headers, document),
        Err(error) => bad_request(error),
    }
}

pub(super) async fn put_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let document: Value = match parse_document(&headers, &body) {
        Ok(document) => document,
        Err(error) => return bad_request(error),
    };
    let Some(fields) = document.as_object() else {
        return bad_request(RuntimeError::Process(
            "image manifest must be an object".into(),
        ));
    };
    for field in fields.keys() {
        if ![
            "image_id",
            "engine_track",
            "supported_engine_tracks",
            "target",
            "disk_sha256",
            "firmware_sha256",
            "tpm_state_sha256",
        ]
        .contains(&field.as_str())
        {
            return bad_request(RuntimeError::Process(format!(
                "unknown image manifest field: {field}"
            )));
        }
    }
    let image: ImageManifest = match serde_json::from_value(document) {
        Ok(image) => image,
        Err(error) => return bad_request(RuntimeError::Process(error.to_string())),
    };
    let result = blocking(move || -> Result<_, RuntimeError> {
        let image_id = Id::new("image", id)?;
        if image.image_id != image_id {
            return Err(RuntimeError::Process("image_id must match URL ID".into()));
        }
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.replace_image_manifest(&image)?;
        Ok(image)
    })
    .await;
    match result {
        Ok(image) => render_document(&headers, image),
        Err(error) => bad_request(error),
    }
}
