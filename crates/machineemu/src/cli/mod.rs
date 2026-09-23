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
use machineemu_core::engine::{
    PlanInput, build_plan, inspect_qemu, load_document, validate_legacy_config,
    validate_profile_against_qemu,
};
use machineemu_core::{domain::Id, storage::Workspace};

#[derive(Debug, Parser)]
#[command(name = "machineemu", about = "MachineEmu Rust planning tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
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
    /// List named profiles, preferring workspace profiles over the catalog.
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
    /// Start a named profile with the concise operator interface.
    Run {
        /// Profile/base ID, for example debian13-cloud.
        profile: String,
        /// Instance ID, for example lab01.
        instance: String,
        /// Registered image to use for disk, NVRAM and TPM seed state.
        #[arg(long)]
        image: Option<String>,
        /// Allow --image with a profile engine track not declared by the image.
        #[arg(long, requires = "image")]
        force: bool,
        /// Optional per-instance NoCloud/cloud-init seed ISO.
        #[arg(long)]
        seed: Option<PathBuf>,
        /// Networking mode: profile, user, bridge[:BRIDGE], or none.
        #[arg(long, default_value = "profile")]
        net: String,
        /// VNC port: profile, auto, none, or TCP port 5900-5999.
        #[arg(long, default_value = "profile")]
        vnc: String,
        /// File containing a VNC password (1-8 bytes, with no newline).
        #[arg(long)]
        vnc_password_file: Option<PathBuf>,
        /// Override the default Unix QMP relay socket for external controllers.
        #[arg(long)]
        qmp_socket: Option<PathBuf>,
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
        /// NIC address for this run. Defaults to the profile's devices.mac,
        /// then an address derived from the instance name.
        #[arg(long)]
        mac: Option<String>,
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
    /// Register an image manifest whose blobs are already imported.
    RegisterImage {
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Import one host asset into the workspace content-addressed store.
    ImportAsset {
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        source: PathBuf,
    },
}

pub fn main() {
    if let Err(error) = run() {
        eprintln!("machineemu: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), machineemu_core::engine::Error> {
    let cli = Cli::parse();
    match cli.command {
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
        Command::Inspect {
            instance,
            workspace,
            json,
            daemon,
            token,
        } => inspect::inspect(&instance, workspace.as_deref(), &daemon, &token, json)?,
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
            image,
            force,
            seed,
            net,
            vnc,
            vnc_password_file,
            qmp_socket,
            fresh,
            workspace,
            qemu,
            swtpm,
            bridge_helper,
            mac,
            daemon,
            token,
        } => {
            run_rust_owned(launch::RunOptions {
                profile_name: &profile,
                instance: &instance,
                image: image.as_deref(),
                force,
                seed: seed.as_deref(),
                net: &net,
                vnc: &vnc,
                vnc_password_file: vnc_password_file.as_deref(),
                external_qmp_socket: qmp_socket.as_deref(),
                fresh,
                workspace_root: &workspace,
                qemu: &qemu,
                swtpm: swtpm.as_deref(),
                bridge_helper: bridge_helper.as_deref(),
                mac: mac.as_deref(),
                daemon: &daemon,
                token: &token,
            })?;
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
            source,
            image_id,
            engine_track,
            target,
            export_bundle,
        } => {
            let workspace = Workspace::open(workspace)
                .map_err(|error| machineemu_core::engine::Error::Runtime(error.to_string()))?;
            let image = workspace
                .import_vmmanager_base(
                    source,
                    Id::new("image", image_id).map_err(|error| {
                        machineemu_core::engine::Error::Runtime(error.to_string())
                    })?,
                    Id::new("engine track", engine_track).map_err(|error| {
                        machineemu_core::engine::Error::Runtime(error.to_string())
                    })?,
                    target,
                )
                .map_err(|error| machineemu_core::engine::Error::Runtime(error.to_string()))?;
            if let Some(destination) = export_bundle {
                workspace
                    .export_image_bundle(&image.image_id, &destination)
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
        Command::ImportAsset { workspace, source } => {
            let workspace = Workspace::open(workspace)
                .map_err(|error| machineemu_core::engine::Error::Runtime(error.to_string()))?;
            let digest = workspace
                .import_file(&source)
                .map_err(|error| machineemu_core::engine::Error::Runtime(error.to_string()))?;
            println!("sha256:{digest}");
        }
    }
    Ok(())
}

mod client;
mod launch;
use client::{daemon_request, effective_client};
use launch::run_rust_owned;

mod console;
mod inspect;
mod inventory;
mod kvm;
mod vnc;
