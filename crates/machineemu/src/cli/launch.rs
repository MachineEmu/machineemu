use super::client::ensure_daemon;
use super::*;
use machineemu_core::launch::{HelperSpec, LaunchContext, LaunchSpec};
use sha2::{Digest, Sha256};
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LaunchMode {
    Create,
    Run,
}
pub(super) struct RunOptions<'a> {
    pub profile_name: Option<&'a str>,
    pub instance: Option<&'a str>,
    pub image: Option<&'a str>,
    pub force: bool,
    pub seed: Option<&'a Path>,
    pub disk_size: Option<&'a str>,
    pub net: &'a str,
    pub vnc: &'a str,
    pub vnc_password_file: Option<&'a Path>,
    pub h264: bool,
    pub hardware: &'a super::hardware::HardwareArgs,
    pub external_qmp_socket: Option<&'a Path>,
    pub fresh: bool,
    pub auto_remove: bool,
    pub mode: LaunchMode,
    pub workspace_root: &'a Path,
    pub qemu: &'a Path,
    pub mac: Option<&'a str>,
    pub daemon: &'a str,
    pub token: &'a str,
}

pub(super) async fn create_from_document(
    file: &Path,
    hardware: &super::hardware::HardwareArgs,
    workspace_root: &Path,
    daemon: &str,
    token: &str,
    start: bool,
    auto_remove: bool,
) -> Result<(), machineemu_core::engine::Error> {
    let mut value = load_document(file)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("revision");
    }
    let mut document: machineemu_core::storage::InstanceDocument = serde_json::from_value(value)
        .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
    if document.schema_version != 1 || document.launch_plan.is_none() {
        return Err(machineemu_core::engine::Error::Invalid(
            "instance file requires schema_version 1 and a launch_plan".into(),
        ));
    }
    Id::new("instance", document.instance_id.clone())
        .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
    if auto_remove {
        document.auto_remove = true;
    }
    let id = document.instance_id.clone();
    let (config, config_path) = machineemu_core::config::load_config(None)
        .map_err(|error| machineemu_core::engine::Error::Runtime(error.to_string()))?;
    let configured = config
        .client
        .as_ref()
        .and_then(|c| c.workspace.clone())
        .or_else(|| config.server.as_ref().and_then(|s| s.workspace.clone()));
    let workspace = if workspace_root == Path::new("machineemu-workspace") {
        configured
            .map(|p| machineemu_core::config::resolve_config_path(config_path.as_deref(), p))
            .unwrap_or_else(|| workspace_root.into())
    } else {
        workspace_root.into()
    };
    let (daemon, token) = effective_client(daemon, token)?;
    ensure_daemon(&daemon, &token, &workspace).await?;
    let mut launch_plan: LaunchSpec = serde_json::from_value(document.launch_plan.take().unwrap())
        .map_err(|e| machineemu_core::engine::Error::Invalid(e.to_string()))?;
    hardware.apply(&mut launch_plan, &workspace, &id)?;
    document.launch_plan = Some(serde_json::to_value(launch_plan).unwrap());
    let value = serde_json::to_value(document)
        .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
    daemon_request(&daemon, &token, "POST", "/api/v2/instances", Some(value)).await?;
    if start {
        daemon_request(
            &daemon,
            &token,
            "POST",
            &format!("/api/v2/instances/{id}/start"),
            Some(serde_json::json!({})),
        )
        .await?;
        println!("started {id}");
    } else {
        println!("created {id}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn update_existing_instance_config(
    daemon: &str,
    token: &str,
    workspace_root: &Path,
    instance: &str,
    image: Option<&str>,
    force: bool,
    seed: Option<&Path>,
    disk_size: Option<&str>,
    net: &str,
    external_qmp_socket: Option<&Path>,
    qemu: &Path,
    mac: Option<&str>,
    auto_remove: bool,
    hardware: &super::hardware::HardwareArgs,
) -> Result<(), machineemu_core::engine::Error> {
    if image.is_some() || force {
        return Err(machineemu_core::engine::Error::Invalid(
            "--image/--force require a profile; use --profile PROFILE INSTANCE or --fresh".into(),
        ));
    }
    if seed.is_some() {
        return Err(machineemu_core::engine::Error::Invalid(
            "--seed requires a profile refresh; use --profile PROFILE INSTANCE or --fresh".into(),
        ));
    }
    if disk_size.is_some() {
        return Err(machineemu_core::engine::Error::Invalid(
            "--disk-size cannot change on an existing instance; use --fresh".into(),
        ));
    }
    if net != "profile" {
        return Err(machineemu_core::engine::Error::Invalid(
            "--net requires a profile refresh; use --network0/--network for existing instances"
                .into(),
        ));
    }
    if external_qmp_socket.is_some() {
        return Err(machineemu_core::engine::Error::Invalid(
            "--qmp-socket requires a profile refresh; use --profile PROFILE INSTANCE or --fresh"
                .into(),
        ));
    }
    if qemu != Path::new("/run/current-system/sw/bin/qemu-system-x86_64") {
        return Err(machineemu_core::engine::Error::Invalid(
            "--qemu requires a profile refresh; use --profile PROFILE INSTANCE or --fresh".into(),
        ));
    }
    if mac.is_some() {
        return Err(machineemu_core::engine::Error::Invalid(
            "--mac requires a profile refresh; use --profile PROFILE INSTANCE or --fresh".into(),
        ));
    }
    if !auto_remove && !hardware.has_updates() {
        return Ok(());
    }
    let path = format!("/api/v2/instances/{instance}/config");
    let mut document = daemon_request(daemon, token, "GET", &path, None).await?;
    let mut launch_plan: LaunchSpec = serde_json::from_value(document["launch_plan"].clone())
        .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
    hardware.apply(&mut launch_plan, workspace_root, instance)?;
    document["launch_plan"] = serde_json::to_value(launch_plan).unwrap();
    if auto_remove {
        document["auto_remove"] = serde_json::Value::Bool(true);
    }
    daemon_request(daemon, token, "PUT", &path, Some(document)).await?;
    Ok(())
}

async fn start_configured_instance(
    daemon: &str,
    token: &str,
    instance: &str,
    auto_remove: bool,
) -> Result<(), machineemu_core::engine::Error> {
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    );
    let started = daemon_request(
        daemon,
        token,
        "POST",
        &format!("/api/v2/instances/{instance}/start"),
        Some(serde_json::json!({
            "operation_id": format!("start-{suffix}"),
            "run_id": format!("run-{suffix}"),
            "idempotency_key": format!("run-{suffix}")
        })),
    )
    .await;
    if started.is_err() && auto_remove {
        let _ = daemon_request(
            daemon,
            token,
            "DELETE",
            &format!("/api/v2/instances/{instance}"),
            None,
        )
        .await;
    }
    let started = started?;
    println!("started {instance}");
    if let Some(port) = started["vnc_port"].as_u64() {
        println!("VNC: 127.0.0.1:{port}");
    }
    Ok(())
}

pub(super) async fn run_rust_owned(
    options: RunOptions<'_>,
) -> Result<(), machineemu_core::engine::Error> {
    let RunOptions {
        profile_name,
        instance,
        image,
        force,
        seed,
        disk_size,
        net,
        vnc,
        vnc_password_file,
        h264,
        hardware,
        external_qmp_socket,
        fresh,
        auto_remove,
        mode,
        workspace_root,
        qemu,
        mac,
        daemon,
        token,
    } = options;
    let instance = instance.ok_or_else(|| {
        machineemu_core::engine::Error::Invalid(
            match mode {
                LaunchMode::Create => "create requires INSTANCE",
                LaunchMode::Run => "run requires INSTANCE",
            }
            .into(),
        )
    })?;
    Id::new("instance", instance.to_owned())
        .map_err(|error| machineemu_core::engine::Error::Runtime(error.to_string()))?;
    let (config, config_path) = machineemu_core::config::load_config(None)
        .map_err(|error| machineemu_core::engine::Error::Runtime(error.to_string()))?;
    let configured_workspace = config
        .client
        .as_ref()
        .and_then(|client| client.workspace.clone())
        .or_else(|| {
            config
                .server
                .as_ref()
                .and_then(|server| server.workspace.clone())
        });
    let workspace_path = if workspace_root == Path::new("machineemu-workspace") {
        configured_workspace
            .map(|path| machineemu_core::config::resolve_config_path(config_path.as_deref(), path))
            .unwrap_or_else(|| workspace_root.to_owned())
    } else {
        workspace_root.to_owned()
    };
    let workspace_root = workspace_path.canonicalize().map_err(|error| {
        machineemu_core::engine::Error::Invalid(format!("cannot open workspace: {error}"))
    })?;
    let (daemon, token) = effective_client(daemon, token)?;
    ensure_daemon(&daemon, &token, &workspace_root).await?;
    let existing = daemon_request(
        &daemon,
        &token,
        "GET",
        &format!("/api/v2/instances/{instance}"),
        None,
    )
    .await
    .ok();
    let exists = existing.is_some();
    if h264 && exists && !fresh {
        return Err(machineemu_core::engine::Error::Invalid(
            "--h264 for an existing instance requires --fresh".into(),
        ));
    }
    if mode == LaunchMode::Create && fresh {
        return Err(machineemu_core::engine::Error::Invalid(
            "create does not accept --fresh".into(),
        ));
    }
    if mode == LaunchMode::Create && auto_remove {
        return Err(machineemu_core::engine::Error::Invalid(
            "--rm is only valid with run".into(),
        ));
    }
    if exists && mode == LaunchMode::Create {
        return Err(machineemu_core::engine::Error::Invalid(format!(
            "instance {instance} already exists; use start {instance}"
        )));
    }
    if exists && !fresh && mode == LaunchMode::Run && profile_name.is_none() {
        update_existing_instance_config(
            &daemon,
            &token,
            &workspace_root,
            instance,
            image,
            force,
            seed,
            disk_size,
            net,
            external_qmp_socket,
            qemu,
            mac,
            auto_remove,
            hardware,
        )
        .await?;
        return start_configured_instance(&daemon, &token, instance, auto_remove).await;
    }
    let profile_name = profile_name.ok_or_else(|| {
        machineemu_core::engine::Error::Invalid(
            "profile is required to create or refresh an instance; use --profile PROFILE INSTANCE"
                .into(),
        )
    })?;
    let instance_dir = workspace_root.join("instances").join(instance);
    let profile_path = resolve_profile_path(profile_name, &workspace_root);
    let mut profile = load_document(&profile_path)?;
    let selected_image = image
        .or_else(|| profile.get("image").and_then(serde_json::Value::as_str))
        .map(str::to_owned);
    let tpm_seed = selected_image
        .as_deref()
        .map(|image_id| {
            let selected = Workspace::list_images(&workspace_root)
                .map_err(|error| machineemu_core::engine::Error::Runtime(error.to_string()))?
                .into_iter()
                .find(|image| image.image_id.as_str() == image_id)
                .ok_or_else(|| {
                    machineemu_core::engine::Error::Invalid(format!(
                        "image {image_id:?} is not registered in {}",
                        workspace_root.display()
                    ))
                })?;
            bind_image(&mut profile, &selected, &workspace_root, force)
        })
        .transpose()?
        .flatten();
    let profile_object = profile.as_object_mut().ok_or_else(|| {
        machineemu_core::engine::Error::Invalid("profile must be a mapping".into())
    })?;
    if let Some(size) = disk_size {
        let storage = profile_object
            .entry("storage")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| {
                machineemu_core::engine::Error::Invalid("profile.storage must be a mapping".into())
            })?;
        let disk = storage
            .entry("disk")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| {
                machineemu_core::engine::Error::Invalid(
                    "profile.storage.disk must be a mapping".into(),
                )
            })?;
        disk.insert("size".into(), serde_json::Value::String(size.to_owned()));
    }
    if net != "profile" {
        let network = match net.strip_prefix("bridge:") {
            Some(bridge) => serde_json::json!({"type":"bridge", "bridge":bridge}),
            None if net == "bridge" => serde_json::json!({"type":"bridge", "bridge":"br0"}),
            None if net == "user" => serde_json::json!({"type":"user"}),
            None if net == "none" => serde_json::json!({"type":"disabled"}),
            _ => {
                return Err(machineemu_core::engine::Error::Invalid(
                    "--net must be profile, user, none, or bridge[:BRIDGE]".into(),
                ));
            }
        };
        profile_object.insert("network".into(), network);
    }
    if h264 {
        let devices = profile_object
            .entry("devices")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| {
                machineemu_core::engine::Error::Invalid("profile.devices must be a mapping".into())
            })?;
        devices.insert("vnc".into(), serde_json::json!(false));
        devices.insert("h264".into(), serde_json::json!(true));
        devices.insert("video".into(), serde_json::json!({"type": "virtio-vga-gl"}));
        devices.insert("usb_tablet".into(), serde_json::json!(true));
    }
    if !h264
        && !matches!(vnc, "profile" | "off" | "none")
        && let Some(devices) = profile_object
            .get_mut("devices")
            .and_then(serde_json::Value::as_object_mut)
    {
        devices.insert("h264".into(), serde_json::json!(false));
        if devices
            .get("video")
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str())
            .is_some_and(|v| matches!(v, "virtio-gpu-gl" | "virtio-vga-gl"))
        {
            devices.remove("video");
        }
    }
    let profile_id = profile_object
        .get("id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| machineemu_core::engine::Error::Invalid("profile.id is required".into()))?
        .to_owned();
    let image_id = profile_object
        .get("image")
        .and_then(|v| v.as_str())
        .unwrap_or(&profile_id)
        .to_owned();
    let track = profile_object
        .get("engine")
        .and_then(|value| value.get("track"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            machineemu_core::engine::Error::Invalid("profile.engine.track is required".into())
        })?
        .to_owned();
    let profile_target = profile_object
        .get("target")
        .and_then(|value| value.as_str())
        .unwrap_or("x86_64-softmmu")
        .to_owned();
    let saved_profile = profile.clone();
    let sidecars = planned_helpers(
        &saved_profile,
        &workspace_root,
        instance,
        config_path.as_deref(),
        config.helpers.as_ref(),
    )?;
    let display = super::vnc::resolve(
        &profile,
        &profile_path,
        if h264 { "none" } else { vnc },
        vnc_password_file,
    )?;
    validate_h264_display(h264, display.is_some())?;
    super::vnc::normalize_profile(&mut profile, display.is_some())?;
    let configured_engine = if qemu == Path::new("/run/current-system/sw/bin/qemu-system-x86_64") {
        config
            .engines
            .get(&track)
            .filter(|engine| {
                engine
                    .target
                    .as_deref()
                    .is_none_or(|target| target == profile_target)
            })
            .or_else(|| config.engines.get("qemu-system"))
    } else {
        None
    };
    let qemu = configured_engine
        .map(|engine| {
            let configured = machineemu_core::config::resolve_config_path(
                config_path.as_deref(),
                engine.path.clone(),
            );
            if configured.is_dir() {
                let target = engine.target.as_deref().unwrap_or(&profile_target);
                let arch = target.strip_suffix("-softmmu").unwrap_or(target);
                configured.join(format!("qemu-system-{arch}"))
            } else {
                configured
            }
        })
        .unwrap_or_else(|| qemu.to_owned());
    let configured_build_digest = configured_engine.and_then(|engine| {
        engine
            .build_digest
            .as_deref()
            .map(|digest| digest.strip_prefix("sha256:").unwrap_or(digest).to_owned())
    });
    if !qemu.is_file() {
        return Err(machineemu_core::engine::Error::Invalid(format!(
            "QEMU executable is unavailable: {}",
            qemu.display()
        )));
    }
    fs::create_dir_all(workspace_root.join("staging"))
        .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
    let engine_root = workspace_root.join("generated-engines").join(&track);
    fs::create_dir_all(engine_root.join("bin"))
        .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
    let target_arch = profile_target
        .strip_suffix("-softmmu")
        .unwrap_or(&profile_target);
    let engine_file = format!("qemu-system-{target_arch}");
    let engine_link = engine_root.join("bin").join(&engine_file);
    if !engine_link.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&qemu, &engine_link).map_err(|error| {
            machineemu_core::engine::Error::Invalid(format!("cannot link QEMU executable: {error}"))
        })?;
        #[cfg(not(unix))]
        fs::copy(&qemu, &engine_link).map_err(|error| {
            machineemu_core::engine::Error::Invalid(format!("cannot copy QEMU executable: {error}"))
        })?;
    }
    let build_digest = configured_build_digest.unwrap_or_else(|| "0".repeat(64));
    fs::write(
        engine_root.join("engine-build.json"),
        serde_json::json!({
            "schema_version": 1,
            "track_id": track,
            "build_digest": build_digest,
            "source_revision": "host-selected",
            "targets": [profile_target],
            "executables": {profile_target.clone(): format!("bin/{engine_file}")},
            "dirty_source": false
        })
        .to_string(),
    )
    .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
    let release_set = serde_json::json!({
        "schema_version": 1,
        "engines": {track.clone(): {"manifest": format!("{track}/engine-build.json"), "build_digest": build_digest}}
    });
    let mut plan = build_plan(PlanInput {
        profile: profile.clone(),
        release_set,
        bundle_root: workspace_root.join("generated-engines"),
        asset_root: None,
        target: profile_target,
        runtime_dir: instance_dir.clone(),
        state_dir: Some(instance_dir.clone()),
        seed: seed.map(Path::to_owned),
        swtpm: None,
        bridge_helper: None,
        mac: mac.map(str::to_owned),
        instance: Some(instance.to_owned()),
    })?;
    if let Some(display) = &display {
        super::vnc::apply(&mut plan.argv, display)?;
    }
    // Keep the QMP pathname below Linux's 108-byte Unix socket limit even
    // when the workspace or instance path is long. The daemon accepts paths
    // relative to its workspace root and creates the parent directory.
    let qmp_name = format!("{:x}", Sha256::digest(instance.as_bytes()));
    let qmp_socket = workspace_root
        .join("s")
        .join(format!("{}.qmp", &qmp_name[..16]));
    let qmp_arg = format!("unix:{},server=on,wait=off", qmp_socket.display());
    let qmp_index = plan
        .argv
        .iter()
        .position(|part| part == "-qmp")
        .ok_or_else(|| {
            machineemu_core::engine::Error::Invalid("launch plan has no QMP socket".into())
        })?;
    plan.argv[qmp_index + 1] = qmp_arg;
    let default_external_qmp = workspace_root
        .join("s")
        .join(format!("{}.relay.qmp", &qmp_name[..16]));
    if external_qmp_socket.is_none() {
        fs::create_dir_all(workspace_root.join("s")).map_err(|error| {
            machineemu_core::engine::Error::Invalid(format!(
                "cannot create QMP socket directory: {error}"
            ))
        })?;
    }
    let external_qmp_socket = prepare_external_qmp_socket(
        external_qmp_socket.unwrap_or(&default_external_qmp),
        &qmp_socket,
        external_qmp_socket.is_none(),
    )?;
    plan.argv.extend([
        "-qmp".into(),
        format!("unix:{},server=on,wait=off", external_qmp_socket.display()),
    ]);
    let options = inspect_qemu(
        &plan.executable,
        Some(profile["machine"].as_str().unwrap_or("")),
        None,
    )?;
    validate_profile_against_qemu(&profile, &options)?;
    let mut launch_plan = LaunchSpec::from_plan(
        plan,
        LaunchContext {
            workspace: &workspace_root,
            qmp_socket: &qmp_socket,
            stdout: Some(&instance_dir.join("qemu.stdout")),
            stderr: Some(&instance_dir.join("qemu.stderr")),
            tpm_seed: tpm_seed.as_deref(),
            vnc_auto: display.as_ref().is_some_and(|display| display.auto),
            helpers: sidecars,
        },
    )?;
    hardware.apply(&mut launch_plan, &workspace_root, instance)?;
    if fresh && exists {
        let _ = daemon_request(
            &daemon,
            &token,
            "POST",
            &format!("/api/v2/instances/{instance}/stop"),
            None,
        )
        .await;
        daemon_request(
            &daemon,
            &token,
            "DELETE",
            &format!("/api/v2/instances/{instance}"),
            None,
        )
        .await?;
    }
    if fresh || !exists {
        daemon_request(
            &daemon,
            &token,
            "POST",
            "/api/v2/instances",
            Some(serde_json::json!({"instance_id":instance,"image_id":image_id,"profile_id":profile_id,"launch_plan":launch_plan.clone(),"auto_remove":auto_remove,"profile":saved_profile})),
        ).await?;
    } else {
        let mut document = daemon_request(
            &daemon,
            &token,
            "GET",
            &format!("/api/v2/instances/{instance}/config"),
            None,
        )
        .await?;
        document["launch_plan"] = serde_json::to_value(launch_plan.clone()).unwrap();
        document["profile"] = saved_profile;
        if auto_remove {
            document["auto_remove"] = serde_json::Value::Bool(true);
        }
        daemon_request(
            &daemon,
            &token,
            "PUT",
            &format!("/api/v2/instances/{instance}/config"),
            Some(document),
        )
        .await?;
    }
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    );
    if mode == LaunchMode::Create {
        println!("created {instance} using profile {profile_id}");
        return Ok(());
    }
    let started = daemon_request(
        &daemon,
        &token,
        "POST",
        &format!("/api/v2/instances/{instance}/start"),
        Some(serde_json::json!({
            "operation_id": format!("start-{suffix}"),
            "run_id": format!("run-{suffix}"),
            "idempotency_key": format!("run-{suffix}")
        })),
    )
    .await;
    if started.is_err() && auto_remove {
        let _ = daemon_request(
            &daemon,
            &token,
            "DELETE",
            &format!("/api/v2/instances/{instance}"),
            None,
        )
        .await;
    }
    let started = started?;
    println!("started {instance} using profile {profile_id}");
    if let Some(display) = display {
        let port = started["vnc_port"]
            .as_u64()
            .unwrap_or(u64::from(display.port));
        println!("VNC: 127.0.0.1:{port}");
    }
    println!("QMP: {}", external_qmp_socket.display());
    Ok(())
}

fn validate_h264_display(h264: bool, vnc: bool) -> Result<(), machineemu_core::engine::Error> {
    if h264 && vnc {
        return Err(machineemu_core::engine::Error::Invalid(
            "VNC conflicts with GL video selected by --h264".into(),
        ));
    }
    Ok(())
}

fn planned_helpers(
    profile: &serde_json::Value,
    root: &Path,
    instance: &str,
    config_path: Option<&Path>,
    configured: Option<&machineemu_core::config::HelperConfig>,
) -> Result<Vec<HelperSpec>, machineemu_core::engine::Error> {
    use machineemu_core::engine::Error;
    let runtime = root.join("instances").join(instance);
    let relative = format!("instances/{instance}");
    let script = |name: &str, override_path: Option<&PathBuf>| -> Result<String, Error> {
        let path = override_path
            .map(|path| machineemu_core::config::resolve_config_path(config_path, path.clone()))
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../scripts/compat")
                    .join(name)
            });
        path.canonicalize()
            .ok()
            .filter(|path| path.is_file())
            .map(|path| path.to_string_lossy().into_owned())
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "helper script {name} is unavailable at {}",
                    path.display()
                ))
            })
    };
    let mut helpers = Vec::new();
    if profile["machine"] == "udm-pro" || profile["machine"] == "us24pro" {
        let hub = script(
            "unifi_helper.py",
            configured.and_then(|c| c.unifi_hub.as_ref()),
        )?;
        helpers.push(HelperSpec {
            name: "frontpanel".into(),
            argv: vec![
                "python3".into(),
                hub,
                "frontpanel".into(),
                "--runtime".into(),
                runtime.to_string_lossy().into_owned(),
                "--profile".into(),
                runtime.join("profile.json").to_string_lossy().into_owned(),
            ],
            after_qemu: true,
            ready_socket: Some(format!("{relative}/frontpanel.sock").into()),
        });
        if profile["machine"] == "udm-pro"
            && profile.pointer("/devices/lcd") == Some(&serde_json::Value::Bool(true))
        {
            let hub = script(
                "unifi_helper.py",
                configured.and_then(|c| c.unifi_hub.as_ref()),
            )?;
            helpers.push(HelperSpec {
                name: "lcm".into(),
                argv: vec![
                    "python3".into(),
                    hub,
                    "lcm".into(),
                    "--runtime".into(),
                    runtime.to_string_lossy().into_owned(),
                ],
                after_qemu: true,
                ready_socket: Some(format!("{relative}/display-input.sock").into()),
            });
        }
        if profile["machine"] == "udm-pro"
            && profile.pointer("/devices/bluetooth") == Some(&serde_json::Value::Bool(true))
        {
            let bluetooth = script(
                "hci_simulator.py",
                configured.and_then(|c| c.bluetooth_simulator.as_ref()),
            )?;
            helpers.push(HelperSpec {
                name: "bluetooth".into(),
                argv: vec![
                    "python3".into(),
                    bluetooth,
                    "--socket".into(),
                    runtime
                        .join("bluetooth.sock")
                        .to_string_lossy()
                        .into_owned(),
                    "--control".into(),
                    runtime
                        .join("bluetooth-control.sock")
                        .to_string_lossy()
                        .into_owned(),
                ],
                after_qemu: true,
                ready_socket: Some(format!("{relative}/bluetooth-control.sock").into()),
            });
        }
    }
    if profile.pointer("/wifi/enabled") == Some(&serde_json::Value::Bool(true)) {
        if profile["machine"] != "mt7981" {
            return Err(Error::Invalid(
                "Wi-Fi simulation requires the mt7981 machine".into(),
            ));
        }
        let namespace = profile
            .pointer("/wifi/namespace")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                Error::Invalid("wifi.namespace is required for the isolated hwsim helper".into())
            })?;
        Id::new("namespace", namespace.to_owned()).map_err(|e| Error::Invalid(e.to_string()))?;
        let radios = if let Some(radios) = profile
            .pointer("/wifi/radios")
            .and_then(serde_json::Value::as_array)
        {
            radios
                .iter()
                .map(|radio| radio.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        } else {
            profile
                .pointer("/wifi/radio")
                .and_then(serde_json::Value::as_str)
                .map(|radio| vec![radio.to_owned()])
        }
        .filter(|radios| !radios.is_empty())
        .ok_or_else(|| {
            Error::Invalid("wifi.radios must list NAME=HWSIM_RADIO_MAC entries".into())
        })?;
        let wifi = script(
            "hwsim_adapter.py",
            configured.and_then(|c| c.wifi_simulator.as_ref()),
        )?;
        let mut argv = vec![
            "ip".into(),
            "netns".into(),
            "exec".into(),
            namespace.into(),
            "python3".into(),
            wifi,
            "--own-medium".into(),
            "--socket".into(),
            runtime.join("wifi.sock").to_string_lossy().into_owned(),
            "--control".into(),
            runtime
                .join("wifi-control.sock")
                .to_string_lossy()
                .into_owned(),
        ];
        for radio in radios {
            argv.extend(["--radio".into(), radio]);
        }
        helpers.push(HelperSpec {
            name: "wifi".into(),
            argv,
            after_qemu: false,
            ready_socket: Some(format!("{relative}/wifi.sock").into()),
        });
    }
    Ok(helpers)
}

fn prepare_external_qmp_socket(
    requested: &Path,
    internal: &Path,
    generated: bool,
) -> Result<PathBuf, machineemu_core::engine::Error> {
    #[cfg(unix)]
    use std::os::unix::fs::FileTypeExt;
    let invalid = |message: String| machineemu_core::engine::Error::Invalid(message);
    let name = requested
        .file_name()
        .ok_or_else(|| invalid("--qmp-socket must name a socket file".into()))?;
    let parent = requested
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .map_err(|error| invalid(format!("cannot resolve --qmp-socket directory: {error}")))?;
    let path = parent.join(name);
    if path == internal {
        return Err(invalid(
            "--qmp-socket must differ from MachineEmu's internal QMP socket".into(),
        ));
    }
    if path.to_string_lossy().contains(',') {
        return Err(invalid("--qmp-socket path cannot contain a comma".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if path.as_os_str().as_bytes().len() > 103 {
            return Err(invalid(
                "--qmp-socket path is too long for a Unix socket (maximum 103 bytes)".into(),
            ));
        }
    }
    match fs::symlink_metadata(&path) {
        #[cfg(unix)]
        Ok(metadata) if generated && metadata.file_type().is_socket() => {
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                return Err(invalid(format!(
                    "QMP relay socket is already active: {}",
                    path.display()
                )));
            }
            fs::remove_file(&path).map_err(|error| {
                invalid(format!("cannot remove stale QMP relay socket: {error}"))
            })?;
        }
        Ok(_) => {
            return Err(invalid(format!(
                "QMP socket path already exists: {}",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(invalid(format!("cannot check QMP socket path: {error}"))),
    }
    Ok(path)
}

fn bind_image(
    profile: &mut serde_json::Value,
    image: &machineemu_core::domain::ImageManifest,
    workspace: &Path,
    force: bool,
) -> Result<Option<PathBuf>, machineemu_core::engine::Error> {
    use machineemu_core::engine::Error;
    let invalid = |message: String| Error::Invalid(message);
    let target = profile["target"].as_str().unwrap_or("x86_64-softmmu");
    if image.target != target {
        return Err(invalid(format!(
            "image target {} does not match profile target {target}",
            image.target
        )));
    }
    let track = profile
        .pointer("/engine/track")
        .and_then(|value| value.as_str())
        .ok_or_else(|| invalid("profile.engine.track is required".into()))?;
    if track != image.engine_track.as_str()
        && !image
            .supported_engine_tracks
            .iter()
            .any(|supported| supported.as_str() == track)
    {
        if !force {
            return Err(invalid(format!(
                "image {:?} does not declare support for engine track {track:?}; use --force to override compatibility for this launch",
                image.image_id.as_str()
            )));
        }
        eprintln!(
            "warning: forcing image {} with undeclared engine track {track}",
            image.image_id.as_str()
        );
    }
    let disk = profile
        .pointer("/storage/disk/asset")
        .and_then(|v| v.as_str())
        .ok_or_else(|| invalid("--image requires profile.storage.disk.asset".into()))?
        .to_owned();
    let nvram = profile
        .pointer("/firmware/nvram/asset")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let loader = profile
        .pointer("/firmware/loader/asset")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let has_tpm = profile.get("tpm").is_some_and(|value| !value.is_null());
    let component = |digest: &str, name: &str| -> Result<PathBuf, Error> {
        let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid(
                "selected image contains an invalid SHA-256 digest".into(),
            ));
        }
        let path = workspace
            .join("images")
            .join(image.image_id.as_str())
            .join("components")
            .join(name);
        if !path.is_file() {
            return Err(invalid(format!(
                "selected image asset is unavailable: {}",
                path.display()
            )));
        }
        Ok(path)
    };
    let disk_path = component(&image.disk_sha256, "disk.qcow2")?;
    let nvram_path = if nvram.is_some() {
        Some(component(
            image.firmware_sha256.as_deref().ok_or_else(|| {
                invalid("selected image has no NVRAM seed required by this profile".into())
            })?,
            "firmware.fd",
        )?)
    } else {
        None
    };
    let tpm_seed = if has_tpm {
        image
            .tpm_state_sha256
            .as_deref()
            .map(|digest| component(digest, "tpm-state"))
            .transpose()?
    } else {
        None
    };
    let object = profile
        .as_object_mut()
        .ok_or_else(|| invalid("profile must be a mapping".into()))?;
    let assets = object
        .entry("assets")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| invalid("profile.assets must be a mapping".into()))?;
    if let Some(loader) = loader
        && !assets.contains_key(&loader)
    {
        return Err(invalid(format!(
            "profile.assets.{loader} is required: --image supplies disk and NVRAM, but firmware code must be imported and bound in the profile"
        )));
    }
    assets.insert(
        disk,
        serde_json::Value::String(disk_path.display().to_string()),
    );
    if let Some((name, path)) = nvram.zip(nvram_path) {
        assets.insert(name, serde_json::Value::String(path.display().to_string()));
    }
    object.insert(
        "image".into(),
        serde_json::Value::String(image.image_id.as_str().to_owned()),
    );
    Ok(tpm_seed)
}

fn resolve_profile_path(profile_name: &str, workspace: &Path) -> PathBuf {
    if Path::new(profile_name).is_file() {
        PathBuf::from(profile_name)
    } else if workspace
        .join("profiles")
        .join(format!("{profile_name}.json"))
        .is_file()
    {
        workspace
            .join("profiles")
            .join(format!("{profile_name}.json"))
    } else {
        PathBuf::from("profiles").join(format!("{profile_name}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udm_helpers_follow_qemu_and_wifi_requires_an_isolated_namespace() {
        let root = std::env::temp_dir();
        let udm = serde_json::json!({"machine":"udm-pro","devices":{"lcd":true,"bluetooth":true}});
        let helpers = planned_helpers(&udm, &root, "lab", None, None).unwrap();
        assert_eq!(
            helpers
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["frontpanel", "lcm", "bluetooth"]
        );
        assert!(helpers.iter().all(|item| item.after_qemu));
        let wifi = serde_json::json!({"machine":"mt7981","wifi":{"enabled":true,"radio":"ap=02:00:00:00:00:01"}});
        assert!(planned_helpers(&wifi, &root, "lab", None, None).is_err());
        let wifi = serde_json::json!({"machine":"mt7981","wifi":{"enabled":true,"namespace":"lab-wifi","radio":"ap=02:00:00:00:00:01"}});
        let helpers = planned_helpers(&wifi, &root, "lab", None, None).unwrap();
        assert_eq!(helpers[0].name, "wifi");
        assert!(!helpers[0].after_qemu);
    }

    #[test]
    fn external_qmp_path_is_separate_and_cannot_replace_an_existing_file() {
        let root = std::env::temp_dir().join(format!(
            "machineemu-external-qmp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let external = root.join("external.sock");
        let internal = root.join("internal.sock");
        assert_eq!(
            prepare_external_qmp_socket(&external, &internal, false).unwrap(),
            external
        );
        assert!(prepare_external_qmp_socket(&internal, &internal, false).is_err());
        fs::write(&external, "occupied").unwrap();
        assert!(prepare_external_qmp_socket(&external, &internal, false).is_err());
        fs::remove_file(external).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn default_qmp_relay_reclaims_a_stale_socket() {
        let root = std::env::temp_dir().join(format!(
            "machineemu-qmp-relay-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let relay = root.join("relay.qmp");
        let internal = root.join("internal.qmp");
        let listener = std::os::unix::net::UnixListener::bind(&relay).unwrap();
        assert!(prepare_external_qmp_socket(&relay, &internal, true).is_err());
        drop(listener);
        assert_eq!(
            prepare_external_qmp_socket(&relay, &internal, true).unwrap(),
            relay
        );
        assert!(!relay.exists());
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn image_override_binds_profile_asset_names_and_preserves_engine() {
        let root =
            std::env::temp_dir().join(format!("machineemu-bind-image-{}", std::process::id()));
        let components = root.join("images/win11-dev/components");
        fs::create_dir_all(&components).unwrap();
        fs::write(components.join("disk.qcow2"), b"disk").unwrap();
        fs::write(components.join("firmware.fd"), b"vars").unwrap();
        fs::write(components.join("tpm-state"), b"tpm").unwrap();
        let image = machineemu_core::domain::ImageManifest {
            image_id: Id::new("image", "win11-dev").unwrap(),
            engine_track: Id::new("track", "qemu-10.2-unifi").unwrap(),
            supported_engine_tracks: Vec::new(),
            target: "x86_64-softmmu".into(),
            disk_sha256: "a".repeat(64),
            firmware_sha256: Some(format!("sha256:{}", "b".repeat(64))),
            tpm_state_sha256: Some("c".repeat(64)),
        };
        let mut profile = serde_json::json!({
            "id": "analysis", "image": "old", "target": "x86_64-softmmu",
            "engine": {"track": "qemu-10.2-analysis"},
            "storage": {"disk": {"asset": "custom-disk"}},
            "firmware": {"loader": {"asset": "code"}, "nvram": {"asset": "vars"}},
            "assets": {"code": "preserved-code", "custom-disk": "old-disk"},
            "tpm": {"model": "tpm-crb"}
        });
        let original = profile.clone();
        assert!(
            bind_image(&mut original.clone(), &image, &root, false)
                .unwrap_err()
                .to_string()
                .contains("--force")
        );
        let mut compatible = image.clone();
        compatible
            .supported_engine_tracks
            .push(Id::new("track", "qemu-10.2-analysis").unwrap());
        assert!(bind_image(&mut original.clone(), &compatible, &root, false).is_ok());
        let tpm = bind_image(&mut profile, &image, &root, true).unwrap();
        assert_eq!(
            tpm.unwrap(),
            root.join("images/win11-dev/components/tpm-state")
        );
        assert_eq!(profile["image"], "win11-dev");
        assert_eq!(profile["engine"], original["engine"]);
        assert_eq!(profile["assets"]["code"], "preserved-code");
        assert_eq!(
            profile["assets"]["custom-disk"],
            root.join("images/win11-dev/components/disk.qcow2")
                .display()
                .to_string()
        );
        assert_eq!(
            profile["assets"]["vars"],
            root.join("images/win11-dev/components/firmware.fd")
                .display()
                .to_string()
        );
        let mut incompatible = image.clone();
        incompatible.target = "aarch64-softmmu".into();
        assert!(
            bind_image(&mut original.clone(), &incompatible, &root, true)
                .unwrap_err()
                .to_string()
                .contains("target")
        );
        incompatible = image.clone();
        incompatible.firmware_sha256 = None;
        assert!(
            bind_image(&mut original.clone(), &incompatible, &root, true)
                .unwrap_err()
                .to_string()
                .contains("NVRAM")
        );
        let mut missing_code = original.clone();
        missing_code["assets"]
            .as_object_mut()
            .unwrap()
            .remove("code");
        assert!(
            bind_image(&mut missing_code, &image, &root, true)
                .unwrap_err()
                .to_string()
                .contains("firmware code")
        );
        incompatible = image.clone();
        incompatible.disk_sha256 = "../invalid".into();
        assert!(bind_image(&mut original.clone(), &incompatible, &root, true).is_err());
        fs::remove_file(root.join("images/win11-dev/components/disk.qcow2")).unwrap();
        assert!(
            bind_image(&mut original.clone(), &image, &root, true)
                .unwrap_err()
                .to_string()
                .contains("unavailable")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_instance_file_does_not_require_a_template() {
        assert!(Cli::try_parse_from(["machineemu", "create", "--file", "instance.yaml"]).is_ok());
        assert!(
            Cli::try_parse_from(["machineemu", "run", "--file", "instance.json", "--rm"]).is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "machineemu",
                "create",
                "--file",
                "instance.yaml",
                "--net",
                "user"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "machineemu",
                "create",
                "--file",
                "instance.yaml",
                "--disk-size",
                "80GiB"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["machineemu", "create"]).is_ok());
    }

    #[test]
    fn run_parser_accepts_image_override() {
        let cli = Cli::try_parse_from([
            "machineemu",
            "run",
            "--profile",
            "malware-analysis-x64",
            "analysis01",
            "--image",
            "win11-dev",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, Command::Run(RunArgs { launch: LaunchArgs { profile: Some(profile), image: Some(image), instance: Some(instance), .. }, .. }) if profile == "malware-analysis-x64" && image == "win11-dev" && instance == "analysis01")
        );
        let forced = Cli::try_parse_from([
            "machineemu",
            "run",
            "--profile",
            "analysis",
            "lab",
            "--image",
            "win11-dev",
            "--force",
        ])
        .unwrap();
        assert!(matches!(
            forced.command,
            Command::Run(RunArgs {
                launch: LaunchArgs { force: true, .. },
                ..
            })
        ));
        assert!(Cli::try_parse_from(["machineemu", "run", "analysis", "lab", "--force"]).is_err());
        let cli = Cli::try_parse_from(["machineemu", "run", "dev01"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Run(RunArgs {
                launch: LaunchArgs { image: None, instance: Some(instance), .. },
                ..
            }) if instance == "dev01"
        ));
        let create =
            Cli::try_parse_from(["machineemu", "create", "--profile", "win11-dev", "dev02"])
                .unwrap();
        assert!(
            matches!(create.command, Command::Create(LaunchArgs { profile: Some(profile), instance: Some(instance), .. }) if profile == "win11-dev" && instance == "dev02")
        );
        let profiled =
            Cli::try_parse_from(["machineemu", "run", "--profile", "debian13", "linux01"]).unwrap();
        assert!(
            matches!(profiled.command, Command::Run(RunArgs { launch: LaunchArgs { profile: Some(profile), instance: Some(instance), .. }, .. }) if profile == "debian13" && instance == "linux01")
        );
        let existing =
            Cli::try_parse_from(["machineemu", "run", "--network0", "bridge:br0", "linux01"])
                .unwrap();
        assert!(
            matches!(existing.command, Command::Run(RunArgs { launch: LaunchArgs { instance: Some(instance), .. }, .. }) if instance == "linux01")
        );
        assert!(Cli::try_parse_from(["machineemu", "run", "debian13", "linux01"]).is_err());
        let create = Cli::try_parse_from([
            "machineemu",
            "create",
            "--profile",
            "win11-dev",
            "dev03",
            "--disk-size",
            "80GiB",
            "--memory",
            "8GiB",
        ])
        .unwrap();
        assert!(
            matches!(create.command, Command::Create(LaunchArgs { disk_size: Some(size), .. }) if size == "80GiB")
        );
        let sized_run = Cli::try_parse_from([
            "machineemu",
            "run",
            "--profile",
            "win11-dev",
            "dev04",
            "--disk-size",
            "80GiB",
            "--memory",
            "8GiB",
            "--fresh",
        ])
        .unwrap();
        assert!(
            matches!(sized_run.command, Command::Run(RunArgs { launch: LaunchArgs { disk_size: Some(size), .. }, fresh: true, .. }) if size == "80GiB")
        );
        let disposable = Cli::try_parse_from([
            "machineemu",
            "run",
            "--profile",
            "win11-dev",
            "temp01",
            "--rm",
        ])
        .unwrap();
        assert!(matches!(
            disposable.command,
            Command::Run(RunArgs { rm: true, .. })
        ));
        assert!(matches!(
            Cli::try_parse_from(["machineemu", "start", "dev02"])
                .unwrap()
                .command,
            Command::Start { .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["machineemu", "restart", "dev02"])
                .unwrap()
                .command,
            Command::Restart { .. }
        ));
    }

    #[test]
    fn named_launch_prefers_imported_profile_and_explicit_path_wins() {
        let root =
            std::env::temp_dir().join(format!("machineemu-profile-path-{}", std::process::id()));
        fs::create_dir_all(root.join("profiles")).unwrap();
        let imported = root.join("profiles/udm-pro-lab.json");
        fs::write(&imported, "{}").unwrap();
        assert_eq!(resolve_profile_path("udm-pro-lab", &root), imported);
        let explicit = root.join("custom.json");
        fs::write(&explicit, "{}").unwrap();
        assert_eq!(
            resolve_profile_path(explicit.to_str().unwrap(), &root),
            explicit
        );
        assert_eq!(
            resolve_profile_path("udm-pro", &root),
            PathBuf::from("profiles/udm-pro.json")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn h264_rejects_a_selected_vnc_display() {
        assert!(
            validate_h264_display(true, true)
                .unwrap_err()
                .to_string()
                .contains("VNC conflicts with GL video")
        );
        assert!(validate_h264_display(true, false).is_ok());
        assert!(validate_h264_display(false, true).is_ok());
    }
}
