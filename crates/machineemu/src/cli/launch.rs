use super::client::ensure_daemon;
use super::*;
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LaunchMode {
    Create,
    Run,
}
pub(super) struct RunOptions<'a> {
    pub profile_name: &'a str,
    pub instance: &'a str,
    pub image: Option<&'a str>,
    pub force: bool,
    pub seed: Option<&'a Path>,
    pub net: &'a str,
    pub vnc: &'a str,
    pub vnc_password_file: Option<&'a Path>,
    pub external_qmp_socket: Option<&'a Path>,
    pub fresh: bool,
    pub auto_remove: bool,
    pub mode: LaunchMode,
    pub workspace_root: &'a Path,
    pub qemu: &'a Path,
    pub swtpm: Option<&'a Path>,
    pub bridge_helper: Option<&'a Path>,
    pub mac: Option<&'a str>,
    pub daemon: &'a str,
    pub token: &'a str,
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
        net,
        vnc,
        vnc_password_file,
        external_qmp_socket,
        fresh,
        auto_remove,
        mode,
        workspace_root,
        qemu,
        swtpm,
        bridge_helper,
        mac,
        daemon,
        token,
    } = options;
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
    if exists && (mode == LaunchMode::Create || auto_remove) {
        return Err(machineemu_core::engine::Error::Invalid(format!(
            "instance {instance} already exists"
        )));
    }
    // The flag wins, then helpers.swtpm from the configuration; a relative
    // configured path is read against the file that declared it, as engine
    // paths are. Without either, the plan names `swtpm` and PATH decides.
    let helper_path = |flag: Option<&Path>, configured: Option<PathBuf>| {
        flag.map(Path::to_owned).or_else(|| {
            configured.map(|path| {
                machineemu_core::config::resolve_config_path(config_path.as_deref(), path)
            })
        })
    };
    let swtpm_path = helper_path(
        swtpm,
        config
            .helpers
            .as_ref()
            .and_then(|helpers| helpers.swtpm.clone()),
    );
    let bridge_helper_path = helper_path(
        bridge_helper,
        config
            .helpers
            .as_ref()
            .and_then(|helpers| helpers.qemu_bridge_helper.clone()),
    );
    let instance_dir = workspace_root.join("instances").join(instance);
    let instance_profile = instance_dir.join("profile.json");
    let profile_path = if exists && !fresh && instance_profile.exists() {
        instance_profile.clone()
    } else {
        resolve_profile_path(profile_name, &workspace_root)
    };
    let mut profile = load_document(&profile_path)?;
    let selected_image = image.or_else(|| {
        (!fresh)
            .then_some(existing.as_ref())
            .flatten()
            .and_then(|value| value["image_id"].as_str())
    });
    let tpm_seed = selected_image
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
    let profile_id = profile_object
        .get("id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| machineemu_core::engine::Error::Invalid("profile.id is required".into()))?
        .to_owned();
    if exists
        && !fresh
        && existing
            .as_ref()
            .and_then(|value| value["profile_id"].as_str())
            != Some(profile_id.as_str())
    {
        return Err(machineemu_core::engine::Error::Invalid(format!(
            "{} has a different profile ID; edit its settings without changing profile.id or use --fresh",
            instance_profile.display()
        )));
    }
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
    let display = super::vnc::resolve(&profile, &profile_path, vnc, vnc_password_file)?;
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
            .or_else(|| config.engines.get("system"))
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
        asset_root: Some(workspace_root.join("blobs")),
        target: profile_target,
        runtime_dir: instance_dir.clone(),
        state_dir: Some(instance_dir.clone()),
        seed: seed.map(Path::to_owned),
        swtpm: swtpm_path,
        bridge_helper: bridge_helper_path,
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
    let relative = |path: &Path| -> Result<String, machineemu_core::engine::Error> {
        path.strip_prefix(&workspace_root)
            .map(|value| value.to_string_lossy().into_owned())
            .map_err(|_| {
                machineemu_core::engine::Error::Invalid(format!(
                    "runtime path is outside workspace: {}",
                    path.display()
                ))
            })
    };
    let preparation = serde_json::json!({
        "disk_backing": plan.preparation.disk_overlay.as_ref().map(|value| relative(&value.backing)).transpose()?,
        "backing_format": plan.preparation.disk_overlay.as_ref().map(|value| value.backing_format.clone()).unwrap_or_else(|| "qcow2".into()),
        "disk_size": plan.preparation.disk_overlay.as_ref().and_then(|value| value.size.clone()),
        "nvram_seed": plan.preparation.nvram.as_ref().map(|value| relative(&value.seed)).transpose()?,
        "tpm_seed": tpm_seed.as_deref().map(relative).transpose()?
    });
    let launch_plan = serde_json::json!({
        "argv": plan.argv,
        "vnc_auto": display.as_ref().is_some_and(|display| display.auto),
        "qmp_socket": relative(&qmp_socket)?,
        "stdout": relative(&instance_dir.join("qemu.stdout"))?,
        "stderr": relative(&instance_dir.join("qemu.stderr"))?,
        "preparation": if plan.preparation.disk_overlay.is_some() { preparation } else { serde_json::Value::Null },
        "helper_argv": plan.helper_argv,
        "helpers": sidecars
    });
    if image.is_some()
        && !fresh
        && let Some(existing) = &existing
    {
        check_instance_image(existing, &image_id)?;
    }
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
    }
    if exists && !fresh && !instance_profile.exists() {
        write_instance_profile(&instance_profile, &saved_profile)?;
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
    if exists && !fresh {
        eprintln!("machineemu: run on an existing instance is deprecated; use start {instance}");
    }
    let started = daemon_request(
        &daemon,
        &token,
        "POST",
        &format!("/api/v2/instances/{instance}/start"),
        Some(serde_json::json!({
            "operation_id": format!("start-{suffix}"),
            "run_id": format!("run-{suffix}"),
            "idempotency_key": format!("run-{suffix}"),
            "launch_plan": if exists && !fresh { Some(launch_plan) } else { None }
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

fn planned_helpers(
    profile: &serde_json::Value,
    root: &Path,
    instance: &str,
    config_path: Option<&Path>,
    configured: Option<&machineemu_core::config::HelperConfig>,
) -> Result<Vec<serde_json::Value>, machineemu_core::engine::Error> {
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
        helpers.push(serde_json::json!({
            "name":"frontpanel", "argv":["python3",hub,"frontpanel","--runtime",runtime,"--profile",runtime.join("profile.json")],
            "after_qemu":true,"ready_socket":format!("{relative}/frontpanel.sock")
        }));
        if profile["machine"] == "udm-pro"
            && profile.pointer("/devices/lcd") == Some(&serde_json::Value::Bool(true))
        {
            let hub = script(
                "unifi_helper.py",
                configured.and_then(|c| c.unifi_hub.as_ref()),
            )?;
            helpers.push(serde_json::json!({
                "name":"lcm", "argv":["python3",hub,"lcm","--runtime",runtime],
                "after_qemu":true,"ready_socket":format!("{relative}/display-input.sock")
            }));
        }
        if profile["machine"] == "udm-pro"
            && profile.pointer("/devices/bluetooth") == Some(&serde_json::Value::Bool(true))
        {
            let bluetooth = script(
                "hci_simulator.py",
                configured.and_then(|c| c.bluetooth_simulator.as_ref()),
            )?;
            helpers.push(serde_json::json!({
                "name":"bluetooth", "argv":["python3",bluetooth,"--socket",runtime.join("bluetooth.sock"),"--control",runtime.join("bluetooth-control.sock")],
                "after_qemu":true,"ready_socket":format!("{relative}/bluetooth-control.sock")
            }));
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
        helpers.push(serde_json::json!({
            "name":"wifi", "argv":argv,
            "after_qemu":false,"ready_socket":format!("{relative}/wifi.sock")
        }));
    }
    Ok(helpers)
}

fn write_instance_profile(
    path: &Path,
    profile: &serde_json::Value,
) -> Result<(), machineemu_core::engine::Error> {
    let bytes = serde_json::to_vec_pretty(profile)
        .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            machineemu_core::engine::Error::Invalid(format!(
                "cannot create instance settings {}: {error}",
                path.display()
            ))
        })?;
    file.write_all(&bytes).map_err(|error| {
        machineemu_core::engine::Error::Invalid(format!(
            "cannot write instance settings {}: {error}",
            path.display()
        ))
    })
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

fn check_instance_image(
    existing: &serde_json::Value,
    image_id: &str,
) -> Result<(), machineemu_core::engine::Error> {
    if existing["image_id"].as_str() != Some(image_id) {
        return Err(machineemu_core::engine::Error::Invalid(format!(
            "instance uses a different image; choose a new instance name or use --fresh to recreate it with {image_id:?}"
        )));
    }
    Ok(())
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
    let blob = |digest: &str| -> Result<(String, PathBuf), Error> {
        let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid(
                "selected image contains an invalid SHA-256 digest".into(),
            ));
        }
        let path = workspace.join("blobs/sha256").join(digest);
        if !path.is_file() {
            return Err(invalid(format!(
                "selected image asset is unavailable: {}",
                path.display()
            )));
        }
        Ok((format!("sha256:{digest}"), path))
    };
    let disk_digest = blob(&image.disk_sha256)?.0;
    let nvram_digest = if nvram.is_some() {
        Some(
            blob(image.firmware_sha256.as_deref().ok_or_else(|| {
                invalid("selected image has no NVRAM seed required by this profile".into())
            })?)?
            .0,
        )
    } else {
        None
    };
    let tpm_seed = if has_tpm {
        image
            .tpm_state_sha256
            .as_deref()
            .map(blob)
            .transpose()?
            .map(|(_, path)| path)
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
    assets.insert(disk, serde_json::Value::String(disk_digest));
    if let Some((name, digest)) = nvram.zip(nvram_digest) {
        assets.insert(name, serde_json::Value::String(digest));
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
        PathBuf::from("catalog/profiles").join(format!("{profile_name}.json"))
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
                .map(|item| item["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["frontpanel", "lcm", "bluetooth"]
        );
        assert!(helpers.iter().all(|item| item["after_qemu"] == true));
        let wifi = serde_json::json!({"machine":"mt7981","wifi":{"enabled":true,"radio":"ap=02:00:00:00:00:01"}});
        assert!(planned_helpers(&wifi, &root, "lab", None, None).is_err());
        let wifi = serde_json::json!({"machine":"mt7981","wifi":{"enabled":true,"namespace":"lab-wifi","radio":"ap=02:00:00:00:00:01"}});
        let helpers = planned_helpers(&wifi, &root, "lab", None, None).unwrap();
        assert_eq!(helpers[0]["name"], "wifi");
        assert_eq!(helpers[0]["after_qemu"], false);
    }

    #[test]
    fn instance_profile_preserves_user_edits() {
        let directory = std::env::temp_dir().join(format!(
            "machineemu-instance-profile-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("profile.json");
        write_instance_profile(
            &path,
            &serde_json::json!({"id":"analysis", "resources":{"memory":"8GiB"}}),
        )
        .unwrap();
        let mut edited = load_document(&path).unwrap();
        edited["resources"]["memory"] = serde_json::json!("12GiB");
        fs::write(&path, serde_json::to_vec_pretty(&edited).unwrap()).unwrap();
        assert!(write_instance_profile(&path, &serde_json::json!({"id":"analysis"})).is_err());
        assert_eq!(
            load_document(&path).unwrap()["resources"]["memory"],
            "12GiB"
        );
        fs::remove_dir_all(directory).unwrap();
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
        fs::create_dir_all(root.join("blobs/sha256")).unwrap();
        for digit in ["a", "b", "c"] {
            fs::write(root.join("blobs/sha256").join(digit.repeat(64)), digit).unwrap();
        }
        let image = machineemu_core::domain::ImageManifest {
            image_id: Id::new("image", "win11-dev").unwrap(),
            engine_track: Id::new("track", "unifi-10.2").unwrap(),
            supported_engine_tracks: Vec::new(),
            target: "x86_64-softmmu".into(),
            disk_sha256: "a".repeat(64),
            firmware_sha256: Some(format!("sha256:{}", "b".repeat(64))),
            tpm_state_sha256: Some("c".repeat(64)),
        };
        let mut profile = serde_json::json!({
            "id": "analysis", "image": "old", "target": "x86_64-softmmu",
            "engine": {"track": "unifi-10.2-analysis"},
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
            .push(Id::new("track", "unifi-10.2-analysis").unwrap());
        assert!(bind_image(&mut original.clone(), &compatible, &root, false).is_ok());
        let tpm = bind_image(&mut profile, &image, &root, true).unwrap();
        assert_eq!(tpm.unwrap(), root.join("blobs/sha256").join("c".repeat(64)));
        assert_eq!(profile["image"], "win11-dev");
        assert_eq!(profile["engine"], original["engine"]);
        assert_eq!(profile["assets"]["code"], "preserved-code");
        assert_eq!(
            profile["assets"]["custom-disk"],
            format!("sha256:{}", "a".repeat(64))
        );
        assert_eq!(
            profile["assets"]["vars"],
            format!("sha256:{}", "b".repeat(64))
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
        fs::remove_file(root.join("blobs/sha256").join("a".repeat(64))).unwrap();
        assert!(
            bind_image(&mut original.clone(), &image, &root, true)
                .unwrap_err()
                .to_string()
                .contains("unavailable")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_instance_cannot_silently_change_image() {
        let existing = serde_json::json!({"image_id": "win11-dev"});
        assert!(check_instance_image(&existing, "win11-dev").is_ok());
        assert!(
            check_instance_image(&existing, "other")
                .unwrap_err()
                .to_string()
                .contains("--fresh")
        );
    }

    #[test]
    fn run_parser_accepts_image_override() {
        let cli = Cli::try_parse_from([
            "machineemu",
            "run",
            "malware-analysis-x64",
            "analysis01",
            "--image",
            "win11-dev",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, Command::Run(RunArgs { launch: LaunchArgs { image: Some(image), .. }, .. }) if image == "win11-dev")
        );
        let forced = Cli::try_parse_from([
            "machineemu",
            "run",
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
        let cli = Cli::try_parse_from(["machineemu", "run", "win11-dev", "dev01"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Run(RunArgs {
                launch: LaunchArgs { image: None, .. },
                ..
            })
        ));
        let create = Cli::try_parse_from(["machineemu", "create", "win11-dev", "dev02"]).unwrap();
        assert!(matches!(create.command, Command::Create(_)));
        let disposable =
            Cli::try_parse_from(["machineemu", "run", "win11-dev", "temp01", "--rm"]).unwrap();
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
            PathBuf::from("catalog/profiles/udm-pro.json")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
