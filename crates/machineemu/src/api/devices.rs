use super::*;
use axum::Json;
use machineemu_core::protocols::async_qmp::AsyncQmp;
use serde_json::{Value, json};
use std::path::{Component, Path as FsPath};
use tokio::sync::OwnedMutexGuard;

#[derive(Deserialize, utoipa::ToSchema)]
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

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ChangeMedium {
    path: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
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

async fn media_path(root: &FsPath, value: &str, iso: bool) -> Result<PathBuf, RuntimeError> {
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
    let base = tokio::fs::canonicalize(&media)
        .await
        .map_err(|_| invalid("workspace/media does not exist"))?;
    let path = tokio::fs::canonicalize(media.join(relative))
        .await
        .map_err(|_| invalid("media file does not exist"))?;
    if !path.starts_with(&base)
        || !tokio::fs::metadata(&path)
            .await
            .is_ok_and(|metadata| metadata.is_file())
    {
        return Err(invalid(
            "media path must name a regular file in workspace/media",
        ));
    }
    Ok(path)
}

struct DeviceSession {
    _gate: OwnedMutexGuard<()>,
    qmp: supervisor::QmpSession,
    root: PathBuf,
    instance_id: String,
}

async fn session(state: &AppState, id: String) -> Result<DeviceSession, RuntimeError> {
    let instance_id = Id::new("instance", id.clone())?;
    let gate = instance_lock(state, &id)?.lock_owned().await;
    let workspace = state.workspace.clone();
    let (run, root) = blocking(move || {
        let workspace = workspace
            .lock()
            .map_err(|_| invalid("workspace lock poisoned"))?;
        let run = workspace
            .live_run(&instance_id)?
            .ok_or_else(|| invalid("instance has no live run"))?;
        Ok((run, workspace.root().to_owned()))
    })
    .await?;
    let qmp = supervisor::qmp(state, &run).await?;
    Ok(DeviceSession {
        _gate: gate,
        qmp,
        root,
        instance_id: id,
    })
}

async fn with_session<F, Fut>(
    state: &AppState,
    id: String,
    action: F,
) -> Result<Value, RuntimeError>
where
    F: FnOnce(DeviceSession) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RuntimeError>> + Send + 'static,
{
    let session = session(state, id).await?;
    tokio::spawn(action(session))
        .await
        .map_err(|error| invalid(&format!("device task failed: {error}")))?
}

fn file_name(path: &FsPath) -> Result<&str, RuntimeError> {
    path.to_str()
        .ok_or_else(|| invalid("media path is not UTF-8"))
}

async fn attach(
    session: &mut DeviceSession,
    kind: &str,
    id: &Id,
    request: &AttachDevice,
) -> Result<Value, RuntimeError> {
    let qmp = &mut session.qmp;
    match kind {
        "usb-host" => {
            let bus = request
                .hostbus
                .ok_or_else(|| invalid("hostbus is required"))?;
            let addr = request
                .hostaddr
                .ok_or_else(|| invalid("hostaddr is required"))?;
            qmp.execute(
                "device_add",
                json!({"driver":"usb-host","id":id.as_str(),"hostbus":bus,"hostaddr":addr}),
            )
            .await
        }
        "usb-image" => {
            let path = media_path(
                &session.root,
                request
                    .path
                    .as_deref()
                    .ok_or_else(|| invalid("path is required"))?,
                false,
            )
            .await?;
            let node = format!("{}-node", id.as_str());
            qmp.execute("blockdev-add", json!({"node-name":node,"driver":"raw","read-only":request.read_only.unwrap_or(false),"file":{"driver":"file","filename":file_name(&path)?}})).await?;
            let result = qmp
                .execute(
                    "device_add",
                    json!({"driver":"usb-storage","id":id.as_str(),"drive":node}),
                )
                .await;
            if result.is_err() {
                let _ = qmp.execute("blockdev-del", json!({"node-name":node})).await;
            }
            result
        }
        "iso" => {
            let path = media_path(
                &session.root,
                request
                    .path
                    .as_deref()
                    .ok_or_else(|| invalid("path is required"))?,
                true,
            )
            .await?;
            let bus = pci_bus(request.bus.as_deref())?;
            let controller = format!("{}-ctl", id.as_str());
            let node = format!("{}-node", id.as_str());
            qmp.execute(
                "device_add",
                json!({"driver":"virtio-scsi-pci","id":controller,"bus":bus}),
            )
            .await?;
            if let Err(error) = qmp.execute("blockdev-add", json!({"node-name":node,"driver":"raw","read-only":true,"file":{"driver":"file","filename":file_name(&path)?}})).await {
                let _ = qmp.execute("device_del", json!({"id":controller})).await;
                return Err(error);
            }
            let result = qmp.execute("device_add", json!({"driver":"scsi-cd","id":id.as_str(),"bus":format!("{controller}.0"),"drive":node})).await;
            if result.is_err() {
                let _ = qmp.execute("blockdev-del", json!({"node-name":node})).await;
                let _ = qmp.execute("device_del", json!({"id":controller})).await;
            }
            result
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
                        octet.len() == 2 && octet.bytes().all(|byte| byte.is_ascii_hexdigit())
                    }))
            {
                return Err(invalid("invalid MAC address"));
            }
            let bus = pci_bus(request.bus.as_deref())?;
            let backend = format!("{}-netdev", id.as_str());
            qmp.execute("netdev_add", json!({"type":"user","id":backend}))
                .await?;
            let mut device = json!({"driver":model,"id":id.as_str(),"netdev":backend,"bus":bus});
            if let Some(mac) = &request.mac {
                device["mac"] = Value::String(mac.clone());
            }
            let result = qmp.execute("device_add", device).await;
            if result.is_err() {
                let _ = qmp.execute("netdev_del", json!({"id":backend})).await;
            }
            result
        }
        "usbredir" => {
            if id.as_str() != "me-redir-0" {
                return Err(invalid("USB redirection supports device ID me-redir-0"));
            }
            let socket = session
                .root
                .join("instances")
                .join(&session.instance_id)
                .join("usbredir.sock");
            let chardev = format!("{}-char", id.as_str());
            qmp.execute("chardev-add", json!({"id":chardev,"backend":{"type":"socket","data":{"addr":{"type":"unix","data":{"path":file_name(&socket)?}},"server":true,"wait":false}}})).await?;
            let result = qmp
                .execute(
                    "device_add",
                    json!({"driver":"usb-redir","id":id.as_str(),"chardev":chardev}),
                )
                .await;
            if result.is_err() {
                let _ = qmp.execute("chardev-remove", json!({"id":chardev})).await;
            }
            result
        }
        _ => Err(invalid("unknown device kind")),
    }
}

async fn detach(qmp: &mut AsyncQmp, id: &Id, kind: &str) -> Result<bool, RuntimeError> {
    qmp.execute("device_del", json!({"id":id.as_str()})).await?;
    if !qmp
        .wait_device_deleted(id.as_str(), std::time::Duration::from_secs(5))
        .await?
    {
        return Ok(false);
    }
    let cleanup = match kind {
        "usb-image" | "iso" => Some(("blockdev-del", "node-name", "-node")),
        "network" => Some(("netdev_del", "id", "-netdev")),
        "usbredir" => Some(("chardev-remove", "id", "-char")),
        "usb-host" => None,
        _ => return Err(invalid("unknown device kind")),
    };
    if let Some((command, key, suffix)) = cleanup {
        let mut arguments = serde_json::Map::new();
        arguments.insert(
            key.into(),
            Value::String(format!("{}{suffix}", id.as_str())),
        );
        qmp.execute(command, Value::Object(arguments)).await?;
    }
    if kind == "iso" {
        let controller = format!("{}-ctl", id.as_str());
        qmp.execute("device_del", json!({"id":controller})).await?;
        let _ = qmp
            .wait_device_deleted(&controller, std::time::Duration::from_secs(5))
            .await?;
    }
    Ok(true)
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
    let result = with_session(&state, id, move |mut session| async move {
        let device_id = managed_id(&kind, request.device_id.clone())?;
        let response = attach(&mut session, &kind, &device_id, &request).await?;
        Ok(json!({"device_id": device_id.as_str(), "kind": kind, "result": response}))
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
    let result = with_session(&state, id, move |mut session| async move {
        let device_id = managed_id(&kind, device_id)?;
        let completed = detach(&mut session.qmp, &device_id, &kind).await?;
        Ok(json!({"device_id": device_id.as_str(), "kind": kind, "detached": completed}))
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
    let result = with_session(&state, id, move |mut session| async move {
        let device_id = managed_id("iso", device_id)?;
        let path = media_path(&session.root, &request.path, true).await?;
        let result = session.qmp.execute("blockdev-change-medium", json!({"id":device_id.as_str(),"filename":file_name(&path)?,"format":"raw","read-only-mode":"read-only"})).await?;
        Ok(json!({"device_id": device_id.as_str(), "result": result}))
    }).await;
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
    let result = with_session(&state, id, move |mut session| async move {
        let device_id = managed_id("iso", device_id)?;
        session
            .qmp
            .execute(
                "blockdev-open-tray",
                json!({"id":device_id.as_str(),"force":request.force}),
            )
            .await?;
        session
            .qmp
            .execute("blockdev-remove-medium", json!({"id":device_id.as_str()}))
            .await?;
        let result = session
            .qmp
            .execute("blockdev-close-tray", json!({"id":device_id.as_str()}))
            .await?;
        Ok(json!({"device_id": device_id.as_str(), "result": result}))
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
    let result = with_session(&state, id, move |mut session| async move {
        match kind.as_str() {
            "usb-host" | "usb-image" | "usbredir" => {
                session
                    .qmp
                    .execute("human-monitor-command", json!({"command-line":"info usb"}))
                    .await
            }
            "iso" => session.qmp.execute("query-block", Value::Null).await,
            "network" => session.qmp.execute("query-pci", Value::Null).await,
            _ => Err(invalid("unknown device kind")),
        }
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

    #[tokio::test]
    async fn media_paths_stay_inside_workspace_media() {
        let root =
            std::env::temp_dir().join(format!("machineemu-media-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("media")).unwrap();
        std::fs::write(root.join("media/install.iso"), b"iso").unwrap();
        assert!(media_path(&root, "install.iso", true).await.is_ok());
        assert!(media_path(&root, "../secret.iso", true).await.is_err());
        assert!(media_path(&root, "/etc/passwd", false).await.is_err());
        assert!(media_path(&root, "install.iso", false).await.is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
}
