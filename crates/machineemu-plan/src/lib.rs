use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("runtime error: {0}")]
    Runtime(String),
}

#[derive(Clone)]
pub struct PlanInput {
    pub profile: Value,
    pub release_set: Value,
    pub bundle_root: PathBuf,
    pub asset_root: Option<PathBuf>,
    pub target: String,
    pub runtime_dir: PathBuf,
    pub state_dir: Option<PathBuf>,
    pub seed: Option<PathBuf>,
    /// The swtpm executable for profiles that ask for an emulated TPM. A bare
    /// name is resolved through PATH when the helper is started; None means
    /// `swtpm`.
    pub swtpm: Option<PathBuf>,
    /// The setuid qemu-bridge-helper for bridge networking. QEMU otherwise
    /// runs the copy beside its own executable, which an engine built from
    /// source has no privileges for.
    pub bridge_helper: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct LaunchPlan {
    pub schema_version: u8,
    pub executable: PathBuf,
    pub argv: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub preparation: Preparation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub helper_argv: Option<Vec<String>>,
    pub manifest: Value,
}

#[derive(Debug, Default, Serialize)]
pub struct Preparation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nvram: Option<PreparationFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_overlay: Option<DiskPreparation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tpm_state: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct PreparationFile {
    pub path: PathBuf,
    pub seed: PathBuf,
}
#[derive(Debug, Serialize)]
pub struct DiskPreparation {
    pub path: PathBuf,
    pub backing: PathBuf,
    pub backing_format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
}

/// Capabilities reported by the selected QEMU binary itself.
///
/// These values are discovery data, not a replacement for model policy. A
/// model still decides which choices are valid for a guest; this prevents the
/// UI and planner from offering values that the installed binary cannot parse.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct QemuOptions {
    pub executable: PathBuf,
    pub version: Option<String>,
    pub machines: Vec<String>,
    pub cpus: Vec<String>,
    pub accelerators: Vec<String>,
    pub devices: Vec<String>,
    pub display_backends: Vec<String>,
    pub chardev_backends: Vec<String>,
    pub tpm_backends: Vec<String>,
    pub audio_drivers: Vec<String>,
    pub machine_properties: Vec<String>,
    pub analysis_machine_properties: Vec<String>,
    pub device_properties: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct LegacyValidationReport {
    pub valid: bool,
    pub qemu: PathBuf,
    pub boards: BTreeMap<String, LegacyBoardReport>,
}

#[derive(Debug, Serialize)]
pub struct LegacyBoardReport {
    pub valid: bool,
    pub machine: Option<String>,
    pub errors: Vec<String>,
}

pub fn validate_legacy_config(
    document: &Value,
    executable: &Path,
    instance: Option<&str>,
) -> Result<LegacyValidationReport, Error> {
    let root = object(document, "legacy configuration")?;
    if root.get("version").and_then(Value::as_i64) != Some(1) {
        return Err(invalid("legacy configuration version must be 1"));
    }
    let boards = object(
        root.get("boards")
            .ok_or_else(|| invalid("legacy configuration boards is required"))?,
        "boards",
    )?;
    let selected = if let Some(instance_name) = instance {
        let instances = object(
            root.get("instances")
                .ok_or_else(|| invalid("legacy configuration instances is required"))?,
            "instances",
        )?;
        let instance_value = object(
            instances
                .get(instance_name)
                .ok_or_else(|| invalid(&format!("legacy instance not found: {instance_name}")))?,
            "instance",
        )?;
        vec![string(instance_value, "board")?]
    } else {
        boards.keys().cloned().collect()
    };
    let mut reports = BTreeMap::new();
    for board_name in selected {
        let board = object(
            boards
                .get(&board_name)
                .ok_or_else(|| invalid(&format!("legacy board not found: {board_name}")))?,
            "board",
        )?;
        let machine = board_machine(board).ok();
        let options = inspect_qemu(executable, machine.as_deref(), None)?;
        reports.insert(board_name, validate_legacy_board(board, &options, machine));
    }
    let valid = reports.values().all(|report| report.valid);
    Ok(LegacyValidationReport {
        valid,
        qemu: executable.to_owned(),
        boards: reports,
    })
}

fn board_machine(board: &Map<String, Value>) -> Result<String, Error> {
    board
        .get("os")
        .and_then(|os| os.get("type"))
        .and_then(|kind| kind.get("machine"))
        .and_then(Value::as_str)
        .or_else(|| {
            board
                .get("machine")
                .and_then(|machine| machine.get("type"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
        .ok_or_else(|| invalid("legacy board machine is missing"))
}

fn validate_legacy_board(
    board: &Map<String, Value>,
    options: &QemuOptions,
    machine: Option<String>,
) -> LegacyBoardReport {
    let mut errors = Vec::new();
    let machine_name = machine.clone().unwrap_or_default();
    if machine.is_none() {
        errors.push("board machine is missing".into());
    } else if !options.machines.iter().any(|value| value == &machine_name) {
        errors.push(format!("QEMU does not support machine {machine_name:?}"));
    }
    if let Some(cpu) = board
        .get("cpu")
        .and_then(|value| value.get("model"))
        .and_then(Value::as_str)
    {
        let name = cpu.split(',').next().unwrap_or(cpu);
        if !options.cpus.iter().any(|value| value == name) {
            errors.push(format!("QEMU does not support CPU {name:?}"));
        }
    }
    if let Some(accelerator) = board
        .get("accelerator")
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
    {
        if !options
            .accelerators
            .iter()
            .any(|value| value == accelerator)
        {
            errors.push(format!("QEMU does not support accelerator {accelerator:?}"));
        }
    }
    if let Some(video) = board
        .get("video")
        .and_then(|value| value.get("model"))
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
    {
        let device = match video {
            "vga" => "VGA",
            "virtio-gl" => "virtio-vga-gl",
            other => other,
        };
        if !options.devices.iter().any(|value| value == device) {
            errors.push(format!("QEMU does not support video device {device:?}"));
        }
    }
    if let Some(audio) = board
        .get("audio")
        .and_then(|value| value.get("model"))
        .and_then(Value::as_str)
    {
        if audio != "none" {
            let device = if audio == "ich9" {
                "ich9-intel-hda"
            } else {
                audio
            };
            if !options.devices.iter().any(|value| value == device) {
                errors.push(format!("QEMU does not support audio device {device:?}"));
            }
        }
    }
    if let Some(tpm) = board
        .get("tpm")
        .and_then(|value| value.get("model"))
        .and_then(Value::as_str)
    {
        if !options.devices.iter().any(|value| value == tpm) {
            errors.push(format!("QEMU does not support TPM device {tpm:?}"));
        }
    }
    if let Some(bus) = board
        .get("storage")
        .and_then(|value| value.get("disk"))
        .and_then(|value| value.get("target"))
        .and_then(|value| value.get("bus"))
        .and_then(Value::as_str)
    {
        if bus == "sata" && !options.devices.iter().any(|value| value == "ich9-ahci") {
            errors.push("QEMU does not support the SATA controller ich9-ahci".into());
        }
    }
    if board
        .get("analysis")
        .and_then(|value| value.get("enabled"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        for property in ["analysis-profile", "x-analysis-profile-json-base64"] {
            if !options
                .machine_properties
                .iter()
                .any(|value| value == property)
            {
                errors.push(format!(
                    "QEMU machine does not support analysis property {property:?}"
                ));
            }
        }
    }
    LegacyBoardReport {
        valid: errors.is_empty(),
        machine,
        errors,
    }
}

pub fn validate_profile_against_qemu(profile: &Value, options: &QemuOptions) -> Result<(), Error> {
    let profile = object(profile, "profile")?;
    let machine = string(profile, "machine")?;
    let machine_name = machine.split(',').next().unwrap_or(&machine);
    if !options.machines.iter().any(|item| item == machine_name) {
        return Err(invalid(&format!(
            "QEMU {} does not support machine {machine_name:?}",
            options.executable.display()
        )));
    }
    if let Some(version) = profile
        .get("engine")
        .and_then(|engine| engine.get("version"))
        .and_then(Value::as_str)
    {
        let actual = options
            .version
            .as_deref()
            .and_then(qemu_version_number)
            .ok_or_else(|| invalid("QEMU version could not be determined"))?;
        if actual != version {
            return Err(invalid(&format!(
                "profile requires QEMU version {version}, but executable reports {actual}"
            )));
        }
    }
    if let Some(cpu) = profile.get("cpu").and_then(Value::as_str) {
        let cpu_name = cpu.split(',').next().unwrap_or(cpu);
        if !options.cpus.iter().any(|item| item == cpu_name) {
            return Err(invalid(&format!(
                "QEMU {} does not support CPU {cpu_name:?}",
                options.executable.display()
            )));
        }
    }
    let empty_resources = Value::Object(Map::new());
    if let Some(accelerator) = object(
        profile.get("resources").unwrap_or(&empty_resources),
        "profile.resources",
    )?
    .get("accelerator")
    .and_then(Value::as_str)
    {
        let accelerator_name = accelerator.split(',').next().unwrap_or(accelerator);
        if !options
            .accelerators
            .iter()
            .any(|item| item == accelerator_name)
        {
            return Err(invalid(&format!(
                "QEMU {} does not support accelerator {accelerator_name:?}",
                options.executable.display()
            )));
        }
    }
    if let Some(devices) = profile.get("devices") {
        let devices = object(devices, "profile.devices")?;
        if let Some(nic) = devices.get("nic").and_then(Value::as_str) {
            if !options.devices.iter().any(|item| item == nic) {
                return Err(invalid(&format!(
                    "QEMU {} does not support NIC device {nic:?}",
                    options.executable.display()
                )));
            }
        }
        if let Some(video) = devices
            .get("video")
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
        {
            let video = match video {
                "vga" => "VGA",
                "virtio-gl" => "virtio-vga-gl",
                other => other,
            };
            if !options.devices.iter().any(|item| item == video) {
                return Err(invalid(&format!(
                    "QEMU {} does not support video device {video:?}",
                    options.executable.display()
                )));
            }
        }
        if let Some(audio) = devices
            .get("audio")
            .and_then(|value| value.get("model"))
            .and_then(Value::as_str)
            .filter(|model| *model != "none")
        {
            let audio = if audio == "ich9" {
                "ich9-intel-hda"
            } else {
                audio
            };
            if !options.devices.iter().any(|item| item == audio) {
                return Err(invalid(&format!(
                    "QEMU {} does not support audio device {audio:?}",
                    options.executable.display()
                )));
            }
        }
    }
    if let Some(tpm) = profile
        .get("tpm")
        .and_then(|value| value.get("model"))
        .and_then(Value::as_str)
    {
        if !options.devices.iter().any(|item| item == tpm) {
            return Err(invalid(&format!(
                "QEMU {} does not support TPM device {tpm:?}",
                options.executable.display()
            )));
        }
    }
    if profile
        .get("analysis")
        .and_then(|value| value.get("enabled"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        for property in ["analysis-profile", "x-analysis-profile-json-base64"] {
            if !options
                .machine_properties
                .iter()
                .any(|value| value == property)
            {
                return Err(invalid(&format!(
                    "QEMU {} does not support analysis property {property:?}",
                    options.executable.display()
                )));
            }
        }
    }
    Ok(())
}

fn qemu_version_number(version: &str) -> Option<&str> {
    let mut words = version.split_whitespace();
    while let Some(word) = words.next() {
        if word == "version" {
            return words.next();
        }
    }
    None
}

pub fn load_document(path: &Path) -> Result<Value, Error> {
    let text = fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    match path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "json" => serde_json::from_str(&text).map_err(|e| Error::Parse {
            path: path.to_owned(),
            message: e.to_string(),
        }),
        "yaml" | "yml" => serde_yaml::from_str(&text).map_err(|e| Error::Parse {
            path: path.to_owned(),
            message: e.to_string(),
        }),
        suffix => Err(Error::Invalid(format!(
            "unsupported configuration format .{suffix}"
        ))),
    }
}

pub fn inspect_qemu(
    executable: &Path,
    machine: Option<&str>,
    device: Option<&str>,
) -> Result<QemuOptions, Error> {
    if !executable.is_file() {
        return Err(invalid(&format!(
            "QEMU executable is missing: {}",
            executable.display()
        )));
    }
    let version = run_qemu_help(executable, &["--version"])
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.contains("QEMU emulator version"))
                .map(str::to_owned)
        });
    let machines = parse_named_help(&run_qemu_help(executable, &["-machine", "help"])?);
    let cpus = parse_named_help(&run_qemu_help(executable, &["-cpu", "help"])?);
    let accelerators = parse_named_help(&run_qemu_help(executable, &["-accel", "help"])?);
    let devices = parse_device_help(&run_qemu_help(executable, &["-device", "help"])?);
    let display_backends = optional_named_help(executable, &["-display", "help"]);
    let chardev_backends = optional_named_help(executable, &["-chardev", "help"]);
    let tpm_backends = optional_named_help(executable, &["-tpmdev", "help"]);
    let audio_drivers = optional_named_help(executable, &["-audiodev", "help"]);
    let machine_properties = machine
        .map(|name| optional_named_help(executable, &["-machine", &format!("{name},help")]))
        .unwrap_or_default();
    let device_properties = device
        .map(|name| optional_named_help(executable, &["-device", &format!("{name},help")]))
        .unwrap_or_default();
    let analysis_machine_properties = machine_properties
        .iter()
        .filter(|name| *name == "analysis-profile" || name.starts_with("x-analysis-"))
        .cloned()
        .collect();
    Ok(QemuOptions {
        executable: executable.to_owned(),
        version,
        machines,
        cpus,
        accelerators,
        devices,
        display_backends,
        chardev_backends,
        tpm_backends,
        audio_drivers,
        machine_properties,
        analysis_machine_properties,
        device_properties,
    })
}

fn optional_named_help(executable: &Path, args: &[&str]) -> Vec<String> {
    run_qemu_help(executable, args)
        .map(|text| {
            if args.len() == 2 && args[0] == "-device" && args[1] == "help" {
                parse_device_help(&text)
            } else if args.len() == 2 && args[1] == "help" {
                parse_indented_list(&text)
            } else {
                parse_property_help(&text)
            }
        })
        .unwrap_or_default()
}

fn run_qemu_help(executable: &Path, args: &[&str]) -> Result<String, Error> {
    let output = Command::new(executable)
        .args(args)
        .output()
        .map_err(|source| Error::Io {
            path: executable.to_owned(),
            source,
        })?;
    if !output.status.success() {
        return Err(invalid(&format!(
            "{} {:?} failed with {}: {}",
            executable.display(),
            args,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(text)
}

fn parse_named_help(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('-')
            || trimmed.ends_with(':')
            || trimmed.starts_with("Supported ")
        {
            continue;
        }
        let Some(name) = trimmed.split_whitespace().next() else {
            continue;
        };
        if name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            && !values.iter().any(|item| item == name)
        {
            values.push(name.to_owned());
        }
    }
    values
}

fn parse_indented_list(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut collecting = !text
        .lines()
        .any(|line| line.trim().starts_with("Available "));
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Available ") {
            collecting = true;
            continue;
        }
        if !collecting {
            continue;
        }
        if trimmed.is_empty() {
            if !values.is_empty() {
                break;
            }
            continue;
        }
        let mut words = trimmed.split_whitespace();
        let Some(name) = words.next() else { continue };
        if words.next().is_none()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            && !values.iter().any(|item| item == name)
        {
            values.push(name.to_owned());
        }
    }
    values
}

fn parse_device_help(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("name \"") else {
            continue;
        };
        let Some(end) = rest.find('"') else { continue };
        let name = &rest[..end];
        if !values.iter().any(|item| item == name) {
            values.push(name.to_owned());
        }
    }
    values
}

fn parse_property_help(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.ends_with(':') {
            continue;
        }
        let Some(token) = trimmed.split_whitespace().next() else {
            continue;
        };
        let name = token.split('=').next().unwrap_or(token);
        if name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            && !values.iter().any(|item| item == name)
        {
            values.push(name.to_owned());
        }
    }
    values
}

pub fn build_plan(input: PlanInput) -> Result<LaunchPlan, Error> {
    let profile = object(&input.profile, "profile")?;
    if number(profile, "schema_version")? != 1 {
        return Err(Error::Invalid("profile schema_version must be 1".into()));
    }
    let profile_id = string(profile, "id")?;
    let machine = string(profile, "machine")?;
    let target = &input.target;
    let track = string(
        object(
            profile
                .get("engine")
                .ok_or_else(|| invalid("profile.engine is required"))?,
            "profile.engine",
        )?,
        "track",
    )?;
    let (engine, executable) =
        resolve_engine(&input.release_set, &input.bundle_root, &track, target)?;
    if let Some(expected) = profile
        .get("engine")
        .and_then(|engine| engine.get("build_digest"))
        .and_then(Value::as_str)
    {
        let expected = expected.strip_prefix("sha256:").unwrap_or(expected);
        let actual = engine
            .get("build_digest")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if expected != actual {
            return Err(invalid(
                "profile.engine.build_digest does not match the resolved engine",
            ));
        }
    }
    let swtpm = input.swtpm.unwrap_or_else(|| PathBuf::from("swtpm"));
    let bridge_helper = input.bridge_helper;
    let runtime = input
        .runtime_dir
        .canonicalize()
        .unwrap_or(input.runtime_dir);
    let state = input.state_dir.unwrap_or_else(|| runtime.clone());
    let empty_resources = Value::Object(Map::new());
    let resources = object(
        profile.get("resources").unwrap_or(&empty_resources),
        "profile.resources",
    )?;
    let memory = memory(
        resources
            .get("memory")
            .ok_or_else(|| invalid("profile.resources.memory must be supplied"))?,
    )?;
    let vcpus = positive_int(
        resources.get("vcpus").unwrap_or(&Value::from(1)),
        "profile.resources.vcpus",
    )?;
    let cpu = profile.get("cpu").and_then(Value::as_str).unwrap_or("max");
    let machine_arg = machine_value(&machine, profile.get("smm"), profile.get("analysis"))?;
    let qmp = runtime.join("sockets/qmp.sock");
    let pidfile = runtime.join("control/qemu.pid");
    let mut argv = vec![
        executable.to_string_lossy().into_owned(),
        "-machine".into(),
        machine_arg.clone(),
    ];
    if let Some(accel) = resources.get("accelerator").and_then(Value::as_str) {
        argv.extend(accelerator(accel, resources.get("accelerator_thread"))?);
    }
    argv.extend([
        "-cpu".into(),
        cpu.into(),
        "-m".into(),
        memory,
        "-smp".into(),
        smp(resources, vcpus)?,
    ]);
    argv.extend([
        "-qmp".into(),
        format!("unix:{},server=on,wait=off", qmp.display()),
        "-pidfile".into(),
        pidfile.display().to_string(),
    ]);
    let nic = profile
        .get("devices")
        .and_then(|value| value.get("nic"))
        .and_then(Value::as_str);
    append_network(
        &mut argv,
        profile.get("network"),
        nic,
        bridge_helper.as_deref(),
    )?;
    if let Some(seed) = &input.seed {
        if !seed.is_file() {
            return Err(invalid(&format!(
                "cloud-init seed is unavailable: {}",
                seed.display()
            )));
        }
        argv.extend([
            "-drive".into(),
            format!("file={},media=cdrom,readonly=on", seed.display()),
        ]);
    }
    let assets = resolve_assets(profile, input.asset_root.as_deref())?;
    let mut prep = Preparation::default();
    append_firmware(
        &mut argv,
        &mut prep,
        profile.get("firmware"),
        &assets,
        &state,
    )?;
    append_storage(
        &mut argv,
        &mut prep,
        profile.get("storage"),
        &assets,
        &machine,
        &state,
    )?;
    let helper_argv = if profile.get("tpm").is_some() {
        let socket = runtime.join("sockets/tpm.sock");
        append_tpm(&mut argv, profile.get("tpm"), &socket)?;
        prep.tpm_state = Some(state.join("tpm"));
        Some(vec![
            swtpm.to_string_lossy().into_owned(),
            "socket".into(),
            "--tpm2".into(),
            "--tpmstate".into(),
            format!("dir={}", state.join("tpm").display()),
            "--ctrl".into(),
            format!("type=unixio,path={}", socket.display()),
        ])
    } else {
        None
    };
    append_devices(&mut argv, profile.get("devices"), &runtime)?;
    let manifest = serde_json::json!({"schema_version":1,"profile_id":profile_id,"target":target,"machine":machine,"machine_argument":machine_arg,"engine":engine,"resources":{"memory":argv[argv.iter().position(|x|x=="-m").unwrap()+1],"vcpus":vcpus},"qmp_socket":qmp,"pidfile":pidfile,"assets":assets});
    Ok(LaunchPlan {
        schema_version: 1,
        executable,
        argv,
        environment: BTreeMap::new(),
        preparation: prep,
        helper_argv,
        manifest,
    })
}

fn invalid(s: &str) -> Error {
    Error::Invalid(s.into())
}
fn object<'a>(v: &'a Value, name: &str) -> Result<&'a Map<String, Value>, Error> {
    v.as_object()
        .ok_or_else(|| invalid(&format!("{name} must be a mapping")))
}
fn string(v: &Map<String, Value>, key: &str) -> Result<String, Error> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|x| !x.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid(&format!("{key} must be a non-empty string")))
}
fn number(v: &Map<String, Value>, key: &str) -> Result<i64, Error> {
    v.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid(&format!("{key} must be an integer")))
}
fn positive_int(v: &Value, key: &str) -> Result<i64, Error> {
    let n = v
        .as_i64()
        .ok_or_else(|| invalid(&format!("{key} must be a positive integer")))?;
    if n < 1 {
        return Err(invalid(&format!("{key} must be a positive integer")));
    }
    Ok(n)
}
fn memory(v: &Value) -> Result<String, Error> {
    let raw = v
        .as_str()
        .map(str::to_owned)
        .or_else(|| v.as_i64().map(|x| x.to_string()))
        .ok_or_else(|| invalid("profile.resources.memory must be a non-empty size"))?;
    let raw = raw.trim();
    let (digits, suffix) = raw.chars().partition::<String, _>(|c| c.is_ascii_digit());
    if digits.is_empty() || digits.parse::<u64>().unwrap_or(0) == 0 {
        return Err(invalid("profile.resources.memory is not a supported size"));
    }
    let suffix = match suffix.as_str() {
        "" | "M" | "MiB" => "M",
        "K" | "KiB" => "K",
        "G" | "GiB" => "G",
        "T" => "T",
        "B" => "B",
        _ => return Err(invalid("profile.resources.memory is not a supported size")),
    };
    Ok(format!("{digits}{suffix}"))
}
fn smp(r: &Map<String, Value>, v: i64) -> Result<String, Error> {
    let Some(t) = r.get("topology") else {
        return Ok(v.to_string());
    };
    let t = object(t, "profile.resources.topology")?;
    let mut out = vec![v.to_string()];
    for k in ["sockets", "dies", "clusters", "cores", "threads"] {
        if let Some(x) = t.get(k) {
            out.push(format!(
                "{k}={}",
                positive_int(x, &format!("profile.resources.topology.{k}"))?
            ));
        }
    }
    Ok(out.join(","))
}
fn accelerator(a: &str, thread: Option<&Value>) -> Result<Vec<String>, Error> {
    match a {
        "kvm" => Ok(vec!["-accel".into(), "kvm".into()]),
        "tcg" => {
            let t = thread.and_then(Value::as_str).unwrap_or("multi");
            if !["single", "multi"].contains(&t) {
                return Err(invalid(
                    "profile.resources.accelerator_thread must be single or multi",
                ));
            }
            Ok(vec!["-accel".into(), format!("tcg,thread={t}")])
        }
        _ => Err(invalid("profile.resources.accelerator must be kvm or tcg")),
    }
}
fn machine_value(
    machine: &str,
    smm: Option<&Value>,
    analysis: Option<&Value>,
) -> Result<String, Error> {
    let mut m = machine.to_owned();
    if smm.and_then(Value::as_bool).unwrap_or(false) {
        m.push_str(",smm=on")
    }
    if analysis.is_some() {
        return Err(invalid(
            "profile.analysis is not implemented in the Rust planner yet",
        ));
    }
    Ok(m)
}
fn append_network(
    argv: &mut Vec<String>,
    v: Option<&Value>,
    nic: Option<&str>,
    bridge_helper: Option<&Path>,
) -> Result<(), Error> {
    let t = v
        .and_then(|x| x.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("user");
    match t {
        "disabled" => argv.extend(["-nic".into(), "none".into()]),
        "user" => {
            let model = nic.unwrap_or("virtio-net-pci");
            argv.extend([
                "-netdev".into(),
                "user,id=net0".into(),
                "-device".into(),
                format!("{model},netdev=net0"),
            ])
        }
        "bridge" => {
            let b = v
                .and_then(|x| x.get("bridge"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    invalid("profile.network.bridge is required for bridge networking")
                })?;
            let model = nic.unwrap_or("virtio-net-pci");
            // QEMU looks for qemu-bridge-helper beside its own executable
            // unless it is told otherwise. That copy carries no capabilities
            // for an engine built out of this repository, so a configured
            // helper is the only one that can open a tap device.
            let helper = match bridge_helper {
                Some(path) => format!(",helper={}", path.display()),
                None => String::new(),
            };
            argv.extend([
                "-netdev".into(),
                format!("bridge,id=net0,br={b}{helper}"),
                "-device".into(),
                format!("{model},netdev=net0"),
            ])
        }
        _ => {
            return Err(invalid(
                "profile.network.type must be disabled, user, or bridge",
            ));
        }
    }
    Ok(())
}

fn append_devices(argv: &mut Vec<String>, v: Option<&Value>, runtime: &Path) -> Result<(), Error> {
    let Some(devices) = v else {
        argv.extend(["-display".into(), "none".into()]);
        return Ok(());
    };
    let devices = object(devices, "profile.devices")?;
    if devices
        .get("guest_agent")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let socket = runtime.join("qga.sock");
        argv.extend([
            "-chardev".into(),
            format!(
                "socket,id=qga0,path={},server=on,wait=off",
                socket.display()
            ),
            "-device".into(),
            "virtio-serial-pci,id=virtio-serial0".into(),
            "-device".into(),
            "virtserialport,chardev=qga0,name=org.qemu.guest_agent.0".into(),
        ]);
    }
    if devices.get("serial").and_then(Value::as_str) == Some("file") {
        argv.extend([
            "-serial".into(),
            format!("file:{}", runtime.join("serial.log").display()),
        ]);
    }
    if devices.get("vnc").and_then(Value::as_bool).unwrap_or(false) {
        argv.extend(["-display".into(), "vnc=:0".into()]);
    } else {
        argv.extend(["-display".into(), "none".into()]);
    }
    Ok(())
}
fn resolve_assets(
    profile: &Map<String, Value>,
    root: Option<&Path>,
) -> Result<BTreeMap<String, PathBuf>, Error> {
    let mut out = BTreeMap::new();
    let Some(assets) = profile.get("assets") else {
        return Ok(out);
    };
    let assets = object(assets, "profile.assets")?;
    let root = root.ok_or_else(|| invalid("profile assets require --asset-root"))?;
    for (name, reference) in assets {
        let r = reference
            .as_str()
            .ok_or_else(|| invalid(&format!("profile.assets.{name} must be a sha256 reference")))?;
        let digest = r
            .strip_prefix("sha256:")
            .filter(|x| x.len() == 64 && x.chars().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(|| invalid(&format!("profile.assets.{name} must be a sha256 reference")))?;
        let p = root.join("sha256").join(digest);
        if !p.is_file() {
            return Err(invalid(&format!("asset {name:?} is unavailable: {r}")));
        }
        out.insert(name.clone(), p);
    }
    Ok(out)
}
fn asset(
    assets: &BTreeMap<String, PathBuf>,
    v: Option<&Value>,
    where_: &str,
) -> Result<PathBuf, Error> {
    let n = v
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(&format!("{where_} must name a profile asset")))?;
    assets.get(n).cloned().ok_or_else(|| {
        invalid(&format!(
            "{where_} names an asset that is not imported: {n}"
        ))
    })
}
fn append_firmware(
    argv: &mut Vec<String>,
    prep: &mut Preparation,
    v: Option<&Value>,
    assets: &BTreeMap<String, PathBuf>,
    state: &Path,
) -> Result<(), Error> {
    let Some(v) = v else { return Ok(()) };
    let x = object(v, "profile.firmware")?;
    let loader = object(
        x.get("loader")
            .ok_or_else(|| invalid("profile.firmware requires both loader and nvram mappings"))?,
        "loader",
    )?;
    let nvram = object(
        x.get("nvram")
            .ok_or_else(|| invalid("profile.firmware requires both loader and nvram mappings"))?,
        "nvram",
    )?;
    let code = asset(assets, loader.get("asset"), "profile.firmware.loader.asset")?;
    let seed = asset(assets, nvram.get("asset"), "profile.firmware.nvram.asset")?;
    let name = nvram
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("OVMF_VARS.fd");
    if name.contains('/') || name == "." || name == ".." {
        return Err(invalid(
            "profile.firmware.nvram.name must be a plain file name",
        ));
    }
    let path = state.join(name);
    argv.extend([
        "-drive".into(),
        format!(
            "if=pflash,format=raw,unit=0,readonly={},file={}",
            if loader
                .get("readonly")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                "on"
            } else {
                "off"
            },
            code.display()
        ),
        "-drive".into(),
        format!("if=pflash,format=raw,unit=1,file={}", path.display()),
    ]);
    if loader
        .get("secure")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        argv.extend([
            "-global".into(),
            "driver=cfi.pflash01,property=secure,value=on".into(),
        ])
    }
    prep.nvram = Some(PreparationFile { path, seed });
    Ok(())
}
fn append_storage(
    argv: &mut Vec<String>,
    prep: &mut Preparation,
    v: Option<&Value>,
    assets: &BTreeMap<String, PathBuf>,
    machine: &str,
    state: &Path,
) -> Result<(), Error> {
    let Some(v) = v else { return Ok(()) };
    let x = object(v, "profile.storage")?;
    let disk = object(
        x.get("disk")
            .ok_or_else(|| invalid("profile.storage.disk must be a mapping"))?,
        "profile.storage.disk",
    )?;
    let backing = asset(assets, disk.get("asset"), "profile.storage.disk.asset")?;
    let bus = disk.get("bus").and_then(Value::as_str).unwrap_or("sata");
    let legacy = machine == "pc" || machine.starts_with("pc-i440fx");
    if (legacy && bus == "sata") || (!legacy && bus == "ide") {
        return Err(invalid(if legacy {
            "the pc machine uses bus ide; sata requires q35"
        } else {
            "q35 machines use bus sata; ide requires the pc machine"
        }));
    }
    let path = state.join("overlay.qcow2");
    let fmt = disk
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("qcow2");
    if !["raw", "qcow2"].contains(&fmt) {
        return Err(invalid("profile.storage.disk.format must be raw or qcow2"));
    }
    let size = disk
        .get("size")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| invalid("profile.storage.disk.size must be a size string"))
                .and_then(disk_size)
        })
        .transpose()?;
    let mut opt = format!("if=none,id=pc-disk,file={},format=qcow2", path.display());
    if disk
        .get("readonly")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        opt.push_str(",readonly=on")
    }
    if bus == "sata" {
        argv.extend(["-device".into(), "ich9-ahci,id=pc-sata".into()])
    }
    argv.extend([
        "-drive".into(),
        opt,
        "-device".into(),
        match bus {
            "virtio" => "virtio-blk-pci,drive=pc-disk".into(),
            "ide" if legacy => "ide-hd,bus=ide.0,drive=pc-disk".into(),
            "sata" if !legacy => "ide-hd,bus=pc-sata.0,drive=pc-disk".into(),
            _ => return Err(invalid("unsupported storage bus for selected machine")),
        },
    ]);
    prep.disk_overlay = Some(DiskPreparation {
        path,
        backing,
        backing_format: fmt.into(),
        size,
    });
    Ok(())
}

fn disk_size(value: &str) -> Result<String, Error> {
    let value = value.trim();
    let (number, suffix) = value
        .chars()
        .position(|character| !character.is_ascii_digit() && character != '.')
        .map(|index| value.split_at(index))
        .ok_or_else(|| invalid("profile.storage.disk.size must include a unit"))?;
    if number.is_empty()
        || number.parse::<f64>().is_err()
        || !matches!(suffix, "M" | "G" | "T" | "MiB" | "GiB" | "TiB")
    {
        return Err(invalid(
            "profile.storage.disk.size must use M, G, T, MiB, GiB, or TiB",
        ));
    }
    let qemu_suffix = match suffix {
        "MiB" => "M",
        "GiB" => "G",
        "TiB" => "T",
        suffix => suffix,
    };
    Ok(format!("{number}{qemu_suffix}"))
}
fn append_tpm(argv: &mut Vec<String>, v: Option<&Value>, socket: &Path) -> Result<(), Error> {
    let x = object(
        v.ok_or_else(|| invalid("profile.tpm must be a mapping"))?,
        "profile.tpm",
    )?;
    let model = x.get("model").and_then(Value::as_str).unwrap_or("tpm-tis");
    if !["tpm-tis", "tpm-crb"].contains(&model) {
        return Err(invalid("profile.tpm.model must be tpm-tis or tpm-crb"));
    }
    argv.extend([
        "-chardev".into(),
        format!("socket,id=chrtpm,path={}", socket.display()),
        "-tpmdev".into(),
        "emulator,id=tpm0,chardev=chrtpm".into(),
        "-device".into(),
        format!("{model},tpmdev=tpm0"),
    ]);
    Ok(())
}
fn resolve_engine(
    release: &Value,
    bundle: &Path,
    track: &str,
    target: &str,
) -> Result<(Value, PathBuf), Error> {
    let release = object(release, "release set")?;
    if number(release, "schema_version")? != 1 {
        return Err(invalid("release set schema_version must be 1"));
    }
    let engines = object(
        release
            .get("engines")
            .ok_or_else(|| invalid("release set engines must be a mapping"))?,
        "engines",
    )?;
    let entry = object(
        engines
            .get(track)
            .ok_or_else(|| invalid(&format!("engine track is not in the release set: {track}")))?,
        "engine entry",
    )?;
    let relative_manifest = string(entry, "manifest")?;
    if Path::new(&relative_manifest).is_absolute() {
        return Err(invalid("engine manifest must be relative"));
    }
    let manifest_path = bundle.join(relative_manifest);
    let manifest_document = load_document(&manifest_path)?;
    let manifest = object(&manifest_document, "engine manifest")?;
    if number(manifest, "schema_version")? != 1 {
        return Err(invalid("engine manifest schema_version must be 1"));
    }
    if string(manifest, "track_id")? != track {
        return Err(invalid("manifest track does not match release track"));
    }
    if string(manifest, "build_digest")? != string(entry, "build_digest")? {
        return Err(invalid("engine build digest does not match release set"));
    }
    if manifest.get("dirty_source").and_then(Value::as_bool) != Some(false) {
        return Err(invalid("dirty engine builds cannot be used for release"));
    }
    let executables = object(
        manifest
            .get("executables")
            .ok_or_else(|| invalid("engine executables must be a mapping"))?,
        "executables",
    )?;
    let executable_relative = executables
        .get(target)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(&format!("engine has no executable for target {target}")))?;
    let executable = manifest_path
        .parent()
        .expect("manifest has a parent")
        .join(executable_relative);
    if !executable.is_file() {
        return Err(invalid(&format!(
            "engine executable is missing: {}",
            executable.display()
        )));
    }
    let empty_hashes = Value::Object(Map::new());
    let hashes = object(
        manifest.get("executable_sha256").unwrap_or(&empty_hashes),
        "executable_sha256",
    )?;
    if let Some(expected) = hashes.get(target).and_then(Value::as_str) {
        let data = fs::read(&executable).map_err(|source| Error::Io {
            path: executable.clone(),
            source,
        })?;
        if format!("{:x}", Sha256::digest(data)) != expected {
            return Err(invalid(&format!(
                "engine executable digest mismatch for {target}"
            )));
        }
    }
    Ok((
        serde_json::json!({"track_id": track, "build_digest": string(manifest, "build_digest")?}),
        executable,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_is_qemu_compatible() {
        assert_eq!(memory(&Value::from("8GiB")).unwrap(), "8G");
        assert!(memory(&Value::from("8GB")).is_err());
    }

    #[test]
    fn disk_sizes_normalize_for_qemu_img() {
        assert_eq!(disk_size("64GiB").unwrap(), "64G");
        assert_eq!(disk_size("1.5T").unwrap(), "1.5T");
        assert!(disk_size("64").is_err());
    }
    #[test]
    fn qemu_values_are_not_shell_commands() {
        assert_eq!(
            machine_value("pc", Some(&Value::Bool(true)), None).unwrap(),
            "pc,smm=on"
        );
    }

    #[test]
    fn qemu_help_parser_ignores_descriptions_and_keeps_order() {
        let help = "Supported machines are:\npc-q35-10.2  Q35 machine\npc  Standard PC\n\n";
        assert_eq!(parse_named_help(help), vec!["pc-q35-10.2", "pc"]);
    }

    #[test]
    fn qemu_help_parser_handles_option_sections() {
        let help = "-cpu cpu select CPU\n\nx86_64  host CPU\nmax  maximum CPU\n\nAccelerators:\nkvm\ntcg\n";
        assert_eq!(parse_named_help(help), vec!["x86_64", "max", "kvm", "tcg"]);
    }

    #[test]
    fn qemu_list_parser_stops_before_display_prose() {
        let help = "Available display backend types:\nnone\ngtk\nsdl\n\nSome display backends support suboptions, which can be set with\n";
        assert_eq!(parse_indented_list(help), vec!["none", "gtk", "sdl"]);
    }

    #[test]
    fn profile_validation_rejects_choices_missing_from_target_qemu() {
        let profile = serde_json::json!({
            "schema_version": 1,
            "id": "fixture",
            "machine": "pc-q35-10.2",
            "cpu": "max",
            "resources": {"accelerator": "kvm"}
        });
        let options = QemuOptions {
            executable: PathBuf::from("qemu-system-x86_64"),
            version: Some("QEMU emulator version 10.2.4".into()),
            machines: vec!["pc".into()],
            cpus: vec!["max".into()],
            accelerators: vec!["tcg".into()],
            devices: vec![],
            display_backends: vec![],
            chardev_backends: vec![],
            tpm_backends: vec![],
            audio_drivers: vec![],
            machine_properties: vec![],
            analysis_machine_properties: vec![],
            device_properties: vec![],
        };
        let error = validate_profile_against_qemu(&profile, &options).unwrap_err();
        assert!(error.to_string().contains("machine"));
    }

    #[test]
    fn profile_validation_checks_optional_qemu_version_pin() {
        let profile = serde_json::json!({
            "schema_version": 1,
            "id": "fixture",
            "engine": {"track": "fixture", "version": "10.2.4"},
            "machine": "pc",
        });
        let options = QemuOptions {
            executable: PathBuf::from("qemu"),
            version: Some("QEMU emulator version 10.2.3".into()),
            machines: vec!["pc".into()],
            cpus: vec![],
            accelerators: vec![],
            devices: vec![],
            display_backends: vec![],
            chardev_backends: vec![],
            tpm_backends: vec![],
            audio_drivers: vec![],
            machine_properties: vec![],
            analysis_machine_properties: vec![],
            device_properties: vec![],
        };
        let error = validate_profile_against_qemu(&profile, &options).unwrap_err();
        assert!(error.to_string().contains("10.2.4"));
    }

    #[test]
    fn profile_validation_checks_declared_devices() {
        let profile = serde_json::json!({
            "schema_version": 1,
            "id": "fixture",
            "machine": "q35",
            "resources": {"accelerator": "kvm"},
            "devices": {"nic": "missing-nic"},
            "tpm": {"model": "tpm-crb"}
        });
        let options = QemuOptions {
            executable: PathBuf::from("qemu-system-x86_64"),
            version: None,
            machines: vec!["q35".into()],
            cpus: vec![],
            accelerators: vec!["kvm".into()],
            devices: vec!["tpm-crb".into()],
            display_backends: vec![],
            chardev_backends: vec![],
            tpm_backends: vec![],
            audio_drivers: vec![],
            machine_properties: vec![],
            analysis_machine_properties: vec![],
            device_properties: vec![],
        };
        let error = validate_profile_against_qemu(&profile, &options).unwrap_err();
        assert!(error.to_string().contains("missing-nic"));
    }

    #[test]
    fn legacy_board_validation_checks_devices_and_analysis_properties() {
        let board = serde_json::json!({
            "os": {"type": {"machine": "q35"}},
            "video": {"model": {"type": "missing-video"}},
            "tpm": {"model": "missing-tpm"},
            "analysis": {"enabled": true}
        });
        let options = QemuOptions {
            executable: PathBuf::from("qemu"),
            version: None,
            machines: vec!["q35".into()],
            cpus: vec![],
            accelerators: vec![],
            devices: vec![],
            display_backends: vec![],
            chardev_backends: vec![],
            tpm_backends: vec![],
            audio_drivers: vec![],
            machine_properties: vec![],
            analysis_machine_properties: vec![],
            device_properties: vec![],
        };
        let report =
            validate_legacy_board(board.as_object().unwrap(), &options, Some("q35".into()));
        assert!(!report.valid);
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("video device"))
        );
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("analysis property"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn inspect_qemu_executes_the_selected_binary_for_each_capability() {
        use std::os::unix::fs::PermissionsExt;

        let root = test_root("inspect");
        fs::create_dir_all(&root).unwrap();
        let executable = root.join("qemu");
        fs::write(
            &executable,
            r#"#!/bin/sh
case "$1:$2" in
  --version:) echo 'QEMU emulator version 10.2.4';;
  -machine:help) printf 'Supported machines are:\npc-q35-10.2\npc\n';;
  -machine:pc-q35-10.2,help) printf 'smm=on\naccel=tcg\n';;
  -cpu:help) printf 'x86_64\nmax\n';;
  -accel:help) printf 'kvm\ntcg\n';;
  -device:help) printf 'name "virtio-net-pci", bus PCI\nname "ich9-ahci", bus PCI\n';;
  -device:virtio-net-pci,help) printf 'mac=address\nnetdev=id\n';;
  -display:help) printf 'gtk\nvnc\nnone\n';;
  -chardev:help) printf 'socket\nnull\n';;
  -tpmdev:help) printf 'emulator\npassthrough\n';;
  -audiodev:help) printf 'none\npa\n';;
  *) exit 1;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let options =
            inspect_qemu(&executable, Some("pc-q35-10.2"), Some("virtio-net-pci")).unwrap();
        assert_eq!(
            options.version.as_deref(),
            Some("QEMU emulator version 10.2.4")
        );
        assert_eq!(options.machines, vec!["pc-q35-10.2", "pc"]);
        assert_eq!(options.cpus, vec!["x86_64", "max"]);
        assert_eq!(options.accelerators, vec!["kvm", "tcg"]);
        assert_eq!(options.devices, vec!["virtio-net-pci", "ich9-ahci"]);
        assert_eq!(options.display_backends, vec!["gtk", "vnc", "none"]);
        assert_eq!(options.chardev_backends, vec!["socket", "null"]);
        assert_eq!(options.tpm_backends, vec!["emulator", "passthrough"]);
        assert_eq!(options.audio_drivers, vec!["none", "pa"]);
        assert_eq!(options.machine_properties, vec!["smm", "accel"]);
        assert_eq!(options.analysis_machine_properties, Vec::<String>::new());
        assert_eq!(options.device_properties, vec!["mac", "netdev"]);
        fs::remove_dir_all(root).unwrap();
    }

    fn tpm_fixture(root: &Path) -> (Value, Value, PathBuf) {
        let bundle = root.join("bundle");
        fs::create_dir_all(bundle.join("bin")).unwrap();
        fs::write(bundle.join("bin/qemu"), b"fixture").unwrap();
        fs::write(bundle.join("manifest.json"), r#"{
          "schema_version": 1, "track_id": "fixture", "build_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "source_revision": "test", "targets": ["x86_64-softmmu"], "executables": {"x86_64-softmmu": "bin/qemu"}, "dirty_source": false
        }"#).unwrap();
        let release = serde_json::json!({"schema_version":1,"engines":{"fixture":{"manifest":"manifest.json","build_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}});
        let profile = serde_json::json!({"schema_version":1,"id":"fixture","engine":{"track":"fixture"},"machine":"pc-q35-10.2","resources":{"memory":"512MiB","vcpus":2},"network":{"type":"disabled"},"tpm":{"model":"tpm-crb","backend":{"type":"emulator","version":"2.0"}}});
        (profile, release, bundle)
    }

    #[test]
    fn tpm_helper_defaults_to_swtpm_on_path() {
        let root = test_root("tpm-default");
        let (profile, release, bundle) = tpm_fixture(&root);
        let plan = build_plan(PlanInput {
            profile,
            release_set: release,
            bundle_root: bundle,
            asset_root: None,
            target: "x86_64-softmmu".into(),
            runtime_dir: root.join("runtime"),
            state_dir: None,
            seed: None,
            swtpm: None,
            bridge_helper: None,
        })
        .unwrap();
        let helper = plan.helper_argv.expect("a TPM profile needs its helper");
        assert_eq!(helper[0], "swtpm");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tpm_helper_uses_the_configured_swtpm() {
        let root = test_root("tpm-configured");
        let (profile, release, bundle) = tpm_fixture(&root);
        let plan = build_plan(PlanInput {
            profile,
            release_set: release,
            bundle_root: bundle,
            asset_root: None,
            target: "x86_64-softmmu".into(),
            runtime_dir: root.join("runtime"),
            state_dir: None,
            seed: None,
            swtpm: Some(PathBuf::from("/nix/store/fixture/bin/swtpm")),
            bridge_helper: None,
        })
        .unwrap();
        let helper = plan.helper_argv.expect("a TPM profile needs its helper");
        assert_eq!(helper[0], "/nix/store/fixture/bin/swtpm");
        assert!(helper.contains(&"socket".to_string()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bridge_networking_names_the_configured_helper() {
        let root = test_root("bridge-helper");
        let (mut profile, release, bundle) = tpm_fixture(&root);
        profile["network"] = serde_json::json!({"type":"bridge","bridge":"br0"});
        let plan = build_plan(PlanInput {
            profile,
            release_set: release,
            bundle_root: bundle,
            asset_root: None,
            target: "x86_64-softmmu".into(),
            runtime_dir: root.join("runtime"),
            state_dir: None,
            seed: None,
            swtpm: None,
            bridge_helper: Some(PathBuf::from("/run/wrappers/bin/qemu-bridge-helper")),
        })
        .unwrap();
        assert!(
            plan.argv.contains(
                &"bridge,id=net0,br=br0,helper=/run/wrappers/bin/qemu-bridge-helper".to_string()
            ),
            "unexpected argv: {:?}",
            plan.argv
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bridge_networking_without_a_helper_leaves_qemu_its_default() {
        let root = test_root("bridge-default");
        let (mut profile, release, bundle) = tpm_fixture(&root);
        profile["network"] = serde_json::json!({"type":"bridge","bridge":"br0"});
        let plan = build_plan(PlanInput {
            profile,
            release_set: release,
            bundle_root: bundle,
            asset_root: None,
            target: "x86_64-softmmu".into(),
            runtime_dir: root.join("runtime"),
            state_dir: None,
            seed: None,
            swtpm: None,
            bridge_helper: None,
        })
        .unwrap();
        assert!(
            plan.argv.contains(&"bridge,id=net0,br=br0".to_string()),
            "unexpected argv: {:?}",
            plan.argv
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plan_resolves_a_fixture_engine_without_writing_runtime_state() {
        let root = test_root("plan");
        let bundle = root.join("bundle");
        fs::create_dir_all(bundle.join("bin")).unwrap();
        fs::write(bundle.join("bin/qemu"), b"fixture").unwrap();
        fs::write(bundle.join("manifest.json"), r#"{
          "schema_version": 1, "track_id": "fixture", "build_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "source_revision": "test", "targets": ["x86_64-softmmu"], "executables": {"x86_64-softmmu": "bin/qemu"}, "dirty_source": false
        }"#).unwrap();
        let release = serde_json::json!({"schema_version":1,"engines":{"fixture":{"manifest":"manifest.json","build_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}});
        let profile = serde_json::json!({"schema_version":1,"id":"fixture","engine":{"track":"fixture"},"machine":"pc-q35-10.2","resources":{"memory":"512MiB","vcpus":2},"network":{"type":"disabled"}});
        let runtime = root.join("runtime");
        let plan = build_plan(PlanInput {
            profile,
            release_set: release,
            bundle_root: bundle,
            asset_root: None,
            target: "x86_64-softmmu".into(),
            runtime_dir: runtime.clone(),
            state_dir: None,
            seed: None,
            swtpm: None,
            bridge_helper: None,
        })
        .unwrap();
        assert!(plan.argv[0].ends_with("bin/qemu"));
        assert!(plan.argv.windows(2).any(|pair| pair == ["-nic", "none"]));
        assert!(
            !runtime.exists(),
            "planning must not create runtime directories"
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn test_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("machineemu-plan-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }
}
