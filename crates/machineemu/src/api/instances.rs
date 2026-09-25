use super::*;
use futures_util::{StreamExt, stream};
use machineemu_core::domain::{
    CreateInstanceOverrides, HardwareIdentityDocument, ImageManifestDocument, PartialProfile,
};
use machineemu_core::resolution::{ResolutionContext, resolve_instance as resolve_domain_instance};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{fs, path::Path as FsPath};

fn workspace_engine_capabilities(
    root: &FsPath,
) -> Vec<machineemu_core::resolution::EngineCapability> {
    let engines = root.join("generated-engines");
    let Ok(entries) = fs::read_dir(engines) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let track = entry.file_name().to_string_lossy().into_owned();
            let manifest = entry.path().join("engine-build.json");
            let value: Value = serde_json::from_slice(&fs::read(manifest).ok()?).ok()?;
            let executable = value
                .get("executables")
                .and_then(Value::as_object)
                .and_then(|values| values.values().find_map(Value::as_str))
                .map(str::to_owned);
            Some(machineemu_core::resolution::EngineCapability {
                track,
                build_digest: value
                    .get("build_digest")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                executable,
                patch_revision: value
                    .get("patch_revision")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                machines: value
                    .get("machines")
                    .or_else(|| value.get("targets"))
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

fn profile_document(
    root: &FsPath,
    profile_id: &str,
    supplied: Option<&Value>,
) -> Result<PartialProfile, RuntimeError> {
    let value = match supplied {
        Some(Value::String(id)) => {
            let path = root.join("profiles").join(format!("{id}.yaml"));
            machineemu_core::engine::load_document(&path)
                .map_err(|error| RuntimeError::Process(error.to_string()))?
        }
        Some(value) if value.get("api_version").is_some() => value.clone(),
        Some(value) => value.clone(),
        None => {
            let path = root.join("profiles").join(format!("{profile_id}.yaml"));
            machineemu_core::engine::load_document(&path)
                .map_err(|error| RuntimeError::Process(error.to_string()))?
        }
    };
    if value.get("api_version").is_some() {
        return Ok(serde_json::from_value(value)?);
    }
    let object = value.as_object().ok_or_else(|| {
        RuntimeError::Process("profile must be an object or document reference".into())
    })?;
    let mut spec = object.clone();
    let name = spec
        .remove("id")
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| profile_id.to_owned());
    spec.remove("schema_version");
    Ok(PartialProfile {
        api_version: "machineemu.io/v1".into(),
        kind: "Profile".into(),
        metadata: machineemu_core::domain::configuration::DocumentMetadata {
            name,
            revision: 1,
            digest: None,
        },
        spec: Value::Object(spec),
    })
}

fn image_document(
    workspace: &Workspace,
    image_id: &str,
) -> Result<ImageManifestDocument, RuntimeError> {
    let id = Id::new("image", image_id)?;
    let image = workspace.image(&id)?;
    let components = image
        .components
        .iter()
        .map(|(name, component)| {
            let digest = component
                .sha256
                .strip_prefix("sha256:")
                .unwrap_or(&component.sha256);
            (
                name.clone(),
                serde_json::json!({
                    "path": component.path,
                    "sha256": component.sha256,
                    "artifact": {
                        "digest": format!("sha256:{digest}"),
                        "path": format!("objects/sha256/{digest}")
                    }
                }),
            )
        })
        .collect::<Map<_, _>>();
    Ok(ImageManifestDocument {
        api_version: "machineemu.io/v1".into(),
        kind: "Image".into(),
        metadata: machineemu_core::domain::configuration::DocumentMetadata {
            name: image.image_id.as_str().into(),
            revision: 1,
            digest: None,
        },
        spec: serde_json::json!({
            "engine_track": image.engine_track,
            "compatible_engines": image.supported_engine_tracks,
            "target": image.target,
            "components": components,
            "disk_sha256": image.disk_sha256,
            "firmware_sha256": image.firmware_sha256,
            "tpm_state_sha256": image.tpm_state_sha256,
        }),
    })
}

fn hardware_identity_document(
    root: &FsPath,
    profile: &PartialProfile,
) -> Result<Option<HardwareIdentityDocument>, RuntimeError> {
    let Some(reference) = profile
        .spec
        .get("hardware_identity")
        .and_then(|v| v.get("ref"))
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let id = Id::new("hardware-identity", reference)?;
    let path = root
        .join("hardware-identities")
        .join(format!("{}.yaml", id.as_str()));
    let value = machineemu_core::engine::load_document(&path)
        .map_err(|e| RuntimeError::Process(e.to_string()))?;
    Ok(Some(serde_json::from_value(value)?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResolveInstanceRequest {
    pub(super) profile: PartialProfile,
    #[serde(default)]
    pub(super) hardware_identity: Option<HardwareIdentityDocument>,
    pub(super) image: ImageManifestDocument,
    #[serde(default)]
    pub(super) overrides: CreateInstanceOverrides,
    #[serde(default)]
    pub(super) context: ResolutionContext,
}

pub(super) async fn resolve_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(mut input): axum::Json<ResolveInstanceRequest>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || {
        if input.context.engines.is_empty() {
            let workspace = state
                .workspace
                .lock()
                .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
            input.context.engines = workspace_engine_capabilities(workspace.root());
        }
        input.context.instance_id = input.profile.metadata.name.clone();
        resolve_domain_instance(
            input.profile,
            input.hardware_identity,
            input.image,
            input.overrides,
            input.context,
        )
    })
    .await;
    match result {
        Ok(document) => axum::Json(document).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

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
        let image_id = Id::new(
            "image",
            if input.image_id.is_empty() {
                input
                    .image
                    .as_ref()
                    .map(|image| image.metadata.name.clone())
                    .ok_or_else(|| {
                        RuntimeError::Process("image_id or image document is required".into())
                    })?
            } else {
                input.image_id.clone()
            },
        )?;
        let profile_id = Id::new(
            "profile",
            if input.profile_id == "custom" {
                input
                    .profile
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or(input.profile_id.as_str())
                    .to_owned()
            } else {
                input.profile_id.clone()
            },
        )?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let profile = profile_document(
            workspace.root(),
            profile_id.as_str(),
            input.profile.as_ref(),
        )?;
        let image = input
            .image
            .clone()
            .map(Ok)
            .unwrap_or_else(|| image_document(&workspace, image_id.as_str()))?;
        let identity = input
            .hardware_identity
            .clone()
            .or(hardware_identity_document(workspace.root(), &profile)?);
        let mut context = input.context.clone();
        context.instance_id = instance_id.as_str().to_owned();
        if context.engines.is_empty() {
            context.engines = workspace_engine_capabilities(workspace.root());
        }
        let resolved =
            resolve_domain_instance(profile, identity, image, input.overrides.clone(), context)?;
        let resolved = serde_json::to_value(resolved)?;
        let staging_root = workspace
            .root()
            .join("staging/instances")
            .join(instance_id.as_str());
        std::fs::create_dir_all(&staging_root).map_err(|source| RuntimeError::Io {
            path: staging_root.clone(),
            source,
        })?;
        let staged = staging_root.join(format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir(&staged).map_err(|source| RuntimeError::Io {
            path: staged.clone(),
            source,
        })?;
        let path = staged.join("domain_document.yaml");
        let bytes = serde_yaml::to_string(&resolved)
            .map_err(|error| RuntimeError::Process(error.to_string()))?;
        if let Err(source) = std::fs::write(&path, bytes) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(RuntimeError::Io { path, source });
        }
        let result = workspace.publish_prepared_instance(
            instance_id,
            image_id,
            profile_id,
            &staged,
            "null",
            input.auto_remove,
        );
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&staged);
        }
        result
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
