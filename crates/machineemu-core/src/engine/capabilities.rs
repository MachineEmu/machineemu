use super::{
    Error,
    plan::{invalid, object, string},
};
use serde::Serialize;
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

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

pub(super) fn validate_legacy_board(
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
        && !options
            .accelerators
            .iter()
            .any(|value| value == accelerator)
    {
        errors.push(format!("QEMU does not support accelerator {accelerator:?}"));
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
        && audio != "none"
    {
        let device = if audio == "ich9" {
            "ich9-intel-hda"
        } else {
            audio
        };
        if !options.devices.iter().any(|value| value == device) {
            errors.push(format!("QEMU does not support audio device {device:?}"));
        }
    }
    if let Some(tpm) = board
        .get("tpm")
        .and_then(|value| value.get("model"))
        .and_then(Value::as_str)
        && !options.devices.iter().any(|value| value == tpm)
    {
        errors.push(format!("QEMU does not support TPM device {tpm:?}"));
    }
    if let Some(bus) = board
        .get("storage")
        .and_then(|value| value.get("disk"))
        .and_then(|value| value.get("target"))
        .and_then(|value| value.get("bus"))
        .and_then(Value::as_str)
        && bus == "sata"
        && !options.devices.iter().any(|value| value == "ich9-ahci")
    {
        errors.push("QEMU does not support the SATA controller ich9-ahci".into());
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
        if devices.get("vsock").and_then(Value::as_bool) == Some(true)
            && !options
                .devices
                .iter()
                .any(|device| device == "vhost-vsock-pci")
        {
            return Err(invalid(
                "QEMU does not support vsock device vhost-vsock-pci",
            ));
        }
        if devices.get("lcd").and_then(Value::as_bool) == Some(true) {
            for device in ["qemu-xhci", "unifi-lcm"] {
                if !options.devices.iter().any(|v| v == device) {
                    return Err(invalid(&format!(
                        "QEMU does not support LCD device {device}"
                    )));
                }
            }
        }
        if let Some(nic) = devices.get("nic").and_then(Value::as_str)
            && !options.devices.iter().any(|item| item == nic)
        {
            return Err(invalid(&format!(
                "QEMU {} does not support NIC device {nic:?}",
                options.executable.display()
            )));
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
        && !options.devices.iter().any(|item| item == tpm)
    {
        return Err(invalid(&format!(
            "QEMU {} does not support TPM device {tpm:?}",
            options.executable.display()
        )));
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

pub(super) fn parse_named_help(text: &str) -> Vec<String> {
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

pub(super) fn parse_indented_list(text: &str) -> Vec<String> {
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
