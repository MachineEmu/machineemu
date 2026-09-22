#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    thread,
    time::Duration,
};

use clap::{Parser, Subcommand};
use machineemu_plan::{
    PlanInput, build_plan, inspect_qemu, load_document, validate_legacy_config,
    validate_profile_against_qemu,
};
use machineemu_runtime::{Id, Workspace};

#[derive(Debug, Parser)]
#[command(name = "machineemu", about = "MachineEmu Rust planning tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Stop an instance through its QMP socket.
    Stop {
        instance: String,
        #[arg(long)]
        force: bool,
        #[arg(long, default_value = "vm-state")]
        state_dir: PathBuf,
        #[arg(long, default_value = "machineemu-workspace")]
        workspace: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        daemon: String,
        #[arg(long, default_value = "machineemu-dev-token")]
        token: String,
    },
    /// Remove a stopped instance state directory.
    Rm {
        instance: String,
        /// Force QEMU to quit before removing the state directory.
        #[arg(long)]
        force: bool,
        #[arg(long, default_value = "vm-state")]
        state_dir: PathBuf,
        #[arg(long, default_value = "machineemu-workspace")]
        workspace: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        daemon: String,
        #[arg(long, default_value = "machineemu-dev-token")]
        token: String,
    },
    /// List instance state and guest-agent IPs when available.
    Ps {
        /// Instance state directory. Defaults to ./vm-state.
        #[arg(long, default_value = "vm-state")]
        state_dir: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        daemon: String,
        #[arg(long, default_value = "machineemu-dev-token")]
        token: String,
    },
    /// Start a named profile with the concise operator interface.
    Run {
        /// Profile/base ID, for example debian13-cloud.
        profile: String,
        /// Instance ID, for example lab01.
        instance: String,
        /// Optional per-instance NoCloud/cloud-init seed ISO.
        #[arg(long)]
        seed: Option<PathBuf>,
        /// Networking mode: user, bridge[:BRIDGE], or none.
        #[arg(long, default_value = "user")]
        net: String,
        /// VNC display selection passed to the compatibility runner.
        #[arg(long, default_value = "auto")]
        vnc: String,
        /// Rebuild the instance state from its base.
        #[arg(long)]
        fresh: bool,
        /// MachineEmu workspace root.
        #[arg(long, default_value = "machineemu-workspace")]
        workspace: PathBuf,
        /// Exact QEMU executable to use for this run.
        #[arg(long, default_value = "/run/current-system/sw/bin/qemu-system-x86_64")]
        qemu: PathBuf,
        /// swtpm executable for profiles with an emulated TPM. Defaults to
        /// helpers.swtpm from the configuration, then `swtpm` on PATH.
        #[arg(long, env = "MACHINEEMU_SWTPM")]
        swtpm: Option<PathBuf>,
        /// Privileged qemu-bridge-helper for bridge networking. Defaults to
        /// helpers.qemu_bridge_helper from the configuration.
        #[arg(long, env = "MACHINEEMU_BRIDGE_HELPER")]
        bridge_helper: Option<PathBuf>,
        /// Daemon HTTP endpoint.
        #[arg(long, default_value = "127.0.0.1:8787")]
        daemon: String,
        #[arg(long, default_value = "machineemu-dev-token")]
        token: String,
    },
    /// Resolve a profile into an executable and argv without changing state.
    Plan {
        /// JSON or YAML profile document.
        #[arg(long)]
        profile: PathBuf,
        /// Release-set JSON/YAML document.
        #[arg(long)]
        release_set: PathBuf,
        /// Installed engine bundle root.
        #[arg(long)]
        bundle_root: PathBuf,
        /// Content-addressed asset root.
        #[arg(long)]
        asset_root: Option<PathBuf>,
        #[arg(long)]
        target: String,
        #[arg(long)]
        runtime_dir: PathBuf,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Optional per-instance NoCloud/cloud-init seed ISO.
        #[arg(long)]
        seed: Option<PathBuf>,
        /// swtpm executable for profiles with an emulated TPM. Defaults to
        /// `swtpm` on PATH.
        #[arg(long, env = "MACHINEEMU_SWTPM")]
        swtpm: Option<PathBuf>,
        /// Privileged qemu-bridge-helper for bridge networking. QEMU otherwise
        /// runs the copy beside its own executable.
        #[arg(long, env = "MACHINEEMU_BRIDGE_HELPER")]
        bridge_helper: Option<PathBuf>,
        /// Emit the machine-readable plan. This is the default and authoritative form.
        #[arg(long)]
        json: bool,
        /// Validate selected machine, CPU, and accelerator against the exact QEMU executable.
        #[arg(long)]
        validate_qemu: bool,
    },
    /// Inspect choices exposed by one exact QEMU executable.
    QemuOptions {
        /// QEMU executable selected by the engine bundle.
        #[arg(long)]
        qemu: PathBuf,
        /// Emit JSON (the default output is also JSON for API stability).
        #[arg(long)]
        json: bool,
        /// Optional machine name whose properties should be queried.
        #[arg(long)]
        machine: Option<String>,
        /// Optional device type whose properties should be queried.
        #[arg(long)]
        device: Option<String>,
    },
    /// Validate an existing unifi-qemu version-1 YAML configuration against QEMU.
    ValidateConfig {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        qemu: PathBuf,
        /// Validate one instance; otherwise validate every board in the document.
        #[arg(long)]
        instance: Option<String>,
    },
    /// Validate a new-format profile against one QEMU executable without resolving assets.
    ValidateProfile {
        #[arg(long)]
        profile: PathBuf,
        #[arg(long)]
        qemu: PathBuf,
    },
    /// Import a vmmanager-sh immutable base into the workspace image store.
    ImportVmmanagerBase {
        /// MachineEmu workspace root.
        #[arg(long)]
        workspace: PathBuf,
        /// vmmanager-sh base directory, for example ~/.vm-base/win11-dev.
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        image_id: String,
        #[arg(long)]
        engine_track: String,
        #[arg(long, default_value = "x86_64-softmmu")]
        target: String,
        /// Also write a portable bundle with named component files.
        #[arg(long)]
        export_bundle: Option<PathBuf>,
    },
    /// Import one host asset into the workspace content-addressed store.
    ImportAsset {
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        source: PathBuf,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("machineemu: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), machineemu_plan::Error> {
    let cli = Cli::parse();
    match cli.command {
        Command::Stop {
            instance,
            force,
            state_dir,
            workspace: _,
            daemon,
            token,
        } => {
            let _ = (force, state_dir);
            daemon_request(
                &daemon,
                &token,
                "POST",
                &format!("/api/v2/instances/{instance}/stop"),
                None,
            )?;
        }
        Command::Rm {
            instance,
            force,
            state_dir,
            workspace: _,
            daemon,
            token,
        } => {
            let _ = state_dir;
            if force {
                let _ = daemon_request(
                    &daemon,
                    &token,
                    "POST",
                    &format!("/api/v2/instances/{instance}/stop"),
                    None,
                );
            }
            daemon_request(
                &daemon,
                &token,
                "DELETE",
                &format!("/api/v2/instances/{instance}"),
                None,
            )?;
            println!("removed {instance}");
        }
        Command::Ps {
            state_dir: _,
            daemon,
            token,
        } => {
            let response = daemon_request(&daemon, &token, "GET", "/api/v2/instances", None)?;
            println!("NAME\tPROFILE\tSTATE\tIP");
            for item in response.as_array().into_iter().flatten() {
                let instance = item.get("instance").unwrap_or(item);
                let name = instance
                    .get("instance_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let profile = instance
                    .get("profile_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let state = instance
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let ip = item.get("ip").and_then(|v| v.as_str()).unwrap_or("unknown");
                println!("{name}\t{profile}\t{state}\t{ip}");
            }
        }
        Command::Run {
            profile,
            instance,
            seed,
            net,
            vnc,
            fresh,
            workspace,
            qemu,
            swtpm,
            bridge_helper,
            daemon,
            token,
        } => {
            run_rust_owned(
                &profile,
                &instance,
                seed.as_deref(),
                &net,
                &vnc,
                fresh,
                &workspace,
                &qemu,
                swtpm.as_deref(),
                bridge_helper.as_deref(),
                &daemon,
                &token,
            )?;
        }
        Command::Plan {
            profile,
            release_set,
            bundle_root,
            asset_root,
            target,
            runtime_dir,
            state_dir,
            seed,
            swtpm,
            bridge_helper,
            json: _,
            validate_qemu,
        } => {
            let input = PlanInput {
                profile: load_document(&profile)?,
                release_set: load_document(&release_set)?,
                bundle_root,
                asset_root,
                target,
                runtime_dir,
                state_dir,
                seed,
                swtpm,
                bridge_helper,
            };
            if validate_qemu {
                let executable = build_plan(input.clone())?.executable;
                let options = inspect_qemu(&executable, None, None)?;
                validate_profile_against_qemu(&input.profile, &options)?;
            }
            let plan = build_plan(input)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&plan).expect("plan is serializable")
            );
        }
        Command::QemuOptions {
            qemu,
            json: _,
            machine,
            device,
        } => {
            let options = inspect_qemu(&qemu, machine.as_deref(), device.as_deref())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&options).expect("options are serializable")
            );
        }
        Command::ValidateConfig {
            config,
            qemu,
            instance,
        } => {
            let document = load_document(&config)?;
            let report = validate_legacy_config(&document, &qemu, instance.as_deref())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("report is serializable")
            );
            if !report.valid {
                std::process::exit(2);
            }
        }
        Command::ValidateProfile { profile, qemu } => {
            let document = load_document(&profile)?;
            let machine = document.get("machine").and_then(serde_json::Value::as_str);
            let options = inspect_qemu(&qemu, machine, None)?;
            validate_profile_against_qemu(&document, &options)?;
            println!("{{\"valid\":true}}");
        }
        Command::ImportVmmanagerBase {
            workspace,
            source,
            image_id,
            engine_track,
            target,
            export_bundle,
        } => {
            let workspace = Workspace::open(workspace)
                .map_err(|error| machineemu_plan::Error::Runtime(error.to_string()))?;
            let image = workspace
                .import_vmmanager_base(
                    source,
                    Id::new("image", image_id)
                        .map_err(|error| machineemu_plan::Error::Runtime(error.to_string()))?,
                    Id::new("engine track", engine_track)
                        .map_err(|error| machineemu_plan::Error::Runtime(error.to_string()))?,
                    target,
                )
                .map_err(|error| machineemu_plan::Error::Runtime(error.to_string()))?;
            if let Some(destination) = export_bundle {
                workspace
                    .export_image_bundle(&image.image_id, &destination)
                    .map_err(|error| machineemu_plan::Error::Runtime(error.to_string()))?;
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&image).expect("image is serializable")
            );
        }
        Command::ImportAsset { workspace, source } => {
            let workspace = Workspace::open(workspace)
                .map_err(|error| machineemu_plan::Error::Runtime(error.to_string()))?;
            let digest = workspace
                .import_file(&source)
                .map_err(|error| machineemu_plan::Error::Runtime(error.to_string()))?;
            println!("sha256:{digest}");
        }
    }
    Ok(())
}

fn run_rust_owned(
    profile_name: &str,
    instance: &str,
    seed: Option<&Path>,
    net: &str,
    _vnc: &str,
    fresh: bool,
    workspace_root: &Path,
    qemu: &Path,
    swtpm: Option<&Path>,
    bridge_helper: Option<&Path>,
    daemon: &str,
    token: &str,
) -> Result<(), machineemu_plan::Error> {
    let (config, config_path) = machineemu_runtime::load_config(None)
        .map_err(|error| machineemu_plan::Error::Runtime(error.to_string()))?;
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
            .map(|path| machineemu_runtime::resolve_config_path(config_path.as_deref(), path))
            .unwrap_or_else(|| workspace_root.to_owned())
    } else {
        workspace_root.to_owned()
    };
    let workspace_root = workspace_path.canonicalize().map_err(|error| {
        machineemu_plan::Error::Invalid(format!("cannot open workspace: {error}"))
    })?;
    // The flag wins, then helpers.swtpm from the configuration; a relative
    // configured path is read against the file that declared it, as engine
    // paths are. Without either, the plan names `swtpm` and PATH decides.
    let helper_path = |flag: Option<&Path>, configured: Option<PathBuf>| {
        flag.map(Path::to_owned).or_else(|| {
            configured
                .map(|path| machineemu_runtime::resolve_config_path(config_path.as_deref(), path))
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
    let profile_path = if Path::new(profile_name).is_file() {
        PathBuf::from(profile_name)
    } else {
        PathBuf::from("catalog/profiles").join(format!("{profile_name}.json"))
    };
    let mut profile = load_document(&profile_path)?;
    let profile_object = profile
        .as_object_mut()
        .ok_or_else(|| machineemu_plan::Error::Invalid("profile must be a mapping".into()))?;
    let network = match net.strip_prefix("bridge:") {
        Some(bridge) => serde_json::json!({"type":"bridge", "bridge":bridge}),
        None if net == "bridge" => serde_json::json!({"type":"bridge", "bridge":"br0"}),
        None if net == "user" => serde_json::json!({"type":"user"}),
        None if net == "none" => serde_json::json!({"type":"disabled"}),
        _ => {
            return Err(machineemu_plan::Error::Invalid(
                "--net must be user, none, or bridge[:BRIDGE]".into(),
            ));
        }
    };
    profile_object.insert("network".into(), network);
    let profile_id = profile_object
        .get("id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| machineemu_plan::Error::Invalid("profile.id is required".into()))?
        .to_owned();
    let track = profile_object
        .get("engine")
        .and_then(|value| value.get("track"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| machineemu_plan::Error::Invalid("profile.engine.track is required".into()))?
        .to_owned();
    let profile_target = profile_object
        .get("target")
        .and_then(|value| value.as_str())
        .unwrap_or("x86_64-softmmu")
        .to_owned();
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
            let configured = machineemu_runtime::resolve_config_path(
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
        return Err(machineemu_plan::Error::Invalid(format!(
            "QEMU executable is unavailable: {}",
            qemu.display()
        )));
    }
    fs::create_dir_all(workspace_root.join("staging"))
        .map_err(|error| machineemu_plan::Error::Invalid(error.to_string()))?;
    let engine_root = workspace_root.join("generated-engines").join(&track);
    fs::create_dir_all(engine_root.join("bin"))
        .map_err(|error| machineemu_plan::Error::Invalid(error.to_string()))?;
    let target_arch = profile_target
        .strip_suffix("-softmmu")
        .unwrap_or(&profile_target);
    let engine_file = format!("qemu-system-{target_arch}");
    let engine_link = engine_root.join("bin").join(&engine_file);
    if !engine_link.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&qemu, &engine_link).map_err(|error| {
            machineemu_plan::Error::Invalid(format!("cannot link QEMU executable: {error}"))
        })?;
        #[cfg(not(unix))]
        fs::copy(&qemu, &engine_link).map_err(|error| {
            machineemu_plan::Error::Invalid(format!("cannot copy QEMU executable: {error}"))
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
    .map_err(|error| machineemu_plan::Error::Invalid(error.to_string()))?;
    let release_set = serde_json::json!({
        "schema_version": 1,
        "engines": {track.clone(): {"manifest": format!("{track}/engine-build.json"), "build_digest": build_digest}}
    });
    let instance_dir = workspace_root.join("instances").join(instance);
    fs::create_dir_all(&instance_dir)
        .map_err(|error| machineemu_plan::Error::Invalid(error.to_string()))?;
    let plan = build_plan(PlanInput {
        profile: profile.clone(),
        release_set,
        bundle_root: workspace_root.join("generated-engines"),
        asset_root: Some(workspace_root.join("blobs")),
        target: profile_target.into(),
        runtime_dir: instance_dir.clone(),
        state_dir: Some(instance_dir.clone()),
        seed: seed.map(Path::to_owned),
        swtpm: swtpm_path,
        bridge_helper: bridge_helper_path,
    })?;
    let options = inspect_qemu(&plan.executable, None, None)?;
    validate_profile_against_qemu(&profile, &options)?;
    let relative = |path: &Path| -> Result<String, machineemu_plan::Error> {
        path.strip_prefix(&workspace_root)
            .map(|value| value.to_string_lossy().into_owned())
            .map_err(|_| {
                machineemu_plan::Error::Invalid(format!(
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
        "tpm_seed": serde_json::Value::Null
    });
    let launch_plan = serde_json::json!({
        "argv": plan.argv,
        "qmp_socket": relative(&instance_dir.join("sockets/qmp.sock"))?,
        "stdout": relative(&instance_dir.join("qemu.stdout"))?,
        "stderr": relative(&instance_dir.join("qemu.stderr"))?,
        "preparation": preparation,
        "helper_argv": plan.helper_argv
    });
    let (daemon, token) = effective_client(daemon, token)?;
    ensure_daemon(&daemon, &token, &workspace_root)?;
    let exists = daemon_request(
        &daemon,
        &token,
        "GET",
        &format!("/api/v2/instances/{instance}"),
        None,
    )
    .is_ok();
    if fresh && exists {
        let _ = daemon_request(
            &daemon,
            &token,
            "POST",
            &format!("/api/v2/instances/{instance}/stop"),
            None,
        );
        daemon_request(
            &daemon,
            &token,
            "DELETE",
            &format!("/api/v2/instances/{instance}"),
            None,
        )?;
    }
    if fresh || !exists {
        daemon_request(
            &daemon,
            &token,
            "POST",
            "/api/v2/instances",
            Some(
                serde_json::json!({"instance_id":instance,"image_id":profile_id,"profile_id":profile_id}),
            ),
        )?;
    }
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    );
    daemon_request(
        &daemon,
        &token,
        "POST",
        &format!("/api/v2/instances/{instance}/start"),
        Some(serde_json::json!({
            "operation_id": format!("start-{suffix}"),
            "run_id": format!("run-{suffix}"),
            "idempotency_key": format!("run-{suffix}"),
            "launch_plan": launch_plan
        })),
    )?;
    println!("started {instance} using profile {profile_id}");
    Ok(())
}

fn ensure_daemon(
    endpoint: &str,
    token: &str,
    workspace: &Path,
) -> Result<(), machineemu_plan::Error> {
    if daemon_request(endpoint, token, "GET", "/api/v2/health", None).is_ok() {
        return Ok(());
    }
    let launch_plans = workspace.join("staging/launch-plans.json");
    fs::write(&launch_plans, "{}")
        .map_err(|error| machineemu_plan::Error::Invalid(error.to_string()))?;
    let executable = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join("machineemu-daemon")))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("machineemu-daemon"));
    #[cfg(unix)]
    let mut command = {
        let mut command = ProcessCommand::new("setsid");
        command.arg("-f").arg(&executable);
        command
    };
    #[cfg(not(unix))]
    let mut command = ProcessCommand::new(&executable);
    let mut args = vec![
        "--workspace".to_owned(),
        workspace.to_string_lossy().into_owned(),
    ];
    if let Some(socket) = endpoint.strip_prefix("unix:") {
        args.extend(["--unix-socket".into(), socket.into()]);
    } else {
        args.extend(["--listen".into(), endpoint.into()]);
        args.extend(["--bearer-token".into(), token.into()]);
    }
    args.extend([
        "--launch-plans".into(),
        launch_plans.to_string_lossy().into_owned(),
    ]);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| {
            machineemu_plan::Error::Invalid(format!("cannot start daemon: {error}"))
        })?;
    for _ in 0..50 {
        thread::sleep(Duration::from_millis(100));
        if daemon_request(endpoint, token, "GET", "/api/v2/health", None).is_ok() {
            return Ok(());
        }
    }
    Err(machineemu_plan::Error::Invalid(
        "daemon did not become ready".into(),
    ))
}

fn effective_client(
    endpoint: &str,
    token: &str,
) -> Result<(String, String), machineemu_plan::Error> {
    if endpoint != "127.0.0.1:8787" || token != "machineemu-dev-token" {
        return Ok((endpoint.to_owned(), token.to_owned()));
    }
    let (config, config_path) = machineemu_runtime::load_config(None)
        .map_err(|error| machineemu_plan::Error::Runtime(error.to_string()))?;
    let Some(client) = config.client else {
        return Ok((endpoint.to_owned(), token.to_owned()));
    };
    let endpoint = client
        .unix_socket
        .map(|path| {
            format!(
                "unix:{}",
                machineemu_runtime::resolve_config_path(config_path.as_deref(), path).display()
            )
        })
        .or(client.endpoint)
        .unwrap_or_else(|| endpoint.to_owned());
    Ok((endpoint, client.token.unwrap_or_else(|| token.to_owned())))
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

fn connect_daemon(endpoint: &str) -> Result<Box<dyn ReadWrite>, machineemu_plan::Error> {
    if let Some(path) = endpoint.strip_prefix("unix:") {
        #[cfg(unix)]
        return UnixStream::connect(path)
            .map(|stream| Box::new(stream) as Box<dyn ReadWrite>)
            .map_err(|error| {
                machineemu_plan::Error::Invalid(format!("cannot connect to daemon: {error}"))
            });
        #[cfg(not(unix))]
        return Err(machineemu_plan::Error::Invalid(
            "Unix daemon sockets are unavailable on this platform".into(),
        ));
    }
    TcpStream::connect(endpoint)
        .map(|stream| Box::new(stream) as Box<dyn ReadWrite>)
        .map_err(|error| {
            machineemu_plan::Error::Invalid(format!("cannot connect to daemon: {error}"))
        })
}

fn daemon_request(
    endpoint: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, machineemu_plan::Error> {
    let (endpoint, token) = effective_client(endpoint, token)?;
    let mut stream = connect_daemon(&endpoint)?;
    let bytes = body
        .map(|value| serde_json::to_vec(&value).expect("JSON value serializes"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {endpoint}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        bytes.len()
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.write_all(&bytes))
        .map_err(|error| {
            machineemu_plan::Error::Invalid(format!("cannot write daemon request: {error}"))
        })?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|error| {
        machineemu_plan::Error::Invalid(format!("cannot read daemon response: {error}"))
    })?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| machineemu_plan::Error::Invalid("malformed daemon response".into()))?;
    let header = String::from_utf8_lossy(&response[..split]);
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(599);
    let payload = &response[split + 4..];
    let value = if payload.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(payload)
            .unwrap_or_else(|_| serde_json::json!({"raw": String::from_utf8_lossy(payload)}))
    };
    if !(200..300).contains(&status) {
        return Err(machineemu_plan::Error::Invalid(format!(
            "daemon returned HTTP {status}: {value}"
        )));
    }
    Ok(value)
}
