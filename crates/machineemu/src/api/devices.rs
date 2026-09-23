use super::*;
use axum::Json;
use machineemu_core::runtime::RunningInstance;
use serde_json::{Value, json};
use std::path::{Component, Path as FsPath};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AttachDevice {
    device_id: String,
    hostbus: Option<u16>,
    hostaddr: Option<u16>,
    path: Option<String>,
    read_only: Option<bool>,
    model: Option<String>,
    mac: Option<String>,
    bus: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChangeMedium {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EjectMedium {
    #[serde(default)]
    force: bool,
}

fn invalid(message: &str) -> RuntimeError {
    RuntimeError::Process(message.into())
}
fn reply(result: Result<Value, RuntimeError>) -> axum::response::Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

fn managed_id(kind: &str, value: String) -> Result<Id, RuntimeError> {
    let id = Id::new("device", value)?;
    let prefix = match kind {
        "usb-host" => "me-usbh-",
        "usb-image" => "me-usbi-",
        "iso" => "me-iso-",
        "network" => "me-net-",
        "usbredir" => "me-redir-",
        _ => return Err(invalid("unknown device kind")),
    };
    if !id.as_str().starts_with(prefix) || id.as_str().len() == prefix.len() {
        return Err(invalid("device ID must use its managed prefix"));
    }
    // QMP derived IDs add up to seven bytes and must stay within QEMU's ID limit.
    if id.as_str().len() > 48 {
        return Err(invalid("device ID is too long"));
    }
    Ok(id)
}

fn pci_bus(value: Option<&str>) -> Result<&str, RuntimeError> {
    let bus = value.ok_or_else(|| {
        invalid("bus is required for PCI hotplug; reserve a PCIe root port at VM launch")
    })?;
    if !bus.starts_with("pcie-root-port-")
        || !bus
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(invalid("bus must name a reserved pcie-root-port-* device"));
    }
    Ok(bus)
}

fn media_path(root: &FsPath, value: &str, iso: bool) -> Result<PathBuf, RuntimeError> {
    let relative = FsPath::new(value);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(invalid("media path must be relative to workspace/media"));
    }
    if iso && relative.extension().and_then(|ext| ext.to_str()) != Some("iso") {
        return Err(invalid("CD media must have an .iso extension"));
    }
    let media = root.join("media");
    let base = media
        .canonicalize()
        .map_err(|_| invalid("workspace/media does not exist"))?;
    let path = media
        .join(relative)
        .canonicalize()
        .map_err(|_| invalid("media file does not exist"))?;
    if !path.starts_with(&base) || !path.is_file() {
        return Err(invalid(
            "media path must name a regular file in workspace/media",
        ));
    }
    Ok(path)
}

fn with_running(
    state: &AppState,
    id: String,
    action: impl FnOnce(&mut RunningInstance, &Workspace) -> Result<Value, RuntimeError>,
) -> Result<Value, RuntimeError> {
    let instance_id = Id::new("instance", id.clone())?;
    let lock = instance_lock(state, &id)?;
    let _guard = lock.lock().map_err(|_| invalid("instance lock poisoned"))?;
    let workspace = state
        .workspace
        .lock()
        .map_err(|_| invalid("workspace lock poisoned"))?
        .attach()?;
    let live = workspace
        .live_run(&instance_id)?
        .ok_or_else(|| invalid("instance has no live run"))?;
    let running = state
        .running
        .lock()
        .map_err(|_| invalid("running map lock poisoned"))?
        .get(&id)
        .cloned();
    let running = match running {
        Some(running)
            if running
                .lock()
                .map_err(|_| invalid("run lock poisoned"))?
                .run_id
                == live.run_id =>
        {
            running
        }
        _ => {
            let recovered = workspace
                .recover_instance_run(&instance_id)?
                .ok_or_else(|| invalid("instance has no live run"))?;
            let running = Arc::new(Mutex::new(recovered));
            state
                .running
                .lock()
                .map_err(|_| invalid("running map lock poisoned"))?
                .insert(id.clone(), running.clone());
            running
        }
    };
    let mut running = running.lock().map_err(|_| invalid("run lock poisoned"))?;
    let result = action(&mut running, &workspace);
    if result.is_err() && running.is_recovered() {
        state
            .running
            .lock()
            .map_err(|_| invalid("running map lock poisoned"))?
            .remove(&id);
    }
    result
}

pub(super) async fn attach_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, kind)): Path<(String, String)>,
    Json(request): Json<AttachDevice>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let instance_path_id = id.clone();
    let result = blocking(move || {
        with_running(&state, id, |running, workspace| {
            let device_id = managed_id(&kind, request.device_id)?;
            let response = match kind.as_str() {
                "usb-host" => {
                    let bus = request
                        .hostbus
                        .ok_or_else(|| invalid("hostbus is required"))?;
                    let addr = request
                        .hostaddr
                        .ok_or_else(|| invalid("hostaddr is required"))?;
                    running.usb_host_attach(&device_id, bus, addr)?
                }
                "usb-image" => {
                    let path = media_path(
                        workspace.root(),
                        request
                            .path
                            .as_deref()
                            .ok_or_else(|| invalid("path is required"))?,
                        false,
                    )?;
                    running.usb_image_attach(
                        &device_id,
                        &path,
                        request.read_only.unwrap_or(false),
                    )?
                }
                "iso" => {
                    let path = media_path(
                        workspace.root(),
                        request
                            .path
                            .as_deref()
                            .ok_or_else(|| invalid("path is required"))?,
                        true,
                    )?;
                    running.iso_attach(&device_id, &path, pci_bus(request.bus.as_deref())?)?
                }
                "network" => {
                    let model = request.model.as_deref().unwrap_or("virtio-net-pci");
                    if !matches!(model, "virtio-net-pci" | "e1000" | "rtl8139") {
                        return Err(invalid("unsupported network card model"));
                    }
                    if let Some(mac) = request.mac.as_deref()
                        && (mac.len() != 17
                            || mac.split(':').count() != 6
                            || !mac.split(':').all(|octet| {
                                octet.len() == 2
                                    && octet.bytes().all(|byte| byte.is_ascii_hexdigit())
                            }))
                    {
                        return Err(invalid("invalid MAC address"));
                    }
                    running.network_attach(
                        &device_id,
                        model,
                        request.mac.as_deref(),
                        pci_bus(request.bus.as_deref())?,
                    )?
                }
                "usbredir" => {
                    if device_id.as_str() != "me-redir-0" {
                        return Err(invalid("USB redirection supports device ID me-redir-0"));
                    }
                    let socket = workspace
                        .root()
                        .join("instances")
                        .join(&instance_path_id)
                        .join("usbredir.sock");
                    running.usbredir_attach(&device_id, &socket)?
                }
                _ => return Err(invalid("unknown device kind")),
            };
            Ok(json!({"device_id": device_id.as_str(), "kind": kind, "result": response}))
        })
    })
    .await;
    reply(result)
}

pub(super) async fn detach_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, kind, device_id)): Path<(String, String, String)>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || {
        with_running(&state, id, |running, _| {
            let device_id = managed_id(&kind, device_id)?;
            let completed = running.detach_device(&device_id, &kind)?;
            Ok(json!({"device_id": device_id.as_str(), "kind": kind, "detached": completed}))
        })
    })
    .await;
    reply(result)
}

pub(super) async fn change_iso(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, device_id)): Path<(String, String)>,
    Json(request): Json<ChangeMedium>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || {
        with_running(&state, id, |running, workspace| {
            let device_id = managed_id("iso", device_id)?;
            let path = media_path(workspace.root(), &request.path, true)?;
            let result = running.iso_change(device_id.as_str(), &path)?;
            Ok(json!({"device_id": device_id.as_str(), "result": result}))
        })
    })
    .await;
    reply(result)
}

pub(super) async fn eject_iso(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, device_id)): Path<(String, String)>,
    Json(request): Json<EjectMedium>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || {
        with_running(&state, id, |running, _| {
            let device_id = managed_id("iso", device_id)?;
            let result = running.iso_eject(device_id.as_str(), request.force)?;
            Ok(json!({"device_id": device_id.as_str(), "result": result}))
        })
    })
    .await;
    reply(result)
}

pub(super) async fn list_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, kind)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || {
        with_running(&state, id, |running, _| match kind.as_str() {
            "usb-host" | "usb-image" | "usbredir" => running.query_usb(),
            "iso" => running.query_block(),
            "network" => running.query_network(),
            _ => Err(invalid("unknown device kind")),
        })
    })
    .await;
    reply(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_cannot_target_boot_hardware() {
        assert!(managed_id("network", "me-net-1".into()).is_ok());
        assert!(managed_id("network", "boot-nic".into()).is_err());
        assert!(managed_id("iso", "me-net-1".into()).is_err());
        assert!(pci_bus(Some("pcie-root-port-0")).is_ok());
        assert!(pci_bus(Some("pcie.0")).is_err());
    }

    #[test]
    fn media_paths_stay_inside_workspace_media() {
        let root =
            std::env::temp_dir().join(format!("machineemu-media-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("media")).unwrap();
        std::fs::write(root.join("media/install.iso"), b"iso").unwrap();
        assert!(media_path(&root, "install.iso", true).is_ok());
        assert!(media_path(&root, "../secret.iso", true).is_err());
        assert!(media_path(&root, "/etc/passwd", false).is_err());
        assert!(media_path(&root, "install.iso", false).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
}
