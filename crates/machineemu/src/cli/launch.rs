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
    _workspace_root: &Path,
    daemon: &str,
    token: &str,
    start: bool,
    auto_remove: bool,
) -> Result<(), machineemu_core::engine::Error> {
    let _ = (
        file,
        hardware,
        _workspace_root,
        daemon,
        token,
        start,
        auto_remove,
    );
    Err(machineemu_core::engine::Error::Invalid(
        "legacy instance documents are no longer accepted; run machineemu-migrate and use profile/image intent".into(),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn update_existing_instance_config(
    daemon: &str,
    token: &str,
    _workspace_root: &Path,
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
    if hardware.has_updates() {
        return Err(machineemu_core::engine::Error::Invalid(
            "hardware launch-plan overrides were removed; pass equivalent values through instance resolve overrides or update the profile before creating the instance".into(),
        ));
    }
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
    let default_profile = config.defaults.as_ref().and_then(|defaults| {
        defaults
            .profile
            .as_deref()
            .filter(|profile| !profile.is_empty())
    });
    let profile_name = profile_name.or(default_profile).ok_or_else(|| {
        machineemu_core::engine::Error::Invalid(
            "profile is required to create or refresh an instance; use --profile PROFILE INSTANCE or configure defaults.profile"
                .into(),
        )
    })?;
    let instance_dir = workspace_root.join("instances").join(instance);
    let profile_path = resolve_profile_path(profile_name, &workspace_root);
    let mut profile = normalize_profile_document(load_document(&profile_path)?, &profile_path)?;
    let profile_dir = profile_path
        .parent()
        .map(Path::to_owned)
        .unwrap_or_else(|| PathBuf::from("."));
    let selected_image = image
        .or_else(|| profile.get("image").and_then(serde_json::Value::as_str))
        .map(str::to_owned);
    let resolved_image = selected_image
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
            resolve_image_components(&profile, &selected, &workspace_root, force)
        })
        .transpose()?
        .unwrap_or_default();
    let image_components = resolved_image.components;
    let tpm_seed = resolved_image.tpm_seed;
    let profile_object = profile.as_object_mut().ok_or_else(|| {
        machineemu_core::engine::Error::Invalid("profile must be a mapping".into())
    })?;
    if !profile_object.contains_key("engine")
        && let Some(engine) = config
            .defaults
            .as_ref()
            .and_then(|defaults| defaults.engine.as_deref())
            .filter(|engine| !engine.is_empty())
    {
        profile_object.insert("engine".into(), serde_json::json!([engine]));
    }
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
        devices.insert("console".into(), serde_json::json!({"type": "h264"}));
        devices.insert(
            "video".into(),
            serde_json::json!({"model": "virtio-vga-gl"}),
        );
        devices.insert("usb_tablet".into(), serde_json::json!(true));
    }
    if !h264
        && !matches!(vnc, "profile" | "off" | "none")
        && let Some(devices) = profile_object
            .get_mut("devices")
            .and_then(serde_json::Value::as_object_mut)
    {
        devices.insert("console".into(), serde_json::json!({"type": "vnc"}));
        if devices
            .get("video")
            .and_then(|v| v.get("model"))
            .and_then(|v| v.as_str())
            .is_some_and(|v| matches!(v, "virtio-gpu-gl" | "virtio-vga-gl"))
        {
            devices.remove("video");
        }
    }
    if let Some(image_id) = selected_image.as_deref() {
        apply_image_to_profile(profile_object, image_id, &image_components)?;
    }
    let profile_id = profile_object
        .get("id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| machineemu_core::engine::Error::Invalid("profile.id is required".into()))?
        .to_owned();
    let image_id = effective_image_id(profile_object, selected_image.as_deref(), &profile_id);
    let tracks = profile_engine_tracks(profile_object)?;
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
    let mut available_engines = crate::engine_import::registered(&workspace_root)
        .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
    available_engines.extend(config.engines.clone());
    let (track, configured_engine) =
        if qemu == Path::new("/run/current-system/sw/bin/qemu-system-x86_64") {
            select_configured_engine(&available_engines, &tracks, &profile_target)
        } else {
            (tracks[0].clone(), None)
        };
    let qemu = configured_engine
        .map(|engine| {
            let configured = machineemu_core::config::resolve_config_path(
                config_path.as_deref(),
                engine.path.clone(),
            );
            resolve_engine_executable(
                &configured,
                engine.target.as_deref().unwrap_or(&profile_target),
            )
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
    fs::create_dir_all(&engine_root)
        .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
    let build_digest = configured_build_digest.unwrap_or_else(|| "0".repeat(64));
    fs::write(
        engine_root.join("engine-build.json"),
        serde_json::json!({
            "schema_version": 1,
            "track_id": track,
            "build_digest": build_digest,
            "source_revision": "host-selected",
            "targets": [profile_target],
            "executables": {profile_target.clone(): absolutize(&qemu)},
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
        asset_root: Some(profile_dir),
        image_components,
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
    let _launch_plan = LaunchSpec::from_plan(
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
            Some(serde_json::json!({"instance_id":instance,"image_id":image_id,"profile_id":profile_id,"auto_remove":auto_remove,"profile":saved_profile})),
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

#[derive(Debug, Default)]
struct ResolvedImage {
    components: BTreeMap<String, PathBuf>,
    tpm_seed: Option<PathBuf>,
}

fn profile_engine_tracks(
    profile: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<String>, machineemu_core::engine::Error> {
    let Some(engine) = profile.get("engine") else {
        return Ok(vec!["qemu-system".into()]);
    };
    let tracks = engine.as_array().ok_or_else(|| {
        machineemu_core::engine::Error::Invalid(
            "profile.engine must be an array of engine track names".into(),
        )
    })?;
    if tracks.is_empty() {
        return Err(machineemu_core::engine::Error::Invalid(
            "profile.engine must not be empty".into(),
        ));
    }
    tracks
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .filter(|track| !track.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    machineemu_core::engine::Error::Invalid(format!(
                        "profile.engine[{index}] must be a non-empty string"
                    ))
                })
        })
        .collect()
}

fn select_configured_engine<'a>(
    engines: &'a BTreeMap<String, machineemu_core::config::EngineConfig>,
    tracks: &[String],
    target: &str,
) -> (String, Option<&'a machineemu_core::config::EngineConfig>) {
    for track in tracks {
        if let Some(engine) = engines
            .get(track)
            .filter(|engine| engine.target.as_deref().is_none_or(|value| value == target))
        {
            return (track.clone(), Some(engine));
        }
    }
    (tracks[0].clone(), None)
}

fn resolve_engine_executable(configured: &Path, target: &str) -> PathBuf {
    if !configured.is_dir() {
        return configured.to_owned();
    }
    let arch = target.strip_suffix("-softmmu").unwrap_or(target);
    let executable = format!("qemu-system-{arch}");
    let packaged = configured.join("bin").join(&executable);
    if packaged.is_file() {
        packaged
    } else {
        configured.join(executable)
    }
}

fn resolve_image_components(
    profile: &serde_json::Value,
    image: &machineemu_core::domain::ImageManifest,
    workspace: &Path,
    force: bool,
) -> Result<ResolvedImage, machineemu_core::engine::Error> {
    use machineemu_core::engine::Error;
    let invalid = |message: String| Error::Invalid(message);
    let target = profile["target"].as_str().unwrap_or("x86_64-softmmu");
    if image.target != target {
        return Err(invalid(format!(
            "image target {} does not match profile target {target}",
            image.target
        )));
    }
    let profile = profile
        .as_object()
        .ok_or_else(|| invalid("profile must be a mapping".into()))?;
    let tracks = profile_engine_tracks(profile)?;
    let compatible = tracks.iter().any(|track| {
        track == image.engine_track.as_str()
            || image
                .supported_engine_tracks
                .iter()
                .any(|supported| supported.as_str() == track)
    });
    if !compatible && !force {
        return Err(invalid(format!(
            "image {:?} does not declare support for any profile engine track {:?}; use --force to override compatibility for this launch",
            image.image_id.as_str(),
            tracks
        )));
    }
    if !compatible {
        eprintln!(
            "warning: forcing image {} with undeclared engine tracks {:?}",
            image.image_id.as_str(),
            tracks
        );
    }
    let root = workspace.join("images").join(image.image_id.as_str());
    let mut components = BTreeMap::new();
    for (name, component) in &image.components {
        let path = Path::new(&component.path);
        if path.is_absolute()
            || path
                .components()
                .any(|part| part == std::path::Component::ParentDir)
        {
            return Err(invalid(format!(
                "selected image component {name:?} has an invalid path"
            )));
        }
        let path = root.join(path);
        if !path.is_file() {
            return Err(invalid(format!(
                "selected image component {name:?} is unavailable: {}",
                path.display()
            )));
        }
        components.insert(name.clone(), path);
    }
    if components.is_empty() {
        let component = |digest: &str, name: &str| -> Result<PathBuf, Error> {
            let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
            if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(invalid(
                    "selected image contains an invalid SHA-256 digest".into(),
                ));
            }
            let path = root.join("components").join(name);
            if !path.is_file() {
                return Err(invalid(format!(
                    "selected image component is unavailable: {}",
                    path.display()
                )));
            }
            Ok(path)
        };
        if !image.disk_sha256.is_empty() {
            components.insert("disk".into(), component(&image.disk_sha256, "disk.qcow2")?);
        }
        if let Some(digest) = &image.firmware_sha256 {
            components.insert("firmware".into(), component(digest, "firmware.fd")?);
        }
        if let Some(digest) = &image.tpm_state_sha256 {
            components.insert("tpm_state".into(), component(digest, "tpm-state")?);
        }
    }
    let tpm_seed = if profile.get("tpm").is_some_and(|value| !value.is_null()) {
        components.get("tpm_state").cloned()
    } else {
        None
    };
    Ok(ResolvedImage {
        components,
        tpm_seed,
    })
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
                runtime.join("profile.yaml").to_string_lossy().into_owned(),
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

fn resolve_profile_path(profile_name: &str, workspace: &Path) -> PathBuf {
    if Path::new(profile_name).is_file() {
        PathBuf::from(profile_name)
    } else {
        let profiles = workspace.join("profiles");
        let json = profiles.join(format!("{profile_name}.json"));
        if json.is_file() {
            json
        } else {
            profiles.join(format!("{profile_name}.yaml"))
        }
    }
}

/// Launch planning still consumes the flattened v2 profile shape, while the
/// workspace document API stores profiles as `{ metadata, spec }` documents.
/// Flatten the document at this CLI boundary so migrated YAML profiles remain
/// usable without rewriting them on disk.
fn normalize_profile_document(
    value: serde_json::Value,
    path: &Path,
) -> Result<serde_json::Value, machineemu_core::engine::Error> {
    let Some(spec) = value.get("spec") else {
        return Ok(value);
    };
    let mut profile = spec.as_object().cloned().ok_or_else(|| {
        machineemu_core::engine::Error::Invalid(format!(
            "profile spec must be a mapping: {}",
            path.display()
        ))
    })?;
    let name = value
        .get("metadata")
        .and_then(|metadata| metadata.get("name"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            machineemu_core::engine::Error::Invalid(format!(
                "profile metadata.name is required: {}",
                path.display()
            ))
        })?;
    profile
        .entry("id")
        .or_insert_with(|| serde_json::Value::String(name.to_owned()));
    profile
        .entry("schema_version")
        .or_insert(serde_json::Value::from(2));
    flatten_native_profile_for_legacy_planner(&mut profile);
    Ok(serde_json::Value::Object(profile))
}

/// The local launcher still feeds the legacy plan builder. Convert the native
/// domain vocabulary back at this boundary; the persisted profile remains in
/// the native document form.
fn flatten_native_profile_for_legacy_planner(
    profile: &mut serde_json::Map<String, serde_json::Value>,
) {
    if let Some(machine) = profile
        .get_mut("machine")
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|machine| machine.remove("type"))
    {
        *profile.get_mut("machine").expect("machine was present") = machine;
    }
    if let Some(engine) = profile
        .get_mut("engine")
        .and_then(serde_json::Value::as_object_mut)
    {
        if let Some(tracks) = engine.remove("tracks") {
            profile.insert("engine".into(), tracks);
        } else if let Some(track) = engine.remove("track") {
            profile.insert("engine".into(), serde_json::json!([track]));
        }
    }
    if let Some(resources) = profile
        .get_mut("resources")
        .and_then(serde_json::Value::as_object_mut)
        && let Some(vcpus) = resources
            .get_mut("vcpus")
            .and_then(serde_json::Value::as_object_mut)
        && let Some(count) = vcpus.get("count").cloned()
    {
        resources.insert("vcpus".into(), count);
    }
    let Some(devices) = profile
        .get_mut("devices")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    for (collection, compact, id) in [
        ("graphics", "console", "graphics0"),
        ("video", "video", "video0"),
        ("serial", "serial", "serial0"),
    ] {
        let Some(mut item) = devices
            .remove(collection)
            .and_then(|value| value.as_array().and_then(|items| items.first().cloned()))
            .and_then(|value| value.as_object().cloned())
        else {
            continue;
        };
        item.remove("id");
        if collection == "graphics" && !item.contains_key("type") {
            item.insert("type".into(), serde_json::Value::String("vnc".into()));
        }
        let _ = id;
        devices.insert(compact.into(), serde_json::Value::Object(item));
    }
    if let Some(model) = devices
        .remove("interfaces")
        .and_then(|value| value.as_array().and_then(|items| items.first().cloned()))
        .and_then(|value| value.get("model").cloned())
    {
        devices.insert("nic".into(), model);
    }
}

fn effective_image_id(
    profile: &serde_json::Map<String, serde_json::Value>,
    selected_image: Option<&str>,
    profile_id: &str,
) -> String {
    selected_image
        .or_else(|| profile.get("image").and_then(serde_json::Value::as_str))
        .unwrap_or(profile_id)
        .to_owned()
}

/// Apply the selected image after the profile has established the instance's
/// hardware and policy. Image components replace only their corresponding
/// sources; profile options such as disk bus, size, firmware security, and
/// device selection remain intact.
fn apply_image_to_profile(
    profile: &mut serde_json::Map<String, serde_json::Value>,
    image_id: &str,
    components: &BTreeMap<String, PathBuf>,
) -> Result<(), machineemu_core::engine::Error> {
    use machineemu_core::engine::Error;
    profile.insert("image".into(), serde_json::Value::String(image_id.into()));
    if components.contains_key("disk") {
        let storage = profile
            .entry("storage")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| Error::Invalid("profile.storage must be a mapping".into()))?;
        let disk = storage
            .entry("disk")
            .or_insert_with(|| serde_json::json!({"format":"qcow2","bus":"virtio"}))
            .as_object_mut()
            .ok_or_else(|| Error::Invalid("profile.storage.disk must be a mapping".into()))?;
        disk.insert(
            "source".into(),
            serde_json::json!({"image_component":"disk"}),
        );
        disk.entry("format")
            .or_insert_with(|| serde_json::Value::String("qcow2".into()));
        disk.entry("bus")
            .or_insert_with(|| serde_json::Value::String("virtio".into()));
    }
    if components.contains_key("firmware") {
        let firmware = profile
            .entry("firmware")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| Error::Invalid("profile.firmware must be a mapping".into()))?;
        let nvram = firmware
            .entry("nvram")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| Error::Invalid("profile.firmware.nvram must be a mapping".into()))?;
        nvram.insert(
            "source".into(),
            serde_json::json!({"image_component":"firmware"}),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_image_overrides_profile_image_and_profile_id_fallback() {
        let without_image = serde_json::json!({"id":"default-uefi"});
        let without_image = without_image.as_object().unwrap();
        assert_eq!(
            effective_image_id(without_image, Some("debian13-cloud-init"), "default-uefi"),
            "debian13-cloud-init"
        );
        assert_eq!(
            effective_image_id(without_image, None, "default-uefi"),
            "default-uefi"
        );
        let with_image = serde_json::json!({"id":"profile", "image":"profile-image"});
        assert_eq!(
            effective_image_id(
                with_image.as_object().unwrap(),
                Some("override-image"),
                "profile"
            ),
            "override-image"
        );
        assert_eq!(
            effective_image_id(with_image.as_object().unwrap(), None, "profile"),
            "profile-image"
        );
    }

    #[test]
    fn image_sources_overlay_profile_without_replacing_profile_options() {
        let mut profile = serde_json::json!({
            "id":"default-uefi",
            "resources":{"memory":"4GiB"},
            "devices":{"video":{"model":"virtio-vga"}},
            "storage":{"disk":{"bus":"scsi","size":"20GiB"}},
            "firmware":{"loader":{"secure":false},"nvram":{"name":"vars.fd","source":{"path":"old.fd"}}}
        });
        let components = BTreeMap::from([
            ("disk".into(), PathBuf::from("disk.qcow2")),
            ("firmware".into(), PathBuf::from("firmware.fd")),
        ]);
        apply_image_to_profile(
            profile.as_object_mut().unwrap(),
            "debian13-cloud-init",
            &components,
        )
        .unwrap();
        assert_eq!(profile["image"], "debian13-cloud-init");
        assert_eq!(
            profile["storage"]["disk"]["source"]["image_component"],
            "disk"
        );
        assert_eq!(profile["storage"]["disk"]["bus"], "scsi");
        assert_eq!(profile["storage"]["disk"]["size"], "20GiB");
        assert_eq!(profile["storage"]["disk"]["format"], "qcow2");
        assert_eq!(
            profile["firmware"]["nvram"]["source"]["image_component"],
            "firmware"
        );
        assert_eq!(profile["firmware"]["nvram"]["name"], "vars.fd");
        assert_eq!(profile["resources"]["memory"], "4GiB");
        assert_eq!(profile["devices"]["video"]["model"], "virtio-vga");
    }

    #[test]
    fn engine_directory_prefers_packaged_bin_executable() {
        let root = std::env::temp_dir().join(format!(
            "machineemu-engine-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::write(root.join("qemu-system-x86_64"), "build-dir").unwrap();
        fs::write(root.join("bin/qemu-system-x86_64"), "package").unwrap();
        assert_eq!(
            resolve_engine_executable(&root, "x86_64-softmmu"),
            root.join("bin/qemu-system-x86_64")
        );
        fs::remove_file(root.join("bin/qemu-system-x86_64")).unwrap();
        assert_eq!(
            resolve_engine_executable(&root, "x86_64-softmmu"),
            root.join("qemu-system-x86_64")
        );
        fs::remove_dir_all(root).unwrap();
    }

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
    fn image_override_resolves_components_and_preserves_profile() {
        let root =
            std::env::temp_dir().join(format!("machineemu-bind-image-{}", std::process::id()));
        let components = root.join("images/win11-dev/components");
        fs::create_dir_all(&components).unwrap();
        fs::write(components.join("disk.qcow2"), b"disk").unwrap();
        fs::write(components.join("firmware.fd"), b"vars").unwrap();
        fs::write(components.join("tpm-state"), b"tpm").unwrap();
        let image = machineemu_core::domain::ImageManifest {
            image_id: Id::new("image", "win11-dev").unwrap(),
            engine_track: Id::new("track", "qemu-system").unwrap(),
            supported_engine_tracks: Vec::new(),
            target: "x86_64-softmmu".into(),
            components: [
                (
                    "disk".into(),
                    machineemu_core::domain::ImageBundleComponent {
                        path: "components/disk.qcow2".into(),
                        sha256: format!("sha256:{}", "a".repeat(64)),
                    },
                ),
                (
                    "firmware".into(),
                    machineemu_core::domain::ImageBundleComponent {
                        path: "components/firmware.fd".into(),
                        sha256: format!("sha256:{}", "b".repeat(64)),
                    },
                ),
                (
                    "tpm_state".into(),
                    machineemu_core::domain::ImageBundleComponent {
                        path: "components/tpm-state".into(),
                        sha256: format!("sha256:{}", "c".repeat(64)),
                    },
                ),
            ]
            .into_iter()
            .collect(),
            disk_sha256: "a".repeat(64),
            firmware_sha256: Some(format!("sha256:{}", "b".repeat(64))),
            tpm_state_sha256: Some("c".repeat(64)),
        };
        let profile = serde_json::json!({
            "schema_version": 2,
            "id": "analysis", "image": "old", "target": "x86_64-softmmu",
            "engine": ["qemu-10.2-analysis"],
            "storage": {"disk": {"source": {"image_component": "disk"}}},
            "firmware": {
                "loader": {"source": {"path": "/firmware/code.fd"}},
                "nvram": {"source": {"image_component": "firmware"}}
            },
            "tpm": {"model": "tpm-crb"}
        });
        assert!(
            resolve_image_components(&profile, &image, &root, false)
                .unwrap_err()
                .to_string()
                .contains("--force")
        );
        let mut compatible = image.clone();
        compatible
            .supported_engine_tracks
            .push(Id::new("track", "qemu-10.2-analysis").unwrap());
        assert!(resolve_image_components(&profile, &compatible, &root, false).is_ok());
        let resolved = resolve_image_components(&profile, &image, &root, true).unwrap();
        assert_eq!(
            resolved.tpm_seed.unwrap(),
            root.join("images/win11-dev/components/tpm-state")
        );
        assert_eq!(
            resolved.components["disk"],
            root.join("images/win11-dev/components/disk.qcow2")
        );
        assert_eq!(
            resolved.components["firmware"],
            root.join("images/win11-dev/components/firmware.fd")
        );
        let mut incompatible = image.clone();
        incompatible.target = "aarch64-softmmu".into();
        assert!(
            resolve_image_components(&profile, &incompatible, &root, true)
                .unwrap_err()
                .to_string()
                .contains("target")
        );
        fs::remove_file(root.join("images/win11-dev/components/disk.qcow2")).unwrap();
        assert!(
            resolve_image_components(&profile, &image, &root, true)
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
            Cli::try_parse_from(["machineemu", "run", "--file", "instance.yaml", "--rm"]).is_ok()
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
            root.join("profiles/udm-pro.json")
        );
        let yaml = root.join("profiles/default-uefi.yaml");
        fs::write(&yaml, "api_version: machineemu.io/v1\n").unwrap();
        assert_eq!(resolve_profile_path("default-uefi", &root), yaml);
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
