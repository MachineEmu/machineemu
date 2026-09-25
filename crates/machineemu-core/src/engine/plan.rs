use super::{Error, analysis, load_document};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct PlanInput {
    pub profile: Value,
    pub release_set: Value,
    pub bundle_root: PathBuf,
    pub asset_root: Option<PathBuf>,
    pub image_components: BTreeMap<String, PathBuf>,
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
    /// An exact NIC address. It outranks the profile's own `devices.mac` and
    /// the address derived from `instance`.
    pub mac: Option<String>,
    /// The instance this plan is for. Without a declared address, its NIC
    /// address is derived from this name.
    pub instance: Option<String>,
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

pub fn build_plan(input: PlanInput) -> Result<LaunchPlan, Error> {
    let profile = object(&input.profile, "profile")?;
    if number(profile, "schema_version")? != 2 {
        return Err(Error::Invalid("profile schema_version must be 2".into()));
    }
    let profile_id = string(profile, "id")?;
    let machine = string(profile, "machine")?;
    validate_launch_settings(profile)?;
    let target = &input.target;
    let tracks = engine_tracks(profile)?;
    let (track, engine, executable) =
        resolve_engine_ordered(&input.release_set, &input.bundle_root, &tracks, target)?;
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
    let cpu =
        profile
            .get("cpu")
            .and_then(Value::as_str)
            .unwrap_or(if profile.contains_key("analysis") {
                "host,kvm=off"
            } else {
                "max"
            });
    let analysis = analysis::plan(profile, &machine, target, vcpus)?;
    let mut machine_arg = machine_value(&machine, profile.get("smm"), None)?;
    if let Some(analysis) = &analysis {
        machine_arg.push(',');
        machine_arg.push_str(&analysis.machine_suffix);
    }
    let qmp = runtime.join("sockets/qmp.sock");
    let pidfile = runtime.join("control/qemu.pid");
    let mut argv = vec![
        executable.to_string_lossy().into_owned(),
        "-machine".into(),
        machine_arg.clone(),
    ];
    if machine.contains("q35") {
        // PCI devices cannot be hotplugged directly on pcie.0. Keep stable
        // downstream ports available for live ISO and NIC operations.
        argv.extend([
            "-device".into(),
            "pcie-root-port,id=pcie-root-port-iso,chassis=16,slot=16".into(),
            "-device".into(),
            "pcie-root-port,id=pcie-root-port-1,chassis=17,slot=17".into(),
            "-device".into(),
            "pcie-root-port,id=pcie-root-port-2,chassis=18,slot=18".into(),
        ]);
    }
    if let Some(accel) = resources.get("accelerator").and_then(Value::as_str) {
        argv.extend(accelerator(accel, resources.get("accelerator_thread"))?);
    }
    if machine != "udm-pro" || profile.contains_key("cpu") {
        argv.extend(["-cpu".into(), cpu.into()]);
    }
    argv.extend(["-m".into(), memory, "-smp".into(), smp(resources, vcpus)?]);
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
    // An explicit address wins, then one the profile declares -- a cloned
    // identity carries its own -- and otherwise the instance name decides.
    let address = input
        .mac
        .or_else(|| {
            profile
                .get("devices")
                .and_then(|devices| devices.get("mac"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| input.instance.as_deref().map(derive_mac));
    if machine == "udm-pro" {
        append_udm_network(
            &mut argv,
            profile.get("network"),
            bridge_helper.as_deref(),
            address.as_deref(),
        )?;
    } else {
        append_network(
            &mut argv,
            profile.get("network"),
            nic,
            bridge_helper.as_deref(),
            address.as_deref(),
        )?;
    }
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
    let sources = SourceResolver {
        components: &input.image_components,
        base_dir: input.asset_root.as_deref(),
    };
    let mut prep = Preparation::default();
    append_firmware(
        &mut argv,
        &mut prep,
        profile.get("firmware"),
        &sources,
        &state,
    )?;
    if machine == "udm-pro" {
        for role in ["kernel", "initrd"] {
            if profile
                .get("boot")
                .and_then(|boot| boot.get(role))
                .is_none_or(Value::is_null)
            {
                return Err(invalid(&format!("UDM Pro requires profile.boot.{role}")));
            }
        }
    }
    append_direct_boot(&mut argv, profile.get("boot"), &sources)?;
    // Analysis device identity descriptors, forged onto the disk and VGA below.
    // They borrow the analysis plan, so this must precede its later move.
    let storage_desc = analysis
        .as_ref()
        .and_then(|a| a.payload.pointer("/analysis/device_descriptors/storage"));
    let display_desc = analysis
        .as_ref()
        .and_then(|a| a.payload.pointer("/analysis/device_descriptors/display"));
    if machine == "udm-pro" {
        append_udm_storage(&mut argv, profile.get("storage"), &sources)?;
    } else {
        append_storage(
            &mut argv,
            &mut prep,
            profile.get("storage"),
            &sources,
            &machine,
            &state,
            storage_desc,
        )?;
    }
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
    let vsock_cid = profile
        .get("devices")
        .and_then(|devices| devices.get("vsock"))
        .and_then(Value::as_bool)
        .filter(|enabled| *enabled)
        .map(|_| derive_vsock_cid(&runtime, input.instance.as_deref()));
    if let Some(cid) = vsock_cid {
        argv.extend([
            "-device".into(),
            format!("vhost-vsock-pci,id=machineemu-vsock,guest-cid={cid}"),
        ]);
    }
    append_audio(
        &mut argv,
        profile
            .get("audio")
            .or_else(|| profile.get("devices").and_then(|d| d.get("audio"))),
        profile.get("devices"),
        &runtime,
    )?;
    append_analysis_video(
        &mut argv,
        profile.get("devices").and_then(|d| d.get("video")),
        display_desc,
    )?;
    if display_desc.is_none()
        && profile
            .get("devices")
            .and_then(|d| d.get("video"))
            .and_then(|v| v.get("model"))
            .and_then(Value::as_str)
            == Some("vga")
    {
        argv.extend(["-vga".into(), "std".into()]);
    }
    if machine == "udm-pro" {
        append_udm_devices(&mut argv, profile.get("devices"), &runtime)?;
    } else if machine == "us24pro" {
        argv.extend([
            "-chardev".into(),
            format!(
                "socket,id=frontpanel-events,path={},server=on,wait=off",
                runtime
                    .join("frontpanel-events.sock")
                    .display()
                    .to_string()
                    .replace(',', ",,")
            ),
            "-global".into(),
            "unifi-board.frontpanel=frontpanel-events".into(),
        ]);
    }
    append_wifi(&mut argv, profile.get("wifi"), &machine, &runtime)?;
    if let Some(analysis) = &analysis {
        argv.extend(analysis.argv.clone());
    }
    let mut manifest = serde_json::json!({"schema_version":1,"profile_id":profile_id,"target":target,"machine":machine,"machine_argument":machine_arg,"engine":engine,"engine_track":track,"resources":{"memory":argv[argv.iter().position(|x|x=="-m").unwrap()+1],"vcpus":vcpus},"qmp_socket":qmp,"pidfile":pidfile,"image_components":input.image_components});
    if let Some(analysis) = analysis {
        manifest["analysis_argv"] = serde_json::json!(analysis.argv);
        manifest["analysis_payload"] = analysis.payload;
    }
    if let Some(cid) = vsock_cid {
        manifest["vsock"] = serde_json::json!({"guest_cid":cid});
    }
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

fn validate_launch_settings(profile: &Map<String, Value>) -> Result<(), Error> {
    if let Some(boot) = profile.get("boot") {
        let boot = object(boot, "profile.boot")?;
        for key in boot.keys() {
            if ![
                "kernel",
                "initrd",
                "dtb",
                "append",
                "menu",
                "from",
                "once",
                "strict",
                "reboot_timeout",
                "splash",
                "timeout",
            ]
            .contains(&key.as_str())
            {
                return Err(invalid(&format!(
                    "profile.boot.{key} is not supported by the launch planner"
                )));
            }
        }
        if boot.get("from").and_then(Value::as_str) == Some("disk")
            && profile
                .get("storage")
                .and_then(|storage| storage.get("disk"))
                .is_none()
        {
            return Err(invalid(
                "profile.boot.from=disk requires profile.storage.disk",
            ));
        }
    }
    let Some(devices) = profile.get("devices") else {
        return Ok(());
    };
    let devices = object(devices, "profile.devices")?;
    for key in devices.keys() {
        if ![
            "nic",
            "mac",
            "console",
            "video",
            "audio",
            "usb_tablet",
            "usb_mouse",
            "guest_agent",
            "serial",
            "vsock",
            "snapshots",
            "usb",
            "lcd",
            "bluetooth",
        ]
        .contains(&key.as_str())
        {
            return Err(invalid(&format!(
                "profile.devices.{key} is not supported by the launch planner"
            )));
        }
    }
    if devices
        .get("vsock")
        .is_some_and(|vsock| !vsock.is_boolean())
    {
        return Err(invalid("profile.devices.vsock must be a boolean"));
    }
    let console = devices
        .get("console")
        .map(|v| object(v, "profile.devices.console"))
        .transpose()?;
    let console_type = console
        .and_then(|console| console.get("type").and_then(Value::as_str))
        .unwrap_or("none");
    if !matches!(console_type, "none" | "vnc" | "h264" | "serial") {
        return Err(invalid(
            "profile.devices.console.type must be none, vnc, h264, or serial",
        ));
    }
    if devices.get("guest_agent").is_some_and(|v| !v.is_boolean()) {
        return Err(invalid("profile.devices.guest_agent must be a boolean"));
    }
    if let Some(serial) = devices.get("serial") {
        let serial = object(serial, "profile.devices.serial")?;
        let kind = serial.get("type").and_then(Value::as_str).unwrap_or("none");
        if !matches!(kind, "none" | "file" | "socket" | "stdout") {
            return Err(invalid(
                "profile.devices.serial.type must be none, file, socket, or stdout",
            ));
        }
    }
    if devices
        .get("nic")
        .is_some_and(|v| v.as_str().is_none_or(|s| s.is_empty() || s.contains(',')))
    {
        return Err(invalid("profile.devices.nic must be a QEMU device name"));
    }
    if let Some(address) = devices.get("mac") {
        mac(address
            .as_str()
            .ok_or_else(|| invalid("profile.devices.mac must be a string"))?)?;
    }
    let video = devices
        .get("video")
        .map(|v| object(v, "profile.devices.video"))
        .transpose()?;
    if let Some(video) = video {
        for key in video.keys() {
            if !["model", "heads", "primary"].contains(&key.as_str()) {
                return Err(invalid(&format!(
                    "profile.devices.video.{key} is not supported"
                )));
            }
        }
        if video.get("heads").is_some_and(|v| v.as_u64() != Some(1)) {
            return Err(invalid("profile.devices.video.heads only supports 1"));
        }
        if video
            .get("primary")
            .is_some_and(|v| v.as_bool() != Some(true))
        {
            return Err(invalid("profile.devices.video.primary only supports true"));
        }
        let model = video
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("profile.devices.video.model is required"))?;
        if console_type == "vnc" && matches!(model, "virtio-vga-gl" | "virtio-gpu-gl") {
            return Err(invalid(
                "profile.devices.console=vnc conflicts with GL video",
            ));
        }
        if !matches!(
            model,
            "vga" | "virtio-vga" | "virtio-vga-gl" | "virtio-gpu-gl"
        ) {
            return Err(invalid(&format!(
                "profile.devices.video.model {model:?} is not supported by the launch planner; supported models: vga, virtio-vga, virtio-vga-gl, virtio-gpu-gl"
            )));
        }
        if matches!(model, "virtio-vga-gl" | "virtio-gpu-gl") && console_type != "h264" {
            return Err(invalid(
                "GL video requires profile.devices.console.type=h264",
            ));
        }
    }
    Ok(())
}

fn append_wifi(
    argv: &mut Vec<String>,
    wifi: Option<&Value>,
    machine: &str,
    runtime: &Path,
) -> Result<(), Error> {
    let Some(wifi) = wifi else {
        return Ok(());
    };
    let wifi = object(wifi, "profile.wifi")?;
    if wifi.get("enabled") != Some(&Value::Bool(true)) {
        return Ok(());
    }
    if machine != "mt7981" {
        return Err(invalid("profile.wifi requires the mt7981 machine"));
    }
    argv.extend([
        "-global".into(),
        format!(
            "unifi-board.wifi-socket={}",
            runtime
                .join("wifi.sock")
                .display()
                .to_string()
                .replace(',', ",,")
        ),
    ]);
    Ok(())
}

#[cfg(test)]
mod wifi_plan_tests {
    use super::*;
    #[test]
    fn mt7981_wifi_binds_an_instance_socket_only_when_enabled() {
        let mut argv = Vec::new();
        append_wifi(
            &mut argv,
            Some(&serde_json::json!({"enabled":true})),
            "mt7981",
            Path::new("/tmp/lab"),
        )
        .unwrap();
        assert_eq!(
            argv,
            ["-global", "unifi-board.wifi-socket=/tmp/lab/wifi.sock"]
        );
        assert!(
            append_wifi(
                &mut Vec::new(),
                Some(&serde_json::json!({"enabled":true})),
                "udm-pro",
                Path::new("/tmp/lab")
            )
            .is_err()
        );
    }
}

pub(super) fn invalid(s: &str) -> Error {
    Error::Invalid(s.into())
}
pub(super) fn object<'a>(v: &'a Value, name: &str) -> Result<&'a Map<String, Value>, Error> {
    v.as_object()
        .ok_or_else(|| invalid(&format!("{name} must be a mapping")))
}
pub(super) fn string(v: &Map<String, Value>, key: &str) -> Result<String, Error> {
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
pub(super) fn memory(v: &Value) -> Result<String, Error> {
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
pub(super) fn machine_value(
    machine: &str,
    smm: Option<&Value>,
    analysis: Option<&Value>,
) -> Result<String, Error> {
    let mut m = machine.to_owned();
    if smm.and_then(Value::as_bool).unwrap_or(false) {
        m.push_str(",smm=on")
    }
    if analysis.is_some() {
        return Err(invalid("analysis must be planned with its profile context"));
    }
    Ok(m)
}
/// Derive a stable NIC address for one instance.
///
/// QEMU hands every guest the same 52:54:00:12:34:56 unless it is told
/// otherwise, so two instances on one bridge answer for each other's traffic
/// and the second lease replaces the first. The 52:54:00 prefix is kept -- it
/// is locally administered and unicast, and it still reads as a QEMU guest on
/// the wire -- and the remaining three bytes come from the instance name, so
/// an instance keeps its address across a rebuild and two instances differ.
pub fn derive_mac(instance: &str) -> String {
    let digest = Sha256::digest(instance.as_bytes());
    format!(
        "52:54:00:{:02x}:{:02x}:{:02x}",
        digest[0], digest[1], digest[2]
    )
}

fn derive_vsock_cid(runtime: &Path, instance: Option<&str>) -> u32 {
    // Linux reserves CIDs 0, 1 and 2. Include the runtime path so instances
    // with the same name in separate workspaces normally receive different CIDs.
    let mut hasher = Sha256::new();
    hasher.update(b"machineemu-vsock-cid-v1\0");
    hasher.update(runtime.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    if let Some(instance) = instance {
        hasher.update(instance.as_bytes());
    }
    let digest = hasher.finalize();
    let candidate = u32::from_be_bytes(digest[..4].try_into().expect("SHA-256 has four bytes"));
    3 + (u64::from(candidate) % (u64::from(u32::MAX) - 2)) as u32
}

fn mac(value: &str) -> Result<String, Error> {
    let octets: Vec<&str> = value.split(':').collect();
    let valid = octets.len() == 6
        && octets
            .iter()
            .all(|octet| octet.len() == 2 && octet.chars().all(|c| c.is_ascii_hexdigit()));
    if !valid {
        return Err(invalid(&format!(
            "mac address must be six colon-separated hex octets: {value:?}"
        )));
    }
    let first = u8::from_str_radix(octets[0], 16).map_err(|_| invalid("mac address is not hex"))?;
    // A multicast address is accepted by QEMU and then ignored by every switch
    // on the path, which looks like a guest that never got a lease.
    if first & 1 == 1 {
        return Err(invalid(&format!(
            "mac address {value:?} is multicast; the low bit of the first octet must be clear"
        )));
    }
    Ok(value.to_ascii_lowercase())
}

fn append_network(
    argv: &mut Vec<String>,
    v: Option<&Value>,
    nic: Option<&str>,
    bridge_helper: Option<&Path>,
    address: Option<&str>,
) -> Result<(), Error> {
    let mac = match address {
        Some(value) => format!(",mac={}", mac(value)?),
        None => String::new(),
    };
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
                format!("{model},netdev=net0{mac}"),
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
                format!("{model},netdev=net0{mac}"),
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

pub(super) fn append_devices(
    argv: &mut Vec<String>,
    v: Option<&Value>,
    runtime: &Path,
) -> Result<(), Error> {
    let empty = Value::Object(Map::new());
    let devices = object(v.unwrap_or(&empty), "profile.devices")?;
    let console = devices
        .get("console")
        .map(|value| object(value, "profile.devices.console"))
        .transpose()?;
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
    let serial_mode = devices
        .get("serial")
        .and_then(|serial| serial.get("type"))
        .and_then(Value::as_str);
    match serial_mode {
        Some("socket") => {
            let socket = runtime
                .join("serial.sock")
                .display()
                .to_string()
                .replace(',', ",,");
            let log = runtime
                .join("serial.log")
                .display()
                .to_string()
                .replace(',', ",,");
            argv.extend([
                "-chardev".into(),
                format!(
                    "socket,id=uart0,path={socket},server=on,wait=off,logfile={log},logappend=off"
                ),
                "-serial".into(),
                "chardev:uart0".into(),
            ]);
        }
        Some("file") => argv.extend([
            "-serial".into(),
            format!("file:{}", runtime.join("serial.log").display()),
        ]),
        Some("stdout") => argv.extend(["-serial".into(), "stdio".into()]),
        _ => {}
    }
    let console_type = console
        .and_then(|console| console.get("type").and_then(Value::as_str))
        .unwrap_or("none");
    let vnc = console_type == "vnc";
    let h264 = console_type == "h264";
    let usb_tablet = match devices.get("usb_tablet") {
        None => false,
        Some(Value::Bool(value)) => *value,
        _ => return Err(invalid("profile.devices.usb_tablet must be a boolean")),
    };
    let usb_mouse = match devices.get("usb_mouse") {
        None => false,
        Some(Value::Bool(value)) => *value,
        _ => return Err(invalid("profile.devices.usb_mouse must be a boolean")),
    };
    let video_model = devices
        .get("video")
        .and_then(|video| video.get("model"))
        .and_then(Value::as_str);
    if video_model == Some("virtio-vga") {
        argv.extend([
            "-vga".into(),
            "none".into(),
            "-device".into(),
            "virtio-vga,id=me-video".into(),
        ]);
    }
    if vnc && matches!(video_model, Some("virtio-vga-gl" | "virtio-gpu-gl")) {
        return Err(invalid(
            "profile.devices.console.type=vnc conflicts with GL video",
        ));
    }
    if usb_tablet || usb_mouse {
        // Q35 does not necessarily create a usable USB bus by itself. Share
        // one explicit xHCI controller across the requested USB pointers.
        argv.extend(["-device".into(), "qemu-xhci,id=usb".into()]);
        if usb_tablet {
            argv.extend(["-device".into(), "usb-tablet,bus=usb.0".into()]);
        }
        if usb_mouse {
            argv.extend(["-device".into(), "usb-mouse,bus=usb.0".into()]);
        }
    }
    if h264 {
        if !matches!(video_model, Some("virtio-vga-gl" | "virtio-gpu-gl")) {
            return Err(invalid(
                "profile.devices.console.type=h264 requires devices.video.model=virtio-vga-gl or virtio-gpu-gl",
            ));
        }
        argv.extend([
            "-vga".into(),
            "none".into(),
            "-device".into(),
            format!("{},id=me-video", video_model.unwrap()),
        ]);
        argv.extend(["-display".into(), "dbus,p2p=on,gl=on".into()]);
        if vnc {
            argv.extend([
                "-vnc".into(),
                format!("unix:{}", runtime.join("sockets/vnc.sock").display()),
            ]);
        }
    } else if vnc {
        argv.extend(["-display".into(), "vnc=:0".into()]);
    } else {
        argv.extend(["-display".into(), "none".into()]);
    }
    Ok(())
}

pub(super) fn append_audio(
    argv: &mut Vec<String>,
    audio: Option<&Value>,
    devices: Option<&Value>,
    runtime: &Path,
) -> Result<(), Error> {
    let Some(audio) = audio else {
        return Ok(());
    };
    let audio = object(audio, "profile.audio")?;
    let model = audio.get("model").and_then(Value::as_str).unwrap_or("ich9");
    if model == "none" {
        return Ok(());
    }
    if model != "ich9" {
        return Err(invalid("profile.audio.model must be ich9 or none"));
    }
    let backend = audio
        .get("backend")
        .ok_or_else(|| invalid("profile.audio.backend is required"))?;
    let backend = object(backend, "profile.audio.backend")?;
    let kind = backend
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("none");
    if kind == "none" {
        return Ok(());
    }
    if !matches!(kind, "dbus" | "spice") {
        return Err(invalid(
            "profile.audio.backend.type must be dbus, spice, or none",
        ));
    }
    let id = backend
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("pc-audio");
    if id.is_empty()
        || id.len() > 48
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(invalid("profile.audio.backend.id is invalid"));
    }
    if kind == "dbus" {
        let display = argv
            .windows(2)
            .position(|pair| pair[0] == "-display")
            .ok_or_else(|| invalid("audio requires a display backend"))?
            + 1;
        if argv[display].starts_with("dbus,") {
            argv[display].push_str(&format!(",audiodev={id}"));
        } else {
            argv[display] = format!("dbus,p2p=on,audiodev={id}");
            let vnc = devices
                .and_then(|d| d.get("console"))
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str)
                == Some("vnc");
            if vnc {
                argv.extend([
                    "-vnc".into(),
                    format!("unix:{}", runtime.join("sockets/vnc.sock").display()),
                ]);
            }
        }
    } else {
        argv.extend([
            "-spice".into(),
            format!(
                "disable-ticketing=on,unix=on,addr={}",
                runtime.join("sockets/spice.sock").display()
            ),
        ]);
    }
    argv.extend([
        "-audiodev".into(),
        format!("{kind},id={id}"),
        "-device".into(),
        "ich9-intel-hda,id=pc-sound".into(),
        "-device".into(),
        format!("hda-duplex,audiodev={id}"),
    ]);
    Ok(())
}

/// The forged display adapter for an analysis profile. QEMU's auto-added VGA
/// answers EDID and PCI config with QEMU's own IDs (1234:1111), and an
/// auto-added device takes no properties, so it is suppressed with `-vga none`
/// and replaced by an explicit VGA carrying the profile's monitor identity.
/// `pci_identity` additionally forges the adapter's own PCI vendor/device IDs;
/// the delay lets the guest first enumerate a plain QEMU adapter, matching a
/// real driver that reprograms identity after it loads (patches 0007, 0009).
fn append_analysis_video(
    argv: &mut Vec<String>,
    video: Option<&Value>,
    display: Option<&Value>,
) -> Result<(), Error> {
    let Some(display) = display else {
        return Ok(());
    };
    // Only the emulated VGA takes these properties; virtio-gpu and passthrough
    // do not, and "none" means the profile wants no adapter forged.
    let model = video
        .and_then(|v| v.get("type"))
        .or_else(|| video.and_then(|v| v.get("model")))
        .and_then(Value::as_str)
        .unwrap_or("vga");
    if model != "vga" {
        return Ok(());
    }
    let mut device = String::from("VGA,id=pc-video");
    let string_prop = |device: &mut String, key: &str, prop: &str| {
        if let Some(value) = display.get(key).and_then(Value::as_str) {
            device.push_str(&format!(",{prop}={}", escape_prop(value)));
        }
    };
    let int_prop = |device: &mut String, key: &str, prop: &str| {
        if let Some(value) = display.get(key).and_then(Value::as_u64) {
            device.push_str(&format!(",{prop}={value}"));
        }
    };
    string_prop(&mut device, "vendor", "vendor");
    string_prop(&mut device, "name", "name");
    string_prop(&mut device, "serial", "serial");
    // xmax/ymax bound the reported mode, and the preferred mode matches it.
    for (key, prop) in [
        ("xres", "xres"),
        ("yres", "yres"),
        ("xres", "xmax"),
        ("yres", "ymax"),
        ("width_mm", "width-mm"),
        ("height_mm", "height-mm"),
    ] {
        int_prop(&mut device, key, prop);
    }
    if display.get("pci_identity").and_then(Value::as_bool) == Some(true) {
        for (key, prop) in [
            ("pci_vendor_id", "pci-vendor-id"),
            ("pci_device_id", "pci-device-id"),
            ("pci_subsystem_vendor_id", "pci-subsystem-vendor-id"),
            ("pci_subsystem_id", "pci-subsystem-id"),
            ("pci_identity_delay_ms", "pci-identity-delay-ms"),
        ] {
            int_prop(&mut device, key, prop);
        }
    }
    argv.extend(["-vga".into(), "none".into(), "-device".into(), device]);
    Ok(())
}

struct SourceResolver<'a> {
    components: &'a BTreeMap<String, PathBuf>,
    base_dir: Option<&'a Path>,
}

impl SourceResolver<'_> {
    fn path(&self, v: Option<&Value>, where_: &str) -> Result<PathBuf, Error> {
        let value = object(
            v.ok_or_else(|| invalid(&format!("{where_}.source is required")))?,
            &format!("{where_}.source"),
        )?;
        for key in value.keys() {
            if !["path", "image_component"].contains(&key.as_str()) {
                return Err(invalid(&format!("{where_}.source.{key} is not supported")));
            }
        }
        let raw_path = value.get("path").and_then(Value::as_str);
        let component = value.get("image_component").and_then(Value::as_str);
        match (raw_path, component) {
            (Some(_), Some(_)) => Err(invalid(&format!(
                "{where_}.source must choose path or image_component"
            ))),
            (Some(path), None) => {
                let path = PathBuf::from(path);
                let path = if path.is_absolute() {
                    path
                } else if let Some(base) = self.base_dir {
                    base.join(path)
                } else {
                    path
                };
                if !path.is_file() {
                    return Err(invalid(&format!(
                        "{where_}.source.path is unavailable: {}",
                        path.display()
                    )));
                }
                Ok(path)
            }
            (None, Some(component)) => self.components.get(component).cloned().ok_or_else(|| {
                invalid(&format!(
                    "{where_}.source.image_component is unavailable: {component}"
                ))
            }),
            (None, None) => Err(invalid(&format!(
                "{where_}.source must include path or image_component"
            ))),
        }
    }
}

fn append_firmware(
    argv: &mut Vec<String>,
    prep: &mut Preparation,
    v: Option<&Value>,
    sources: &SourceResolver<'_>,
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
    let code = sources.path(loader.get("source"), "profile.firmware.loader")?;
    let seed = sources.path(nvram.get("source"), "profile.firmware.nvram")?;
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
/// Escape a QEMU option value: a comma is the option separator, so it is
/// doubled. Analysis identity strings ("SATA SSD", "DELL P2419H") have none,
/// but a profile-supplied serial might.
fn escape_prop(value: &str) -> String {
    value.replace(',', ",,")
}

fn append_storage(
    argv: &mut Vec<String>,
    prep: &mut Preparation,
    v: Option<&Value>,
    sources: &SourceResolver<'_>,
    machine: &str,
    state: &Path,
    storage_desc: Option<&Value>,
) -> Result<(), Error> {
    let Some(v) = v else { return Ok(()) };
    let x = object(v, "profile.storage")?;
    for key in x.keys() {
        if key != "disk" {
            return Err(invalid(&format!(
                "profile.storage.{key} is not supported for this machine"
            )));
        }
    }
    let disk = object(
        x.get("disk")
            .ok_or_else(|| invalid("profile.storage.disk must be a mapping"))?,
        "profile.storage.disk",
    )?;
    for key in disk.keys() {
        if ![
            "source",
            "bus",
            "format",
            "size",
            "readonly",
            "cache",
            "aio",
            "discard",
            "detect_zeroes",
            "serial",
        ]
        .contains(&key.as_str())
        {
            return Err(invalid(&format!(
                "profile.storage.disk.{key} is not supported by the launch planner"
            )));
        }
    }
    let backing = sources.path(disk.get("source"), "profile.storage.disk")?;
    if disk.get("bus").is_some_and(|v| !v.is_string()) {
        return Err(invalid("profile.storage.disk.bus must be a string"));
    }
    if disk.get("format").is_some_and(|v| !v.is_string()) {
        return Err(invalid("profile.storage.disk.format must be raw or qcow2"));
    }
    if disk.get("readonly").is_some_and(|v| !v.is_boolean()) {
        return Err(invalid("profile.storage.disk.readonly must be a boolean"));
    }
    let bus = disk.get("bus").and_then(Value::as_str).unwrap_or("sata");
    if bus == "virtio" && disk.contains_key("serial") {
        return Err(invalid(
            "profile.storage.disk.serial is unsupported on virtio",
        ));
    }
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
    for (field, option) in [
        ("cache", "cache"),
        ("aio", "aio"),
        ("discard", "discard"),
        ("detect_zeroes", "detect-zeroes"),
    ] {
        if let Some(value) = disk.get(field) {
            let value = value
                .as_str()
                .filter(|value| !value.is_empty() && !value.contains([',', '\n', '\r']))
                .ok_or_else(|| invalid(&format!("profile.storage.disk.{field} is invalid")))?;
            opt.push_str(&format!(",{option}={value}"));
        }
    }
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
    let mut device = match bus {
        "virtio" => "virtio-blk-pci,drive=pc-disk".to_string(),
        "ide" if legacy => "ide-hd,bus=ide.0,drive=pc-disk".to_string(),
        "sata" if !legacy => "ide-hd,bus=pc-sata.0,drive=pc-disk".to_string(),
        _ => return Err(invalid("unsupported storage bus for selected machine")),
    };
    // Analysis storage descriptors forge the ATA identity so the guest reads a
    // real drive's model and serial instead of QEMU's "QEMU HARDDISK"/"QM00005".
    // virtio-blk has no model/serial to answer with, so it carries neither. An
    // explicit profile serial still wins over the derived one.
    if bus != "virtio" {
        if let Some(model) = storage_desc
            .and_then(|d| d.get("disk_product"))
            .and_then(Value::as_str)
        {
            device.push_str(&format!(",model={}", escape_prop(model)));
        }
        let serial = disk
            .get("serial")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                storage_desc
                    .and_then(|d| d.get("disk_serial_prefix"))
                    .and_then(Value::as_str)
                    .map(|prefix| format!("{prefix}-0"))
            });
        if let Some(serial) = serial {
            device.push_str(&format!(",serial={}", escape_prop(&serial)));
        }
    }
    argv.extend(["-drive".into(), opt, "-device".into(), device]);
    prep.disk_overlay = Some(DiskPreparation {
        path,
        backing,
        backing_format: fmt.into(),
        size,
    });
    Ok(())
}

pub(super) fn disk_size(value: &str) -> Result<String, Error> {
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
    if let Some(backend) = x.get("backend") {
        let backend = object(backend, "profile.tpm.backend")?;
        if backend
            .get("type")
            .is_some_and(|v| v.as_str() != Some("emulator"))
        {
            return Err(invalid("profile.tpm.backend.type must be emulator"));
        }
        if backend
            .get("version")
            .is_some_and(|v| v.as_str() != Some("2.0"))
        {
            return Err(invalid("profile.tpm.backend.version only supports 2.0"));
        }
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

fn engine_tracks(profile: &Map<String, Value>) -> Result<Vec<String>, Error> {
    let Some(engine) = profile.get("engine") else {
        return Ok(vec!["qemu-system".into()]);
    };
    let tracks = engine
        .as_array()
        .ok_or_else(|| invalid("profile.engine must be an array of engine track names"))?;
    if tracks.is_empty() {
        return Err(invalid("profile.engine must not be empty"));
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
                    invalid(&format!(
                        "profile.engine[{index}] must be a non-empty string"
                    ))
                })
        })
        .collect()
}

fn resolve_engine_ordered(
    release: &Value,
    bundle: &Path,
    tracks: &[String],
    target: &str,
) -> Result<(String, Value, PathBuf), Error> {
    let mut last_error = None;
    for track in tracks {
        match resolve_engine(release, bundle, track, target) {
            Ok((engine, executable)) => return Ok((track.clone(), engine, executable)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| invalid("profile.engine must not be empty")))
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
    // Resolved instances may carry a pinned executable directly. This avoids
    // consulting mutable release metadata at start while retaining the same
    // validation path for the selected build.
    if let Some(executable) = entry.get("executable").and_then(Value::as_str) {
        let executable = PathBuf::from(executable);
        if !executable.is_file() {
            return Err(invalid(&format!(
                "engine executable is missing: {}",
                executable.display()
            )));
        }
        let engine = serde_json::json!({
            "track_id": track,
            "build_digest": string(entry, "build_digest")?,
        });
        return Ok((engine, executable));
    }
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

// These NICs and AHCI disks are created by the board, not generic PCI devices.
fn append_udm_network(
    argv: &mut Vec<String>,
    network: Option<&Value>,
    helper: Option<&Path>,
    address: Option<&str>,
) -> Result<(), Error> {
    if let Some(ports) = network.and_then(|v| v.get("ports")) {
        let ports = ports
            .as_array()
            .filter(|ports| ports.len() == 4)
            .ok_or_else(|| {
                invalid(
                    "UDM Pro network.ports must contain eth9, eth8, eth10, switch0 (four ports)",
                )
            })?;
        argv.extend(["-net".into(), "none".into()]);
        for (index, port) in ports.iter().enumerate() {
            let port = object(port, "UDM Pro network port")?;
            let kind = string(port, "type")?;
            let id = format!("udm-port{index}");
            let backend = match kind.as_str() {
                // An empty hub reserves a slot without connecting it anywhere.
                "disabled" => format!("hubport,id={id},hubid={index}"),
                "user" => format!("user,id={id}"),
                "bridge" => {
                    let bridge = string(port, "bridge")?;
                    if bridge.len() > 15
                        || !bridge
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                    {
                        return Err(invalid("invalid UDM Pro bridge name"));
                    }
                    let mut backend = format!("bridge,id={id},br={bridge}");
                    if let Some(helper) = helper {
                        backend.push_str(&format!(
                            ",helper={}",
                            helper.display().to_string().replace(',', ",,")
                        ));
                    }
                    backend
                }
                _ => {
                    return Err(invalid(
                        "UDM Pro port type must be disabled, user, or bridge",
                    ));
                }
            };
            let address = if let Some(value) = port.get("mac") {
                mac(value
                    .as_str()
                    .ok_or_else(|| invalid("port.mac must be a string"))?)?
            } else if let Some(base) = address {
                // Keep the instance-derived prefix and allocate adjacent addresses.
                let base = mac(base)?;
                let mut bytes: Vec<u8> = base
                    .split(':')
                    .map(|v| u8::from_str_radix(v, 16).unwrap())
                    .collect();
                let suffix = ((u32::from(bytes[3]) << 16)
                    | (u32::from(bytes[4]) << 8)
                    | u32::from(bytes[5]))
                    + index as u32;
                bytes[3] = (suffix >> 16) as u8;
                bytes[4] = (suffix >> 8) as u8;
                bytes[5] = suffix as u8;
                bytes
                    .iter()
                    .map(|v| format!("{v:02x}"))
                    .collect::<Vec<_>>()
                    .join(":")
            } else {
                return Err(invalid(
                    "UDM Pro multi-port networking requires an instance identity or a MAC for every port",
                ));
            };
            argv.extend([
                "-netdev".into(),
                backend,
                "-net".into(),
                format!("nic,model=alpine-eth-pci,netdev={id},macaddr={address}"),
            ]);
        }
        return Ok(());
    }
    let kind = network
        .and_then(|v| v.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("disabled");
    argv.extend(["-net".into(), "none".into()]);
    let mut backend = match kind {
        "disabled" => return Ok(()),
        "user" => "user".to_owned(),
        "bridge" => {
            let bridge = network
                .and_then(|v| v.get("bridge"))
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("profile.network.bridge is required"))?;
            let mut value = format!("bridge,br={bridge}");
            if let Some(helper) = helper {
                value.push_str(&format!(",helper={}", helper.display()));
            }
            value
        }
        _ => return Err(invalid("unsupported UDM Pro network type")),
    };
    backend.push_str(",model=alpine-eth-pci");
    if let Some(address) = address {
        backend.push_str(&format!(",mac={}", mac(address)?));
    }
    argv.extend(["-nic".into(), backend]);
    Ok(())
}

fn append_direct_boot(
    argv: &mut Vec<String>,
    boot: Option<&Value>,
    sources: &SourceResolver<'_>,
) -> Result<(), Error> {
    let Some(boot) = boot else { return Ok(()) };
    let mut options = Vec::new();
    for (key, option) in [("from", "order"), ("once", "once")] {
        if let Some(value) = boot.get(key) {
            let drive = match value.as_str() {
                Some("disk") => "c",
                Some("cdrom") => "d",
                Some("network") => "n",
                Some("floppy") => "a",
                _ => {
                    return Err(invalid(&format!(
                        "profile.boot.{key} must be disk, cdrom, network, or floppy"
                    )));
                }
            };
            options.push(format!("{option}={drive}"));
        }
    }
    if let Some(strict) = boot.get("strict") {
        let strict = strict
            .as_bool()
            .ok_or_else(|| invalid("profile.boot.strict must be a boolean"))?;
        options.push(format!("strict={}", if strict { "on" } else { "off" }));
    }
    if let Some(timeout) = boot.get("reboot_timeout") {
        let timeout = timeout
            .as_i64()
            .filter(|v| (-1..=i64::from(i32::MAX)).contains(v))
            .ok_or_else(|| {
                invalid(
                    "profile.boot.reboot_timeout must be -1 or milliseconds from 0 to 2147483647",
                )
            })?;
        options.push(format!("reboot-timeout={timeout}"));
    }
    if let Some(menu) = boot.get("menu") {
        let menu = menu
            .as_bool()
            .ok_or_else(|| invalid("profile.boot.menu must be a boolean"))?;
        options.push(format!("menu={}", if menu { "on" } else { "off" }));
    }
    if let Some(splash) = boot.get("splash") {
        let splash = splash
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("profile.boot.splash must be a non-empty file path"))?;
        let path = Path::new(splash)
            .canonicalize()
            .map_err(|e| invalid(&format!("cannot open profile.boot.splash {splash:?}: {e}")))?;
        if !path.is_file() {
            return Err(invalid("profile.boot.splash must name a regular file"));
        }
        options.push(format!(
            "splash={}",
            path.to_string_lossy().replace(',', ",,")
        ));
    }
    if let Some(timeout) = boot.get("timeout") {
        let timeout = timeout
            .as_u64()
            .filter(|v| *v <= u64::from(u32::MAX))
            .ok_or_else(|| {
                invalid("profile.boot.timeout must be milliseconds from 0 to 4294967295")
            })?;
        options.push(format!("splash-time={timeout}"));
    }
    if !options.is_empty() {
        argv.extend(["-boot".into(), options.join(",")]);
    }
    for (key, flag) in [
        ("kernel", "-kernel"),
        ("initrd", "-initrd"),
        ("dtb", "-dtb"),
    ] {
        if let Some(reference) = boot.get(key).filter(|v| !v.is_null()) {
            let reference = object(reference, &format!("profile.boot.{key}"))?;
            let path = sources.path(reference.get("source"), &format!("profile.boot.{key}"))?;
            argv.extend([flag.into(), path.display().to_string()]);
        }
    }
    if let Some(append) = boot.get("append") {
        argv.extend([
            "-append".into(),
            append
                .as_str()
                .ok_or_else(|| invalid("profile.boot.append must be a string"))?
                .into(),
        ]);
    }
    Ok(())
}

fn append_udm_storage(
    argv: &mut Vec<String>,
    storage: Option<&Value>,
    sources: &SourceResolver<'_>,
) -> Result<(), Error> {
    let storage = object(
        storage.ok_or_else(|| invalid("UDM Pro requires boot and SPI storage"))?,
        "profile.storage",
    )?;
    for (role, id) in [("boot", "udm-boot"), ("spi", "udm-config")] {
        let disk = storage
            .get(role)
            .ok_or_else(|| invalid(&format!("profile.storage.{role} is required")))?;
        if disk.get("format").and_then(Value::as_str) != Some("raw") {
            return Err(invalid("UDM Pro storage requires raw images"));
        }
        let disk = object(disk, &format!("profile.storage.{role}"))?;
        let path = sources.path(disk.get("source"), &format!("profile.storage.{role}"))?;
        // The firmware rewrites its GPT: every process starts with pristine
        // backing images and discards writes when it exits.
        argv.extend([
            "-drive".into(),
            format!(
                "file={},if=none,format=raw,id={id},snapshot=on",
                path.display().to_string().replace(',', ",,")
            ),
        ]);
    }
    Ok(())
}

fn append_udm_devices(
    argv: &mut Vec<String>,
    devices: Option<&Value>,
    runtime: &Path,
) -> Result<(), Error> {
    let Some(devices) = devices else {
        return Ok(());
    };
    let enabled = |key: &str| -> Result<bool, Error> {
        devices
            .get(key)
            .map(|v| {
                v.as_bool()
                    .ok_or_else(|| invalid(&format!("UDM Pro devices.{key} must be a boolean")))
            })
            .unwrap_or(Ok(false))
    };
    if enabled("lcd")? {
        argv.extend([
            "-device".into(),
            "qemu-xhci,id=udm-usb,bus=pcie-external,addr=9".into(),
        ]);
        for name in ["lcm-events", "lcm-input"] {
            let path = runtime.join(format!("{name}.sock"));
            argv.extend([
                "-chardev".into(),
                format!(
                    "socket,id={name},path={},server=on,wait=off",
                    path.display().to_string().replace(',', ",,")
                ),
            ]);
        }
        argv.extend([
            "-device".into(),
            "unifi-lcm,bus=udm-usb.0,events=lcm-events,input=lcm-input,udm-pro=on".into(),
        ]);
    }
    if enabled("bluetooth")? {
        if !matches!(
            devices
                .get("serial")
                .and_then(|serial| serial.get("type"))
                .and_then(Value::as_str),
            Some("file" | "socket")
        ) {
            return Err(invalid(
                "UDM Pro Bluetooth requires devices.serial.type=file or socket to reserve ttyS0",
            ));
        }
        let path = runtime.join("bluetooth.sock");
        argv.extend([
            "-chardev".into(),
            format!(
                "socket,id=btuart,path={},server=on,wait=off",
                path.display().to_string().replace(',', ",,")
            ),
            "-serial".into(),
            "chardev:btuart".into(),
        ]);
    }
    Ok(())
}

#[cfg(test)]
mod video_storage_tests {
    use super::*;
    use serde_json::json;

    fn disk_device(argv: &[String]) -> &str {
        let idx = argv.iter().rposition(|a| a == "-device").unwrap();
        &argv[idx + 1]
    }

    fn sources() -> (BTreeMap<String, PathBuf>, SourceResolver<'static>) {
        let mut components = BTreeMap::new();
        components.insert("disk".into(), PathBuf::from("/backing"));
        let leaked = Box::leak(Box::new(components));
        (
            leaked.clone(),
            SourceResolver {
                components: leaked,
                base_dir: None,
            },
        )
    }

    #[test]
    fn analysis_disk_carries_forged_model_and_serial() {
        let mut argv = Vec::new();
        let mut prep = Preparation::default();
        let (_components, sources) = sources();
        let storage = json!({"disk_product": "SATA SSD", "disk_serial_prefix": "ANSSD"});
        append_storage(
            &mut argv,
            &mut prep,
            Some(&json!({"disk": {"source": {"image_component": "disk"}, "bus": "sata"}})),
            &sources,
            "pc-q35-10.1",
            Path::new("/state"),
            Some(&storage),
        )
        .unwrap();
        let device = disk_device(&argv);
        assert!(
            device.starts_with("ide-hd,bus=pc-sata.0,drive=pc-disk"),
            "{device}"
        );
        assert!(device.contains(",model=SATA SSD"), "{device}");
        assert!(device.contains(",serial=ANSSD-0"), "{device}");
    }

    #[test]
    fn explicit_disk_serial_wins_over_the_descriptor_prefix() {
        let mut argv = Vec::new();
        let mut prep = Preparation::default();
        let (_components, sources) = sources();
        let storage = json!({"disk_serial_prefix": "ANSSD"});
        append_storage(
            &mut argv,
            &mut prep,
            Some(&json!({"disk": {"source": {"image_component": "disk"}, "bus": "sata", "serial": "SN-EXPLICIT"}})),
            &sources,
            "pc-q35-10.1",
            Path::new("/state"),
            Some(&storage),
        )
        .unwrap();
        assert!(disk_device(&argv).contains(",serial=SN-EXPLICIT"));
    }

    #[test]
    fn virtio_disk_takes_no_ata_identity() {
        let mut argv = Vec::new();
        let mut prep = Preparation::default();
        let (_components, sources) = sources();
        let storage = json!({"disk_product": "SATA SSD", "disk_serial_prefix": "ANSSD"});
        append_storage(
            &mut argv,
            &mut prep,
            Some(&json!({"disk": {"source": {"image_component": "disk"}, "bus": "virtio"}})),
            &sources,
            "pc-q35-10.1",
            Path::new("/state"),
            Some(&storage),
        )
        .unwrap();
        let device = disk_device(&argv);
        assert_eq!(device, "virtio-blk-pci,drive=pc-disk");
    }

    #[test]
    fn analysis_vga_suppresses_default_and_forges_monitor_and_pci_identity() {
        let mut argv = Vec::new();
        let display = json!({
            "vendor": "DEL", "name": "DELL P2419H", "serial": "10000001",
            "xres": 1920, "yres": 1080, "width_mm": 527, "height_mm": 296,
            "pci_identity": true, "pci_identity_delay_ms": 15000,
            "pci_vendor_id": 32902, "pci_device_id": 39497,
            "pci_subsystem_vendor_id": 32902, "pci_subsystem_id": 12290
        });
        append_analysis_video(&mut argv, Some(&json!({"type": "vga"})), Some(&display)).unwrap();
        assert_eq!(argv[0], "-vga");
        assert_eq!(argv[1], "none");
        let device = &argv[3];
        for expected in [
            "VGA,id=pc-video",
            ",vendor=DEL",
            ",name=DELL P2419H",
            ",serial=10000001",
            ",xres=1920",
            ",xmax=1920",
            ",ymax=1080",
            ",width-mm=527",
            ",pci-vendor-id=32902",
            ",pci-device-id=39497",
            ",pci-identity-delay-ms=15000",
        ] {
            assert!(device.contains(expected), "missing {expected} in {device}");
        }
    }

    #[test]
    fn pci_identity_off_forges_only_the_monitor() {
        let mut argv = Vec::new();
        let display =
            json!({"vendor": "DEL", "xres": 1920, "pci_identity": false, "pci_vendor_id": 32902});
        append_analysis_video(&mut argv, Some(&json!({"type": "vga"})), Some(&display)).unwrap();
        let device = &argv[3];
        assert!(device.contains(",vendor=DEL"));
        assert!(!device.contains("pci-vendor-id"), "{device}");
    }

    #[test]
    fn non_vga_and_absent_analysis_leave_the_default_adapter() {
        let mut argv = Vec::new();
        append_analysis_video(
            &mut argv,
            Some(&json!({"type": "virtio"})),
            Some(&json!({"vendor": "DEL"})),
        )
        .unwrap();
        assert!(argv.is_empty());
        append_analysis_video(&mut argv, Some(&json!({"type": "vga"})), None).unwrap();
        assert!(argv.is_empty());
    }
}

#[cfg(test)]
mod boot_tests {
    use super::*;
    #[test]
    fn firmware_boot_settings_are_combined_and_validated() {
        let mut args = Vec::new();
        let components = BTreeMap::new();
        let sources = SourceResolver {
            components: &components,
            base_dir: None,
        };
        append_direct_boot(
            &mut args,
            Some(&serde_json::json!({
                "from":"disk", "once":"cdrom", "menu":true, "strict":true,
                "timeout":2500, "reboot_timeout":-1
            })),
            &sources,
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "-boot",
                "order=c,once=d,strict=on,reboot-timeout=-1,menu=on,splash-time=2500"
            ]
        );
        for source in ["disk", "cdrom", "network", "floppy"] {
            assert!(
                append_direct_boot(
                    &mut Vec::new(),
                    Some(&serde_json::json!({"from":source})),
                    &sources
                )
                .is_ok()
            );
        }
        for invalid in [
            serde_json::json!({"from":"disk,once=d"}),
            serde_json::json!({"once":true}),
            serde_json::json!({"strict":"yes"}),
            serde_json::json!({"menu":1}),
            serde_json::json!({"splash":false}),
            serde_json::json!({"reboot_timeout":-2}),
            serde_json::json!({"reboot_timeout":2147483648_u64}),
            serde_json::json!({"timeout":1.5}),
        ] {
            assert!(
                append_direct_boot(&mut Vec::new(), Some(&invalid), &sources).is_err(),
                "{invalid}"
            );
        }
    }
}
