use super::launch::{plan_paths, prepare_paths};
use super::*;
use machineemu_core::{
    engine::{PlanInput, build_plan},
    launch::{LaunchContext, LaunchSpec},
};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{collections::BTreeMap, path::PathBuf};

static NEXT_LIFECYCLE_ID: AtomicU64 = AtomicU64::new(1);

fn lifecycle_id(prefix: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    format!(
        "{prefix}-{now:x}-{:x}",
        NEXT_LIFECYCLE_ID.fetch_add(1, Ordering::Relaxed)
    )
}

/// Render the immutable resolved domain document into the runtime launch
/// contract. This is the only compatibility boundary: the planner receives a
/// normalized profile assembled from the resolved spec, never a source profile.
fn render_domain_launch_plan(
    workspace: &Workspace,
    instance_id: &Id,
    spec: &Value,
) -> Result<LaunchSpec, RuntimeError> {
    let mut profile = spec
        .as_object()
        .cloned()
        .ok_or_else(|| RuntimeError::Process("resolved instance spec must be an object".into()))?;
    // The domain contract uses structured machine and resource values while
    // the existing QEMU planner consumes the equivalent legacy scalar
    // representation.  Normalize that representation at this one renderer
    // boundary; no source profile or image is consulted here.
    if let Some(machine) = profile.get("machine").cloned()
        && let Some(machine_object) = machine.as_object()
    {
        let machine_type = machine_object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RuntimeError::Process("resolved instance machine.type is required".into())
            })?;
        profile.insert("machine".into(), Value::String(machine_type.into()));
        if let Some(smm) = machine_object.get("smm") {
            profile.insert("smm".into(), smm.clone());
        }
        if let Some(accelerator) = machine_object.get("accelerator") {
            let resources = profile
                .entry("resources")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(resources) = resources.as_object_mut() {
                resources
                    .entry("accelerator")
                    .or_insert_with(|| accelerator.clone());
            }
        }
    }
    // The domain document uses structured machine/resources/engine values;
    // the legacy planner's input contract is scalar machine, numeric vcpus,
    // and an ordered engine-track list. Normalize that contract here so the
    // renderer consumes the saved domain snapshot without rereading sources.
    let machine = profile
        .get("machine")
        .and_then(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .or_else(|| value.as_str())
        })
        .ok_or_else(|| RuntimeError::Process("resolved instance has no machine".into()))?
        .to_owned();
    profile.insert("machine".into(), Value::String(machine));
    if let Some(resources) = profile.get_mut("resources").and_then(Value::as_object_mut) {
        if let Some(memory) = resources.get("memory").cloned()
            && let Some(memory_object) = memory.as_object()
            && let Some(bytes) = memory_object.get("bytes").and_then(Value::as_u64)
        {
            resources.insert("memory".into(), Value::String(memory_size(bytes)?));
        }
        if let Some(vcpus) = resources.get("vcpus").cloned()
            && let Some(vcpu_object) = vcpus.as_object()
        {
            let count = vcpu_object
                .get("count")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    RuntimeError::Process(
                        "resolved instance resources.vcpus.count is required".into(),
                    )
                })?;
            resources.insert("vcpus".into(), Value::from(count));
            let mut topology = serde_json::Map::new();
            for key in ["sockets", "dies", "clusters", "cores", "threads"] {
                if let Some(value) = vcpu_object.get(key) {
                    topology.insert(key.into(), value.clone());
                }
            }
            if !topology.is_empty() {
                resources.insert("topology".into(), Value::Object(topology));
            }
        }
    }
    if let Some(firmware) = profile.get_mut("firmware").and_then(Value::as_object_mut) {
        // Resolved artifacts are immutable bindings.  The planner's source
        // resolver is retained as the final QEMU argument writer, so expose
        // those bindings through its source.path vocabulary.
        for (part, default_name) in [("loader", "OVMF_CODE.fd"), ("nvram", "OVMF_VARS.fd")] {
            let Some(section) = firmware.get_mut(part).and_then(Value::as_object_mut) else {
                continue;
            };
            if section.get("source").is_none()
                && let Some(path) = section
                    .get("artifact")
                    .and_then(|artifact| artifact.get("path"))
                    .cloned()
            {
                section.insert("source".into(), serde_json::json!({"path": path}));
            }
            if section.get("source").is_none()
                && let Some(path) = section
                    .get("template")
                    .and_then(|template| template.get("artifact"))
                    .and_then(|artifact| artifact.get("path"))
                    .cloned()
            {
                section.insert("source".into(), serde_json::json!({"path": path}));
            }
            if part == "nvram" && section.get("name").is_none() {
                section.insert("name".into(), Value::String(default_name.into()));
            }
        }
    }
    // Translate the normalized device collections into the planner's final
    // QEMU-facing disk/network fields.  Selection and identity generation have
    // already happened in resolution; this only copies pinned values.
    if let Some(devices) = profile
        .get("devices")
        .cloned()
        .and_then(|v| v.as_object().cloned())
    {
        let mut normalized_devices = serde_json::Map::new();
        for key in [
            "console",
            "guest_agent",
            "nic",
            "mac",
            "video",
            "audio",
            "usb_tablet",
            "usb_mouse",
            "serial",
            "vsock",
            "snapshots",
            "usb",
            "lcd",
            "bluetooth",
        ] {
            if let Some(value) = devices.get(key) {
                normalized_devices.insert(key.into(), value.clone());
            }
        }
        if let Some(graphics) = devices
            .get("graphics")
            .and_then(Value::as_array)
            .and_then(|values| values.first())
        {
            let graphics_type = graphics
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("none");
            normalized_devices.insert("console".into(), serde_json::json!({"type": graphics_type}));
        }
        if let Some(video) = devices
            .get("video")
            .and_then(Value::as_array)
            .and_then(|values| values.first())
        {
            let mut video = video.clone();
            if let Some(object) = video.as_object_mut() {
                object.remove("id");
                object.remove("primary");
                object.remove("heads");
            }
            normalized_devices.insert("video".into(), video);
        }
        if let Some(serial) = devices
            .get("serial")
            .and_then(Value::as_array)
            .and_then(|values| values.first())
        {
            let mut serial = serial.clone();
            if let Some(object) = serial.as_object_mut() {
                object.remove("id");
            }
            normalized_devices.insert("serial".into(), serial);
        }
        profile.insert("devices".into(), Value::Object(normalized_devices));
        if profile.get("storage").is_none()
            && let Some(disk) = devices
                .get("disks")
                .and_then(Value::as_array)
                .and_then(|disks| {
                    disks
                        .iter()
                        .find(|disk| disk.get("role").and_then(Value::as_str) == Some("root"))
                        .or_else(|| disks.first())
                })
        {
            let mut disk_spec = serde_json::Map::new();
            if let Some(driver) = disk.get("driver").and_then(Value::as_object) {
                if let Some(format) = driver.get("type") {
                    disk_spec.insert("format".into(), format.clone());
                }
                if let Some(bus) = driver.get("bus") {
                    disk_spec.insert("bus".into(), bus.clone());
                }
            }
            if let Some(target_bus) = disk.pointer("/target/bus") {
                disk_spec.insert("bus".into(), target_bus.clone());
            }
            if let Some(source) = disk.get("source").and_then(Value::as_object) {
                let mut source_spec = serde_json::Map::new();
                if let Some(path) = source
                    .get("artifact")
                    .and_then(|artifact| artifact.get("path"))
                {
                    source_spec.insert("path".into(), path.clone());
                } else if let Some(component) = source.get("component") {
                    source_spec.insert("image_component".into(), component.clone());
                }
                if !source_spec.is_empty() {
                    disk_spec.insert("source".into(), Value::Object(source_spec));
                }
            }
            if !disk_spec.contains_key("bus") {
                disk_spec.insert("bus".into(), Value::String("virtio".into()));
            }
            profile.insert("storage".into(), serde_json::json!({"disk": disk_spec}));
        }
        if profile.get("network").is_none()
            && let Some(interface) = devices
                .get("interfaces")
                .and_then(Value::as_array)
                .and_then(|interfaces| interfaces.first())
        {
            let mut network = serde_json::Map::new();
            let network_type = interface
                .get("network")
                .and_then(|value| {
                    value
                        .get("type")
                        .and_then(Value::as_str)
                        .or_else(|| value.as_str())
                })
                .unwrap_or("user");
            network.insert("type".into(), Value::String(network_type.into()));
            profile.insert("network".into(), Value::Object(network));
            if let Some(model) = interface
                .get("model")
                .or_else(|| interface.get("device"))
                .cloned()
            {
                profile
                    .entry("devices")
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));
                if let Some(devices) = profile.get_mut("devices").and_then(Value::as_object_mut) {
                    devices.insert("nic".into(), model);
                    if let Some(mac) = interface.get("mac") {
                        devices.insert("mac".into(), mac.clone());
                    }
                }
            }
        }
    }
    let engine = profile
        .get("engine")
        .and_then(|v| v.get("track"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RuntimeError::Process("resolved instance has no pinned engine track".into())
        })?
        .to_owned();
    let executable = profile
        .get("engine")
        .and_then(|v| v.get("executable"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RuntimeError::Process("resolved instance has no pinned engine executable".into())
        })?
        .to_owned();
    if let Some(resources) = profile.get_mut("resources").and_then(Value::as_object_mut)
        && let Some(vcpus) = resources.get("vcpus").cloned()
        && let Some(count) = vcpus
            .as_object()
            .and_then(|value| value.get("count"))
            .and_then(Value::as_u64)
    {
        resources.insert("vcpus".into(), Value::from(count));
    }
    let executable = {
        let path = PathBuf::from(&executable);
        if path.is_absolute() {
            path
        } else {
            workspace
                .root()
                .join("generated-engines")
                .join(&engine)
                .join(path)
        }
    };
    let target = profile
        .get("target")
        .and_then(Value::as_str)
        .or_else(|| {
            profile
                .get("image")
                .and_then(|v| v.get("target"))
                .and_then(Value::as_str)
        })
        .unwrap_or("x86_64-softmmu")
        .to_owned();
    profile.insert("schema_version".into(), Value::from(2));
    profile.insert("id".into(), Value::String(instance_id.as_str().to_owned()));
    profile.insert("engine".into(), serde_json::json!([engine]));
    let image_components = profile
        .get("image")
        .and_then(|v| v.get("components"))
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    v.get("artifact")
                        .and_then(|artifact| artifact.get("path"))
                        .and_then(Value::as_str)
                        .or_else(|| v.get("path").and_then(Value::as_str))
                        .map(|p| {
                            let path = PathBuf::from(p);
                            let path = if path.is_absolute() {
                                path
                            } else {
                                workspace.root().join(path)
                            };
                            (k.clone(), path)
                        })
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let build_digest = spec
        .get("engine")
        .and_then(|v| v.get("build_digest"))
        .and_then(Value::as_str)
        .unwrap_or("0");
    let vnc_auto = profile
        .get("devices")
        .and_then(Value::as_object)
        .and_then(|devices| devices.get("console"))
        .and_then(Value::as_object)
        .and_then(|console| console.get("type"))
        .and_then(Value::as_str)
        == Some("vnc");
    let plan = build_plan(PlanInput {
        profile: Value::Object(profile),
        release_set: serde_json::json!({"schema_version":1,"engines":{engine.clone():{"executable": executable, "build_digest": build_digest}}}),
        bundle_root: workspace.root().join("generated-engines"),
        asset_root: Some(workspace.root().to_owned()),
        image_components,
        target: target.clone(),
        runtime_dir: workspace.root().join("instances").join(instance_id.as_str()),
        state_dir: Some(workspace.root().join("instances").join(instance_id.as_str())),
        seed: None, swtpm: None, bridge_helper: None,
        mac: None, instance: Some(instance_id.as_str().to_owned()),
    }).map_err(|e| RuntimeError::Process(e.to_string()))?;
    let runtime = workspace
        .root()
        .join("instances")
        .join(instance_id.as_str());
    LaunchSpec::from_plan(
        plan,
        LaunchContext {
            workspace: workspace.root(),
            qmp_socket: &runtime.join("sockets/qmp.sock"),
            stdout: Some(&runtime.join("qemu.stdout")),
            stderr: Some(&runtime.join("qemu.stderr")),
            tpm_seed: None,
            vnc_auto,
            helpers: Vec::new(),
        },
    )
    .map_err(|e| RuntimeError::Process(e.to_string()))
}

fn memory_size(bytes: u64) -> Result<String, RuntimeError> {
    const UNITS: &[(&str, u64)] = &[
        ("T", 1 << 40),
        ("G", 1 << 30),
        ("M", 1 << 20),
        ("K", 1 << 10),
    ];
    for (unit, size) in UNITS {
        if bytes >= *size && bytes.is_multiple_of(*size) {
            return Ok(format!("{}{}", bytes / *size, unit));
        }
    }
    if bytes > 0 {
        Ok(bytes.to_string())
    } else {
        Err(RuntimeError::Process(
            "resolved instance memory must be positive".into(),
        ))
    }
}

async fn refresh_vnc_port(argv: &mut [String], auto: bool) -> Result<Option<u16>, RuntimeError> {
    let display = argv.windows(2).position(|pair| {
        pair[0] == "-display"
            && (pair[1].starts_with("vnc=127.0.0.1:") || pair[1].starts_with("vnc=:"))
    });
    let Some(index) = display else {
        if auto {
            return Err(RuntimeError::Process(
                "automatic VNC port requested without a local VNC display".into(),
            ));
        }
        return Ok(None);
    };
    let setting = &argv[index + 1];
    let tail = setting
        .strip_prefix("vnc=127.0.0.1:")
        .or_else(|| setting.strip_prefix("vnc=:"))
        .expect("display matched a supported VNC form");
    let (display_number, suffix) = tail.split_once(',').unwrap_or((tail, ""));
    let number: u16 = display_number
        .parse()
        .map_err(|_| RuntimeError::Process("invalid local VNC display number".into()))?;
    let port = number
        .checked_add(5900)
        .ok_or_else(|| RuntimeError::Process("invalid local VNC port".into()))?;
    if !(5900..=5999).contains(&port) {
        return Err(RuntimeError::Process(
            "local VNC port must be 5900–5999".into(),
        ));
    }
    let candidates = if auto { 5900..=5999 } else { port..=port };
    for candidate in candidates {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", candidate)).await {
            drop(listener);
            if auto {
                argv[index + 1] = format!(
                    "vnc=127.0.0.1:{}{}",
                    candidate - 5900,
                    if suffix.is_empty() {
                        String::new()
                    } else {
                        format!(",{suffix}")
                    }
                );
            }
            return Ok(Some(candidate));
        }
    }
    Err(RuntimeError::Process(if auto {
        "no free VNC port in 5900–5999".into()
    } else {
        format!("VNC port {port} is already in use")
    }))
}

#[allow(clippy::too_many_arguments)]
async fn rollback_failed_start(
    state: &AppState,
    workspace: &mut Workspace,
    instance_id: &Id,
    run_id: &Id,
    operation_id: &Id,
    running: &mut machineemu_core::runtime::AsyncRunningInstance,
    helpers: &mut Vec<ManagedProcess>,
    error: &RuntimeError,
) {
    if workspace
        .stop_instance_async(instance_id, running)
        .await
        .is_err()
    {
        let _ = running.abort_owned_child();
        let _ = workspace.finish_run(run_id, "failed");
        if let Ok(instance) = workspace.instance(instance_id)
            && matches!(
                instance.state.as_str(),
                "starting" | "running" | "paused" | "stopping"
            )
        {
            let _ = workspace.transition_instance(instance_id, "error");
        }
    }
    super::helpers::stop_all(helpers);
    if let Ok(failed) = workspace.fail_operation(operation_id, &error.to_string()) {
        events::publish_operation(state, &failed, Some(run_id.as_str()));
    }
    if let Ok(instance) = workspace.instance(instance_id) {
        let run = workspace.run(run_id).ok();
        events::publish_state(state, &instance, run.as_ref(), "start_failed");
    }
}

pub(super) async fn start_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(input): axum::Json<StartInstance>,
) -> impl IntoResponse {
    start_instance_with_lock(state, headers, id, input, false).await
}

async fn start_instance_with_lock(
    state: AppState,
    headers: HeaderMap,
    id: String,
    input: StartInstance,
    lock_held: bool,
) -> axum::response::Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let watch_state = state.clone();
    let cleanup_state = state.clone();
    let cleanup_id = id.clone();
    let future = async move {
        let input_json = serde_json::to_string(&input)
            .map_err(|error| RuntimeError::Process(error.to_string()))?;
        let instance_id = Id::new("instance", id)?;
        let instance_lock = instance_lock(&state, instance_id.as_str())?;
        let _instance_guard = if lock_held {
            None
        } else {
            Some(instance_lock.lock_owned().await)
        };
        let operation_id = Id::new(
            "operation",
            input
                .operation_id
                .clone()
                .unwrap_or_else(|| lifecycle_id("op")),
        )?;
        let run_id = Id::new(
            "run",
            input.run_id.clone().unwrap_or_else(|| lifecycle_id("run")),
        )?;
        let idempotency_key = input
            .idempotency_key
            .clone()
            .unwrap_or_else(|| format!("start-{}", run_id.as_str()));
        let mut workspace = {
            let owner = state
                .workspace
                .lock()
                .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
            owner.attach()?
        };
        // A migrated/resolved instance is self-contained. Do not materialize
        // or reread its source profile, image, or mutable defaults: the domain
        // document is the authority. A cached launch plan is accepted only as
        // an immutable field inside that complete domain snapshot.
        let mut plan: LaunchSpec = if workspace.has_domain_instance_document(&instance_id)? {
            let document = workspace.domain_instance_document(&instance_id)?;
            let plan = document
                .spec
                .get("launch_plan")
                .or_else(|| document.spec.get("rendered_launch_plan"))
                .cloned();
            match plan {
                Some(plan) => serde_json::from_value(plan)?,
                None => render_domain_launch_plan(&workspace, &instance_id, &document.spec)?,
            }
        } else {
            return Err(RuntimeError::Process(format!(
                "instance {} has no complete domain document; run machineemu-migrate before starting it",
                instance_id.as_str()
            )));
        };
        super::launch::apply_daemon_helpers(&mut plan, &state.helpers);
        let (qmp, stdout, stderr) = plan_paths(workspace.root(), &plan)?;
        let workspace_root = workspace.root().to_owned();
        if let Some(preparation) = &plan.preparation {
            let (backing, nvram, tpm) = prepare_paths(workspace.root(), preparation)?;
            workspace.prepare_instance_files_sized(
                &instance_id,
                &backing,
                &preparation.backing_format,
                preparation.disk_size.as_deref(),
                nvram.as_deref(),
                tpm.as_deref(),
            )?;
        }
        let mut argv = plan.argv.clone();
        let vnc_port = refresh_vnc_port(&mut argv, plan.vnc_auto).await?;
        let mut helpers = Vec::new();
        if let Some(helper_argv) = &plan.helper_argv {
            let helper_id = Id::new("run", format!("{}-helper", run_id.as_str()))?;
            helpers.push(ManagedProcess::spawn_systemd_scope(
                helper_id,
                helper_argv,
                None,
                None,
            )?);
        }
        for spec in plan.helpers.iter().filter(|spec| !spec.after_qemu) {
            match super::helpers::spawn(&workspace_root, &instance_id, &run_id, spec).await {
                Ok(process) => helpers.push(process),
                Err(error) => {
                    super::helpers::stop_all(&mut helpers);
                    return Err(error);
                }
            }
        }
        let post_helpers = plan.helpers.iter().any(|spec| spec.after_qemu);
        if post_helpers && !argv.iter().any(|value| value == "-S") {
            argv.push("-S".into());
        }
        let on_operation = |operation: &machineemu_core::domain::Operation| {
            events::publish_operation(&state, operation, Some(run_id.as_str()));
        };
        let on_state = |instance: &machineemu_core::domain::Instance| {
            events::publish_state_fields(
                &state,
                instance,
                Some(run_id.as_str()),
                matches!(instance.state.as_str(), "running" | "paused").then_some("running"),
                if instance.state == "error" {
                    "start_failed"
                } else {
                    "start"
                },
            );
        };
        let running = workspace
            .start_instance_async(machineemu_core::runtime::StartRequest {
                operation_id: operation_id.clone(),
                run_id: run_id.clone(),
                instance_id: instance_id.clone(),
                idempotency_key: &idempotency_key,
                input_json: &input_json,
                argv: &argv,
                qmp_socket: &qmp,
                stdout: stdout.as_deref(),
                stderr: stderr.as_deref(),
                qmp_timeout: std::time::Duration::from_secs(10),
                on_operation: Some(&on_operation),
                on_state: Some(&on_state),
                complete_operation: false,
            })
            .await;
        let mut running = match running {
            Ok(running) => running,
            Err(error) => {
                super::helpers::stop_all(&mut helpers);
                return Err(error);
            }
        };
        for spec in plan.helpers.iter().filter(|spec| spec.after_qemu) {
            match super::helpers::spawn(&workspace_root, &instance_id, &run_id, spec).await {
                Ok(mut process) => {
                    if spec.name == "bluetooth"
                        && let Err(error) = super::helpers::wait_bluetooth_attached(
                            &state,
                            &instance_id,
                            &mut process,
                        )
                        .await
                    {
                        let _ = process.terminate_gracefully();
                        rollback_failed_start(
                            &state,
                            &mut workspace,
                            &instance_id,
                            &run_id,
                            &operation_id,
                            &mut running,
                            &mut helpers,
                            &error,
                        )
                        .await;
                        return Err(error);
                    }
                    helpers.push(process);
                }
                Err(error) => {
                    rollback_failed_start(
                        &state,
                        &mut workspace,
                        &instance_id,
                        &run_id,
                        &operation_id,
                        &mut running,
                        &mut helpers,
                        &error,
                    )
                    .await;
                    return Err(error);
                }
            }
        }
        if post_helpers {
            match workspace
                .resume_instance_async(&instance_id, &mut running)
                .await
            {
                Ok(instance) => events::publish_state_fields(
                    &state,
                    &instance,
                    Some(run_id.as_str()),
                    Some("running"),
                    "resume",
                ),
                Err(error) => {
                    rollback_failed_start(
                        &state,
                        &mut workspace,
                        &instance_id,
                        &run_id,
                        &operation_id,
                        &mut running,
                        &mut helpers,
                        &error,
                    )
                    .await;
                    return Err(error);
                }
            }
        }
        let publication = (|| -> Result<_, RuntimeError> {
            workspace.save_run_helpers(&run_id, &helpers)?;
            let result = workspace.instance(&instance_id)?;
            let completed = workspace.complete_operation(
                &operation_id,
                &serde_json::json!({"state": result.state}).to_string(),
            )?;
            events::publish_operation(&state, &completed, Some(run_id.as_str()));
            Ok(result)
        })();
        let result = match publication {
            Ok(instance) => instance,
            Err(error) => {
                rollback_failed_start(
                    &state,
                    &mut workspace,
                    &instance_id,
                    &run_id,
                    &operation_id,
                    &mut running,
                    &mut helpers,
                    &error,
                )
                .await;
                return Err(error);
            }
        };
        let connection = Arc::new(tokio::sync::Mutex::new(running));
        supervisor::install(
            &state,
            instance_id.as_str(),
            run_id.clone(),
            Some(connection.clone()),
            helpers,
        )?;
        // Only published, fully initialized runs outlive this daemon.
        connection.lock().await.preserve_on_drop();
        if let Some(owner) = state
            .supervisors
            .lock()
            .map_err(|_| RuntimeError::Process("supervisor lock poisoned".into()))?
            .get_mut(instance_id.as_str())
        {
            for helper in &mut owner.helpers {
                helper.preserve_on_drop();
            }
        }
        drop(workspace);
        Ok((result, run_id, operation_id, vnc_port))
    };
    let result = tokio::spawn(future)
        .await
        .map_err(|error| RuntimeError::Process(format!("start task failed: {error}")))
        .and_then(|result| result);
    match result {
        Ok((instance, run_id, operation_id, vnc_port)) => {
            if let Ok(workspace) = watch_state.workspace.lock()
                && let Ok(Some(run)) = workspace.active_run(&instance.instance_id)
            {
                run_events::spawn(&watch_state, run);
            }
            let mut value = serde_json::to_value(&instance).unwrap_or_default();
            value["run_id"] = serde_json::Value::String(run_id.as_str().into());
            value["operation_id"] = serde_json::Value::String(operation_id.as_str().into());
            if let Some(port) = vnc_port {
                value["vnc_port"] = serde_json::Value::from(port);
            }
            axum::Json(value).into_response()
        }
        Err(error) => {
            if !lock_held {
                let _ = blocking(move || -> Result<(), RuntimeError> {
                    let instance_id = Id::new("instance", cleanup_id)?;
                    let lock = instance_lock(&cleanup_state, instance_id.as_str())?;
                    let _guard = lock.blocking_lock();
                    let workspace = cleanup_state
                        .workspace
                        .lock()
                        .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
                    if workspace.active_run(&instance_id)?.is_none()
                        && matches!(
                            workspace.instance(&instance_id)?.state.as_str(),
                            "created" | "stopped" | "error"
                        )
                    {
                        instances::remove_if_disposable(
                            &cleanup_state,
                            &workspace,
                            &instance_id,
                            None,
                            "start_failed",
                        )?;
                    }
                    Ok(())
                })
                .await;
            }
            (
                StatusCode::CONFLICT,
                axum::Json(ErrorBody {
                    error: error.to_string(),
                }),
            )
                .into_response()
        }
    }
}

pub(super) async fn stop_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    lifecycle_action(state, headers, id, "stop").await
}

pub(super) async fn restart_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let lock = match instance_lock(&state, &id) {
        Ok(lock) => lock,
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                axum::Json(ErrorBody {
                    error: error.to_string(),
                }),
            )
                .into_response();
        }
    };
    let _restart_guard = lock.lock_owned().await;
    let check_state = state.clone();
    let check_id = id.clone();
    let prepared = blocking(move || {
        let instance_id = Id::new("instance", check_id)?;
        let workspace = check_state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let instance = workspace.instance(&instance_id)?;
        let Some((_, auto_remove)) = workspace.instance_launch(&instance_id)? else {
            return Err(RuntimeError::Process(
                "instance has no saved launch plan".into(),
            ));
        };
        if auto_remove {
            return Err(RuntimeError::Process(
                "disposable instances cannot restart".into(),
            ));
        }
        Ok(instance.state)
    })
    .await;
    let current = match prepared {
        Ok(current) => current,
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                axum::Json(ErrorBody {
                    error: error.to_string(),
                }),
            )
                .into_response();
        }
    };
    if matches!(current.as_str(), "running" | "paused") {
        let response =
            lifecycle_action_with_lock(state.clone(), headers.clone(), id.clone(), "stop", true)
                .await;
        if !response.status().is_success() {
            return response;
        }
    } else if !matches!(current.as_str(), "created" | "stopped" | "error") {
        return (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: format!("cannot restart instance in state {current}"),
            }),
        )
            .into_response();
    }
    start_instance_with_lock(
        state,
        headers,
        id,
        StartInstance {
            operation_id: None,
            run_id: None,
            idempotency_key: None,
        },
        true,
    )
    .await
}

pub(super) async fn pause_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    lifecycle_action(state, headers, id, "pause").await
}

pub(super) async fn resume_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    lifecycle_action(state, headers, id, "resume").await
}

pub(super) async fn reset_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    lifecycle_action(state, headers, id, "reset").await
}

pub(super) async fn lifecycle_action(
    state: AppState,
    headers: HeaderMap,
    id: String,
    action: &str,
) -> axum::response::Response {
    lifecycle_action_with_lock(state, headers, id, action, false).await
}

async fn lifecycle_action_with_lock(
    state: AppState,
    headers: HeaderMap,
    id: String,
    action: &str,
    lock_held: bool,
) -> axum::response::Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let action = action.to_owned();
    let future = async move {
        let instance_id = Id::new("instance", id.clone())?;
        let instance_lock = instance_lock(&state, &id)?;
        let _instance_guard = if lock_held {
            None
        } else {
            Some(instance_lock.lock_owned().await)
        };
        let mut workspace = {
            let owner = state
                .workspace
                .lock()
                .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
            owner.attach()?
        };
        let running = supervisor::connection(&state, &id)?;
        let running = if running.is_some() {
            running
        } else if let Some(recovered) = workspace.recover_instance_run_async(&instance_id).await? {
            let run_id = recovered.run_id.clone();
            let recovered = Arc::new(tokio::sync::Mutex::new(recovered));
            let helpers = workspace.recover_run_helpers(&run_id)?;
            supervisor::install(&state, &id, run_id, Some(recovered.clone()), helpers)?;
            Some(recovered)
        } else {
            None
        };
        let Some(running) = running else {
            let instance = workspace.instance(&instance_id)?;
            if action == "stop" && matches!(instance.state.as_str(), "stopped" | "error") {
                return Ok(instance);
            }
            return Err(RuntimeError::Process("instance has no live run".into()));
        };
        let mut running = running.lock().await;
        let instance = match action.as_str() {
            "stop" => {
                workspace
                    .stop_instance_async(&instance_id, &mut running)
                    .await
            }
            "pause" => {
                workspace
                    .pause_instance_async(&instance_id, &mut running)
                    .await
            }
            "resume" => {
                workspace
                    .resume_instance_async(&instance_id, &mut running)
                    .await
            }
            "reset" => workspace
                .reset_instance_async(&instance_id, &mut running)
                .await
                .map(|_| workspace.instance(&instance_id))
                .and_then(|result| result),
            _ => Err(RuntimeError::Process("unknown lifecycle action".into())),
        };
        if action == "stop" && instance.is_ok() {
            supervisor::teardown(&state, &id, &running.run_id)?;
        } else if instance.is_err() && running.is_recovered() {
            supervisor::disconnect(&state, &id, &running.run_id)?;
        }
        if let Ok(ref instance) = instance {
            let run = workspace.run(&running.run_id).ok();
            events::publish_state(&state, instance, run.as_ref(), &action);
        }
        if action == "stop" && instance.is_ok() {
            instances::remove_if_disposable(
                &state,
                &workspace,
                &instance_id,
                Some(&running.run_id),
                "operator_stop",
            )?;
        }
        instance
    };
    let result = tokio::spawn(future)
        .await
        .map_err(|error| RuntimeError::Process(format!("lifecycle task failed: {error}")))
        .and_then(|result| result);
    match result {
        Ok(instance) => axum::Json(instance).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod vnc_start_tests {
    use super::*;

    #[tokio::test]
    async fn start_rechecks_explicit_port_and_reselects_auto_port() {
        let (busy_port, _listener) = (5900..=5999)
            .find_map(|port| {
                std::net::TcpListener::bind(("127.0.0.1", port))
                    .ok()
                    .map(|listener| (port, listener))
            })
            .unwrap();
        let mut explicit = vec![
            "-display".into(),
            format!("vnc=127.0.0.1:{},password-secret=secret", busy_port - 5900),
        ];
        assert!(refresh_vnc_port(&mut explicit, false).await.is_err());
        let mut automatic = explicit.clone();
        let selected = refresh_vnc_port(&mut automatic, true)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(selected, busy_port);
        assert_eq!(
            automatic[1],
            format!("vnc=127.0.0.1:{},password-secret=secret", selected - 5900)
        );
    }
}
