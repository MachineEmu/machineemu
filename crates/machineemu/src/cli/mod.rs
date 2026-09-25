#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    thread,
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand, ValueEnum};
use machineemu_core::engine::{
    PlanInput, build_plan, inspect_qemu, load_document, validate_legacy_config,
    validate_profile_against_qemu,
};
use machineemu_core::{domain::Id, storage::Workspace};

#[derive(Debug, clap::Args)]
struct LaunchArgs {
    /// Creation template (name or file). Existing run can omit this.
    #[arg(long)]
    profile: Option<String>,
    /// Instance ID.
    instance: Option<String>,
    /// Create directly from a complete instance JSON/YAML document.
    #[arg(long, conflicts_with_all = ["profile", "instance", "image", "seed", "disk_size", "qmp_socket", "mac", "net", "qemu"])]
    file: Option<PathBuf>,
    /// Registered image to use for disk and firmware state.
    #[arg(long)]
    image: Option<String>,
    /// Allow an image whose engine track differs from the profile.
    #[arg(long, requires = "image")]
    force: bool,
    /// Per-instance cloud-init seed ISO.
    #[arg(long)]
    seed: Option<PathBuf>,
    /// Disk overlay size for a new VM; existing run requires --fresh.
    #[arg(long)]
    disk_size: Option<String>,
    /// profile, user, bridge[:BRIDGE], or none.
    #[arg(long, default_value = "profile")]
    net: String,
    #[command(flatten)]
    hardware: hardware::HardwareArgs,
    /// External QMP relay socket path.
    #[arg(long)]
    qmp_socket: Option<PathBuf>,
    /// MachineEmu workspace root.
    #[arg(long, default_value = "machineemu-workspace")]
    workspace: PathBuf,
    /// QEMU executable.
    #[arg(long, default_value = "/run/current-system/sw/bin/qemu-system-x86_64")]
    qemu: PathBuf,
    /// Guest NIC MAC address.
    #[arg(long)]
    mac: Option<String>,
    /// Daemon endpoint.
    #[arg(long, default_value = "127.0.0.1:8787")]
    daemon: String,
    /// Daemon bearer token.
    #[arg(long, default_value = "machineemu-dev-token")]
    token: String,
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    #[command(flatten)]
    launch: LaunchArgs,
    /// Recreate an existing instance from a clean overlay before starting.
    #[arg(long)]
    fresh: bool,
    /// Automatically remove the instance after its run ends.
    #[arg(long)]
    rm: bool,
}

impl LaunchArgs {
    fn options(
        &self,
        mode: launch::LaunchMode,
        fresh: bool,
        auto_remove: bool,
    ) -> launch::RunOptions<'_> {
        launch::RunOptions {
            profile_name: self.profile.as_deref(),
            instance: self.instance.as_deref(),
            image: self.image.as_deref(),
            force: self.force,
            seed: self.seed.as_deref(),
            disk_size: self.disk_size.as_deref(),
            net: &self.net,
            vnc: self.hardware.vnc.as_deref().unwrap_or("profile"),
            vnc_password_file: self.hardware.vnc_password_file.as_deref(),
            h264: self.hardware.h264,
            hardware: &self.hardware,
            external_qmp_socket: self.qmp_socket.as_deref(),
            fresh,
            auto_remove,
            mode,
            workspace_root: &self.workspace,
            qemu: &self.qemu,
            mac: self.mac.as_deref(),
            daemon: &self.daemon,
            token: &self.token,
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "machineemu", about = "MachineEmu Rust planning tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Add, remove, change, or inspect devices on a running instance.
    Device {
        #[command(subcommand)]
        command: device::DeviceCommand,
        #[arg(long, default_value = "machineemu-workspace")]
        workspace: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        daemon: String,
        #[arg(long, default_value = "machineemu-dev-token")]
        token: String,
    },
    /// Edit hardware on a stopped instance, preserving its disks and identity.
    Config {
        instance: String,
        #[command(flatten)]
        hardware: hardware::HardwareArgs,
        #[arg(long, default_value = "machineemu-workspace")]
        workspace: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        daemon: String,
        #[arg(long, default_value = "machineemu-dev-token")]
        token: String,
    },
    /// Show an instance config, shared profile, or image manifest as YAML.
    Show {
        #[arg(value_enum)]
        kind: DocumentKind,
        id: String,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = "127.0.0.1:8787")]
        daemon: String,
        #[arg(long, default_value = "machineemu-dev-token")]
        token: String,
    },
    /// Replace an instance config, shared profile, or image manifest from YAML or JSON.
    Update {
        #[arg(value_enum)]
        kind: DocumentKind,
        id: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        daemon: String,
        #[arg(long, default_value = "machineemu-dev-token")]
        token: String,
    },
    /// Export old instance configuration to files and remove legacy SQLite config.
    MigrateInstances {
        #[arg(long, default_value = "machineemu-workspace")]
        workspace: PathBuf,
    },
    /// Migrate legacy SQLite image metadata to editable images/<id>/manifest.json files.
    MigrateImages {
        #[arg(long, default_value = "machineemu-workspace")]
        workspace: PathBuf,
    },
    /// List registered workspace images.
    Images {
        #[arg(long, default_value = "machineemu-workspace")]
        workspace: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// List named profiles, preferring workspace profiles over bundled profiles.
    Profiles {
        #[arg(long, default_value = "machineemu-workspace")]
        workspace: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Show an instance's local console log (or QEMU errors if boot failed).
    Logs {
        instance: String,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
        /// auto, serial, stderr, or stdout.
        #[arg(long, default_value = "auto", value_parser = ["auto", "serial", "stderr", "stdout"])]
        source: String,
    },
    /// Attach to the local UART. Ctrl-] detaches without stopping the guest.
    Serial {
        instance: String,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Stop an instance through the daemon, retaining its writable state.
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
    /// Remove a stopped instance and its owned writable state.
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
    /// Show an instance's processes, sockets, listeners and live hardware.
    Inspect {
        instance: String,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = "127.0.0.1:8787")]
        daemon: String,
        #[arg(long, default_value = "machineemu-dev-token")]
        token: String,
    },
    /// Point the patched kvm_intel at one or more analysis instances (pods).
    AnalysisTarget {
        /// Instances to target. Several require the rebuilt multi-pod module.
        #[arg(required_unless_present = "clear")]
        instances: Vec<String>,
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Add to the current target set instead of replacing it.
        #[arg(long, conflicts_with = "clear")]
        add: bool,
        /// Detach kvm_intel from every instance (writes -1).
        #[arg(long)]
        clear: bool,
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
    /// Create a persistent VM without starting it.
    Create(LaunchArgs),
    /// Create or update a VM and start it.
    Run(RunArgs),
    /// Start a created or stopped VM from its saved configuration.
    Start {
        instance: String,
        #[arg(long, default_value = "127.0.0.1:8787")]
        daemon: String,
        #[arg(long, default_value = "machineemu-dev-token")]
        token: String,
    },
    /// Stop a live VM and start it with a new run ID.
    Restart {
        instance: String,
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
        /// Optional root for legacy sha256: profile assets.
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
        /// NIC address for the planned instance.
        #[arg(long)]
        mac: Option<String>,
        /// Instance this plan is for. Without --mac or a profile address, its
        /// NIC address is derived from this name.
        #[arg(long)]
        instance: Option<String>,
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
        #[arg(long, default_value = "127.0.0.1:8787")]
        daemon: String,
        #[arg(long, default_value = "machineemu-dev-token")]
        token: String,
        /// vmmanager-sh base directory. Defaults to ~/.vm-base/<image-id>.
        #[arg(long)]
        source: Option<PathBuf>,
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
    /// Register an image manifest already present in the workspace.
    RegisterImage {
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DocumentKind {
    Instance,
    Profile,
    Image,
}

impl DocumentKind {
    fn path(self, id: &str) -> String {
        match self {
            Self::Instance => format!("/api/v2/instances/{id}/config"),
            Self::Profile => format!("/api/v2/profiles/{id}"),
            Self::Image => format!("/api/v2/images/{id}"),
        }
    }
}

pub async fn main() {
    if let Err(error) = run().await {
        eprintln!("machineemu: {error}");
        std::process::exit(2);
    }
}

async fn run() -> Result<(), machineemu_core::engine::Error> {
    let cli = Cli::parse();
    match cli.command {
        Command::Device {
            command,
            workspace,
            daemon,
            token,
        } => device::run(command, &workspace, &daemon, &token).await?,
        Command::Config {
            instance,
            hardware,
            workspace,
            daemon,
            token,
        } => {
            Id::new("instance", instance.clone())
                .map_err(|e| machineemu_core::engine::Error::Invalid(e.to_string()))?;
            let path = format!("/api/v2/instances/{instance}/config");
            let mut document = daemon_request(&daemon, &token, "GET", &path, None).await?;
            let mut plan: machineemu_core::launch::LaunchSpec =
                serde_json::from_value(document["launch_plan"].clone())
                    .map_err(|e| machineemu_core::engine::Error::Invalid(e.to_string()))?;
            let (config, config_path) = machineemu_core::config::load_config(None)
                .map_err(|e| machineemu_core::engine::Error::Invalid(e.to_string()))?;
            let workspace = if workspace == Path::new("machineemu-workspace") {
                config
                    .client
                    .as_ref()
                    .and_then(|c| c.workspace.clone())
                    .or_else(|| config.server.as_ref().and_then(|s| s.workspace.clone()))
                    .map(|p| {
                        machineemu_core::config::resolve_config_path(config_path.as_deref(), p)
                    })
                    .unwrap_or(workspace)
            } else {
                workspace
            };
            hardware.apply(&mut plan, &workspace, &instance)?;
            document["launch_plan"] = serde_json::to_value(plan).unwrap();
            daemon_request(&daemon, &token, "PUT", &path, Some(document)).await?;
            println!("configured {instance}");
        }
        Command::Show {
            kind,
            id,
            json,
            daemon,
            token,
        } => {
            Id::new("document", id.clone())
                .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
            let value = daemon_request(&daemon, &token, "GET", &kind.path(&id), None).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&value).unwrap());
            } else {
                print!(
                    "{}",
                    serde_yaml::to_string(&value).map_err(|error| {
                        machineemu_core::engine::Error::Invalid(error.to_string())
                    })?
                );
            }
        }
        Command::Update {
            kind,
            id,
            file,
            daemon,
            token,
        } => {
            Id::new("document", id.clone())
                .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
            let document = load_document(&file)?;
            daemon_request(&daemon, &token, "PUT", &kind.path(&id), Some(document)).await?;
            println!("updated {kind:?} {id}");
        }
        Command::MigrateInstances { workspace } => {
            let workspace = Workspace::open(workspace)
                .map_err(|error| machineemu_core::engine::Error::Runtime(error.to_string()))?;
            println!(
                "instance documents: {}",
                workspace.root().join("instances").display()
            );
        }
        Command::MigrateImages { workspace } => {
            let workspace = Workspace::open(workspace)
                .map_err(|error| machineemu_core::engine::Error::Runtime(error.to_string()))?;
            println!(
                "image manifests: {}",
                workspace.root().join("images").display()
            );
        }
        Command::Images { workspace, json } => inventory::images(&workspace, json)?,
        Command::Profiles { workspace, json } => inventory::profiles(&workspace, json)?,
        Command::Logs {
            instance,
            workspace,
            follow,
            lines,
            source,
        } => {
            console::logs(&instance, workspace.as_deref(), follow, lines, &source)?;
        }
        Command::Serial {
            instance,
            workspace,
        } => {
            console::serial(&instance, workspace.as_deref())?;
        }
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
            )
            .await?;
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
                daemon_request(
                    &daemon,
                    &token,
                    "POST",
                    &format!("/api/v2/instances/{instance}/stop"),
                    None,
                )
                .await?;
            }
            let removed = daemon_request(
                &daemon,
                &token,
                "DELETE",
                &format!("/api/v2/instances/{instance}"),
                None,
            )
            .await;
            if let Err(error) = removed
                && (!force || !error.to_string().contains("HTTP 404"))
            {
                return Err(error);
            }
            println!("removed {instance}");
        }
        Command::Inspect {
            instance,
            workspace,
            json,
            daemon,
            token,
        } => inspect::inspect(&instance, workspace.as_deref(), &daemon, &token, json).await?,
        Command::AnalysisTarget {
            instances,
            workspace,
            add,
            clear,
        } => kvm::analysis_target(&instances, workspace.as_deref(), add, clear)?,
        Command::Ps {
            state_dir: _,
            daemon,
            token,
        } => {
            let response =
                daemon_request(&daemon, &token, "GET", "/api/v2/instances", None).await?;
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
        Command::Create(args) => {
            if let Some(file) = &args.file {
                launch::create_from_document(
                    file,
                    &args.hardware,
                    &args.workspace,
                    &args.daemon,
                    &args.token,
                    false,
                    false,
                )
                .await?;
            } else {
                run_rust_owned(args.options(launch::LaunchMode::Create, false, false)).await?;
            }
        }
        Command::Run(args) => {
            if let Some(file) = &args.launch.file {
                if args.fresh {
                    return Err(machineemu_core::engine::Error::Invalid(
                        "--fresh cannot be combined with --file".into(),
                    ));
                }
                launch::create_from_document(
                    file,
                    &args.launch.hardware,
                    &args.launch.workspace,
                    &args.launch.daemon,
                    &args.launch.token,
                    true,
                    args.rm,
                )
                .await?;
            } else {
                run_rust_owned(
                    args.launch
                        .options(launch::LaunchMode::Run, args.fresh, args.rm),
                )
                .await?;
            }
        }
        Command::Start {
            instance,
            daemon,
            token,
        } => {
            let started = daemon_request(
                &daemon,
                &token,
                "POST",
                &format!("/api/v2/instances/{instance}/start"),
                Some(serde_json::json!({})),
            )
            .await?;
            println!("started {instance}");
            if let Some(port) = started["vnc_port"].as_u64() {
                println!("VNC: 127.0.0.1:{port}");
            }
        }
        Command::Restart {
            instance,
            daemon,
            token,
        } => {
            let started = daemon_request(
                &daemon,
                &token,
                "POST",
                &format!("/api/v2/instances/{instance}/restart"),
                Some(serde_json::json!({})),
            )
            .await?;
            println!("restarted {instance}");
            if let Some(port) = started["vnc_port"].as_u64() {
                println!("VNC: 127.0.0.1:{port}");
            }
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
            mac,
            instance,
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
                mac,
                instance,
            };
            if validate_qemu {
                let executable = build_plan(input.clone())?.executable;
                let options = inspect_qemu(&executable, input.profile["machine"].as_str(), None)?;
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
            daemon,
            token,
            source,
            image_id,
            engine_track,
            target,
            export_bundle,
        } => {
            let source = source.unwrap_or_else(|| default_vmmanager_base_source(&image_id));
            let (daemon, token) = effective_client(&daemon, &token)?;
            client::ensure_daemon(&daemon, &token, &workspace).await?;
            let response = daemon_request(
                &daemon,
                &token,
                "POST",
                "/api/v2/image-imports/vmmanager-base",
                Some(serde_json::json!({
                    "source": source,
                    "image_id": image_id,
                    "engine_track": engine_track,
                    "target": target
                })),
            )
            .await?;
            let events_url = response["events_url"].as_str().ok_or_else(|| {
                machineemu_core::engine::Error::Invalid("daemon response has no events_url".into())
            })?;
            let status_url = response["status_url"].as_str().ok_or_else(|| {
                machineemu_core::engine::Error::Invalid("daemon response has no status_url".into())
            })?;
            let image = follow_image_import(&daemon, &token, events_url, status_url).await?;
            if let Some(destination) = export_bundle {
                Workspace::export_image_bundle_from_root(&workspace, &image.image_id, &destination)
                    .map_err(|error| machineemu_core::engine::Error::Runtime(error.to_string()))?;
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&image).expect("image is serializable")
            );
        }
        Command::RegisterImage {
            workspace,
            manifest,
        } => {
            let image: machineemu_core::domain::ImageManifest =
                serde_json::from_value(load_document(&manifest)?)
                    .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
            Workspace::open(workspace)
                .and_then(|workspace| workspace.register_image(&image))
                .map_err(|error| machineemu_core::engine::Error::Runtime(error.to_string()))?;
            println!("registered {}", image.image_id.as_str());
        }
    }
    Ok(())
}

fn default_vmmanager_base_source(image_id: &str) -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".vm-base")
        .join(image_id)
}

async fn follow_image_import(
    daemon: &str,
    token: &str,
    events_url: &str,
    status_url: &str,
) -> Result<machineemu_core::domain::ImageManifest, machineemu_core::engine::Error> {
    let mut last_event_id = None::<String>;
    let mut progress = ImportProgress::new();
    loop {
        let mut terminal = None::<Result<machineemu_core::domain::ImageManifest, String>>;
        let request_last_event_id = last_event_id.clone();
        let stream_result = client::daemon_sse_request(
            daemon,
            token,
            "GET",
            events_url,
            None,
            request_last_event_id.as_deref(),
            |event| {
                if let Some(id) = event.id {
                    last_event_id = Some(id);
                }
                let data: serde_json::Value =
                    serde_json::from_str(&event.data).map_err(|error| {
                        machineemu_core::engine::Error::Invalid(format!(
                            "invalid import progress event: {error}"
                        ))
                    })?;
                match event.event.as_str() {
                    "snapshot" | "progress" | "component-start" | "component-complete" => {
                        progress.render(&data);
                    }
                    "complete" => {
                        progress.render(&data);
                        eprintln!();
                        let manifest =
                            serde_json::from_value(data["manifest"].clone()).map_err(|error| {
                                machineemu_core::engine::Error::Invalid(error.to_string())
                            })?;
                        terminal = Some(Ok(manifest));
                        return Ok(false);
                    }
                    "failed" => {
                        eprintln!();
                        terminal = Some(Err(data["error"]
                            .as_str()
                            .unwrap_or("import failed")
                            .into()));
                        return Ok(false);
                    }
                    _ => {}
                }
                Ok(true)
            },
        )
        .await;
        if let Some(result) = terminal {
            return result.map_err(machineemu_core::engine::Error::Invalid);
        }
        if let Err(error) = stream_result {
            eprintln!("\nimport event stream disconnected: {error}; reconnecting");
        }
        let status = daemon_request(daemon, token, "GET", status_url, None).await?;
        match status["status"].as_str() {
            Some("complete") => {
                eprintln!();
                let manifest = serde_json::from_value(status["manifest"].clone())
                    .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
                return Ok(manifest);
            }
            Some("failed") => {
                eprintln!();
                return Err(machineemu_core::engine::Error::Invalid(
                    status["error"].as_str().unwrap_or("import failed").into(),
                ));
            }
            _ => progress.render(&status),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

struct ImportProgress {
    started: Instant,
    last_len: usize,
}

impl ImportProgress {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last_len: 0,
        }
    }

    fn render(&mut self, data: &serde_json::Value) {
        let done = data["bytes_done"].as_u64().unwrap_or(0);
        let total = data["bytes_total"].as_u64().unwrap_or(0);
        let component = data["component"]
            .as_str()
            .or_else(|| data["current_component"].as_str())
            .unwrap_or("import");
        let phase = data["phase"]
            .as_str()
            .or_else(|| data["status"].as_str())
            .unwrap_or("running");
        let byte_phase = matches!(
            phase,
            "copying+hashing" | "queued" | "complete" | "verified"
        );
        let percent = if total == 0 {
            0.0
        } else {
            (done as f64 / total as f64) * 100.0
        };
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let speed = done as f64 / elapsed;
        let eta = if !byte_phase {
            "--:--".into()
        } else if total > done && speed > 0.0 {
            format_duration(Duration::from_secs_f64((total - done) as f64 / speed))
        } else if total > 0 {
            "00:00".into()
        } else {
            "--:--".into()
        };
        let bar = progress_bar(done, total, 28);
        let line = format!(
            "{component:>10} {phase:<15} {bar} {percent:5.1}%  {} / {}  {}/s  eta {eta}",
            human_bytes(done),
            human_bytes(total),
            human_bytes(speed as u64)
        );
        let padding = self.last_len.saturating_sub(line.len());
        eprint!("\r{line}{}", " ".repeat(padding));
        self.last_len = line.len();
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}

fn progress_bar(done: u64, total: u64, width: usize) -> String {
    if total == 0 {
        return format!("[{}]", ".".repeat(width));
    }
    let filled = ((done as f64 / total as f64) * width as f64)
        .round()
        .clamp(0.0, width as f64) as usize;
    format!(
        "[{}{}]",
        "#".repeat(filled),
        ".".repeat(width.saturating_sub(filled))
    )
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn human_bytes(value: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

mod client;
mod device;
mod hardware;
mod launch;
use client::{daemon_request, effective_client};
use launch::run_rust_owned;

mod console;
mod inspect;
mod inventory;
mod kvm;
mod vnc;
