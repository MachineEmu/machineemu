//! Compatibility with the existing Python analysis profile launch contract.
use super::Error;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

pub(super) struct AnalysisPlan {
    pub machine_suffix: String,
    pub argv: Vec<String>,
    pub payload: Value,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
fn mapping<'a>(value: &'a Value, name: &str) -> Result<&'a Map<String, Value>, Error> {
    value
        .as_object()
        .ok_or_else(|| invalid(format!("{name} must be a mapping")))
}
fn optional_mapping<'a>(
    value: Option<&'a Value>,
    name: &str,
) -> Result<Option<&'a Map<String, Value>>, Error> {
    value.map(|value| mapping(value, name)).transpose()
}
fn ascii(value: &Value, name: &str, limit: usize) -> Result<String, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid(format!("{name} must be printable ASCII")))?;
    if text.is_empty()
        || text.len() > limit
        || !text.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
    {
        return Err(invalid(format!(
            "{name} must be printable ASCII of length 1..{limit}"
        )));
    }
    Ok(text.to_owned())
}
fn number(value: &Value, name: &str, low: i64, high: i64) -> Result<i64, Error> {
    let n = value
        .as_i64()
        .ok_or_else(|| invalid(format!("{name} must be an integer")))?;
    if !(low..=high).contains(&n) {
        return Err(invalid(format!("{name} is outside its supported range")));
    }
    Ok(n)
}
fn merge_known(defaults: Value, override_: Option<&Value>, name: &str) -> Result<Value, Error> {
    let mut defaults = defaults;
    let Some(override_) = optional_mapping(override_, name)? else {
        return Ok(defaults);
    };
    let target = defaults.as_object_mut().expect("defaults are mappings");
    for (key, value) in override_ {
        if !target.contains_key(key) {
            return Err(invalid(format!("{name}.{key} is unsupported")));
        }
        target.insert(key.clone(), value.clone());
    }
    Ok(defaults)
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn digest(seed: &str, label: &str) -> [u8; 32] {
    let mut sha = Sha256::new();
    sha.update(b"unifi-qemu-analysis\0");
    sha.update(seed.as_bytes());
    sha.update(b"\0");
    sha.update(label.as_bytes());
    sha.finalize().into()
}
fn base64(bytes: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as usize;
        let second = chunk.get(1).copied().unwrap_or(0) as usize;
        let third = chunk.get(2).copied().unwrap_or(0) as usize;
        out.push(CHARS[first >> 2] as char);
        out.push(CHARS[((first & 3) << 4) | (second >> 4)] as char);
        out.push(if chunk.len() > 1 {
            CHARS[((second & 15) << 2) | (third >> 6)] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            CHARS[third & 63] as char
        } else {
            '='
        });
    }
    out
}
fn escape_machine(value: &Value) -> String {
    match value {
        Value::String(text) => text.replace(',', ",,"),
        _ => value.to_string().replace(',', ",,"),
    }
}
fn smbios_string(value: &Value, key: &str) -> Result<(), Error> {
    ascii(value, &format!("analysis.smbios.{key}"), 64)?;
    Ok(())
}
fn validate_smbios(input: Option<&Value>) -> Result<Value, Error> {
    let mut result = Map::new();
    let Some(input) = optional_mapping(input, "analysis.smbios")? else {
        return Ok(Value::Object(result));
    };
    const TEXT: &str = "bios_vendor bios_version system_manufacturer system_product system_version board_manufacturer board_product board_version chassis_manufacturer chassis_version chassis_asset chassis_sku processor_manufacturer processor_version processor_asset processor_part processor_socket_prefix memory_manufacturer memory_part memory_bank memory_asset memory_locator_prefix";
    for (key, value) in input {
        if TEXT.split_whitespace().any(|known| known == key) {
            smbios_string(value, key)?;
        } else if [
            "processor_max_speed",
            "processor_current_speed",
            "processor_family",
            "memory_speed",
        ]
        .contains(&key.as_str())
        {
            number(value, &format!("analysis.smbios.{key}"), 0, 65535)?;
        } else if key == "processor_id" {
            if value.as_u64().is_none() {
                return Err(invalid(
                    "analysis.smbios.processor_id must be a u64 integer",
                ));
            }
        } else if key == "bios_vm" {
            if !value.is_boolean() {
                return Err(invalid("analysis.smbios.bios_vm must be boolean"));
            }
        } else {
            return Err(invalid(format!("analysis.smbios.{key} is unsupported")));
        }
        result.insert(key.clone(), value.clone());
    }
    Ok(Value::Object(result))
}
fn validate_acpi(input: Option<&Value>) -> Result<Value, Error> {
    let acpi = input.cloned().unwrap_or_else(|| json!({}));
    for (key, value) in mapping(&acpi, "analysis.acpi")? {
        match key.as_str() {
            "oem_id" => {
                ascii(value, "analysis.acpi.oem_id", 6)?;
            }
            "oem_table_id" => {
                ascii(value, "analysis.acpi.oem_table_id", 8)?;
            }
            "creator_id" => {
                ascii(value, "analysis.acpi.creator_id", 4)?;
            }
            "oem_revision" | "creator_revision" => {
                number(value, &format!("analysis.acpi.{key}"), 0, u32::MAX as i64)?;
            }
            _ => return Err(invalid(format!("analysis.acpi.{key} is unsupported"))),
        }
    }
    Ok(acpi)
}
fn validate_descriptors(input: Option<&Value>) -> Result<Value, Error> {
    let input = optional_mapping(input, "analysis.device_descriptors")?;
    let section = |name: &str, defaults: Value| -> Result<Value, Error> {
        merge_known(
            defaults,
            input.and_then(|map| map.get(name)),
            &format!("analysis.device_descriptors.{name}"),
        )
    };
    if let Some(input) = input {
        for key in input.keys() {
            if !["storage", "display", "usb"].contains(&key.as_str()) {
                return Err(invalid(format!(
                    "analysis.device_descriptors.{key} is unsupported"
                )));
            }
        }
    }
    let storage = section(
        "storage",
        json!({"disk_vendor":"ATA","disk_product":"SATA SSD","disk_serial_prefix":"ANSSD","optical_vendor":"ATA","optical_product":"DVD-ROM"}),
    )?;
    for (key, value) in mapping(&storage, "analysis.device_descriptors.storage")? {
        let limit = match key.as_str() {
            "disk_vendor" | "optical_vendor" => 8,
            "disk_serial_prefix" => 12,
            _ => 16,
        };
        ascii(
            value,
            &format!("analysis.device_descriptors.storage.{key}"),
            limit,
        )?;
    }
    let display = section(
        "display",
        json!({"vendor":"DEL","name":"DELL P2419H","serial":"10000001","xres":1920,"yres":1080,"width_mm":527,"height_mm":296,"refresh_rate":60000,"pci_identity":false,"pci_identity_delay_ms":15000,"pci_vendor_id":32902,"pci_device_id":18048,"pci_subsystem_vendor_id":4136,"pci_subsystem_id":2646}),
    )?;
    for (key, value) in mapping(&display, "analysis.device_descriptors.display")? {
        let path = format!("analysis.device_descriptors.display.{key}");
        match key.as_str() {
            "vendor" => {
                let value = ascii(value, &path, 3)?;
                if value.len() != 3 || !value.bytes().all(|b| b.is_ascii_uppercase()) {
                    return Err(invalid(format!(
                        "{path} must be three uppercase ASCII letters"
                    )));
                }
            }
            "name" | "serial" => {
                ascii(value, &path, 12)?;
            }
            "pci_identity" => {
                if !value.is_boolean() {
                    return Err(invalid(format!("{path} must be boolean")));
                }
            }
            "xres" => {
                number(value, &path, 640, 7680)?;
            }
            "yres" => {
                number(value, &path, 480, 4320)?;
            }
            "width_mm" | "height_mm" => {
                number(value, &path, 100, 2000)?;
            }
            "refresh_rate" => {
                number(value, &path, 24000, 240000)?;
            }
            "pci_identity_delay_ms" => {
                number(value, &path, 0, 300000)?;
            }
            _ => {
                number(value, &path, 1, 65535)?;
            }
        }
    }
    let usb = input.and_then(|map| map.get("usb"));
    let usb = optional_mapping(usb, "analysis.device_descriptors.usb")?;
    if let Some(usb) = usb {
        for key in usb.keys() {
            if !["hid", "storage"].contains(&key.as_str()) {
                return Err(invalid(format!(
                    "analysis.device_descriptors.usb.{key} is unsupported"
                )));
            }
        }
    }
    let hid = merge_known(
        json!({"manufacturer":"Wacom Co.,Ltd.","product":"Wacom Tablet","serial":"WTAB10000001","vendorid":1386,"productid":185,"bcd_device":256}),
        usb.and_then(|v| v.get("hid")),
        "analysis.device_descriptors.usb.hid",
    )?;
    let usb_storage = merge_known(
        json!({"manufacturer":"SanDisk","product":"Ultra USB 3.0","serial_prefix":"USBSSD","vendorid":1921,"productid":21889,"bcd_device":256}),
        usb.and_then(|v| v.get("storage")),
        "analysis.device_descriptors.usb.storage",
    )?;
    for (name, item) in [("hid", &hid), ("storage", &usb_storage)] {
        for (key, value) in mapping(item, "usb descriptors")? {
            let path = format!("analysis.device_descriptors.usb.{name}.{key}");
            if ["vendorid", "productid", "bcd_device"].contains(&key.as_str()) {
                number(value, &path, 1, 65535)?;
            } else {
                ascii(value, &path, 31)?;
            }
        }
    }
    Ok(json!({"storage":storage,"display":display,"usb":{"hid":hid,"storage":usb_storage}}))
}
fn validate_sensors(input: Option<&Value>) -> Result<Value, Error> {
    let sensors = merge_known(
        json!({"temperature_celsius":42,"passive_celsius":75,"critical_celsius":95,"fan_rpm":1200}),
        input,
        "analysis.sensors",
    )?;
    let get = |key| {
        number(
            &sensors[key],
            &format!("analysis.sensors.{key}"),
            -20,
            20000,
        )
    };
    let temp = get("temperature_celsius")?;
    let passive = get("passive_celsius")?;
    let critical = get("critical_celsius")?;
    let fan = get("fan_rpm")?;
    if temp > passive || passive > critical || critical > 127 || !(0..=20000).contains(&fan) {
        return Err(invalid("analysis.sensors thresholds are invalid"));
    }
    Ok(sensors)
}
fn validate_pci(input: Option<&Value>) -> Result<Value, Error> {
    let pci = merge_known(
        json!({"subsystem_vendor_id":4136,"subsystem_id":2646}),
        input,
        "analysis.pci",
    )?;
    for (key, value) in mapping(&pci, "analysis.pci")? {
        number(value, &format!("analysis.pci.{key}"), 1, 65535)?;
    }
    Ok(pci)
}
fn smbios_argv(smbios: &Value, uuid: &str, serials: &Map<String, Value>) -> Vec<String> {
    let mut args = vec!["-uuid".to_owned(), uuid.to_owned()];
    type SmbiosFields<'a> = (
        u8,
        &'a [(&'a str, &'a str)],
        &'a [(&'a str, &'a str)],
        Option<&'a str>,
    );
    let fields: &[SmbiosFields<'_>] = &[
        (
            0,
            &[("vendor", "bios_vendor"), ("version", "bios_version")],
            &[],
            None,
        ),
        (
            1,
            &[
                ("manufacturer", "system_manufacturer"),
                ("product", "system_product"),
                ("version", "system_version"),
            ],
            &[],
            Some("system"),
        ),
        (
            2,
            &[
                ("manufacturer", "board_manufacturer"),
                ("product", "board_product"),
                ("version", "board_version"),
            ],
            &[],
            Some("board"),
        ),
        (
            3,
            &[
                ("manufacturer", "chassis_manufacturer"),
                ("version", "chassis_version"),
                ("asset", "chassis_asset"),
                ("sku", "chassis_sku"),
            ],
            &[],
            Some("chassis"),
        ),
        (
            4,
            &[
                ("manufacturer", "processor_manufacturer"),
                ("version", "processor_version"),
                ("asset", "processor_asset"),
                ("part", "processor_part"),
                ("sock_pfx", "processor_socket_prefix"),
            ],
            &[
                ("max-speed", "processor_max_speed"),
                ("current-speed", "processor_current_speed"),
                ("processor-family", "processor_family"),
                ("processor-id", "processor_id"),
            ],
            Some("processor"),
        ),
        (
            17,
            &[
                ("manufacturer", "memory_manufacturer"),
                ("part", "memory_part"),
                ("asset", "memory_asset"),
                ("bank", "memory_bank"),
                ("loc_pfx", "memory_locator_prefix"),
            ],
            &[("speed", "memory_speed")],
            Some("memory"),
        ),
    ];
    for (kind, strings, numbers, serial) in fields {
        let mut parts = vec![format!("type={kind}")];
        for (name, key) in *strings {
            if let Some(text) = smbios.get(key).and_then(Value::as_str) {
                parts.push(format!("{name}={}", text.replace(',', ",,")));
            }
        }
        for (name, key) in *numbers {
            if let Some(value) = smbios.get(key) {
                parts.push(format!("{name}={value}"));
            }
        }
        if *kind == 0
            && let Some(value) = smbios.get("bios_vm").and_then(Value::as_bool)
        {
            parts.push(format!("vm={value}"));
        }
        if *kind == 1 {
            parts.push(format!("uuid={uuid}"));
        }
        if let Some(serial) = serial {
            parts.push(format!(
                "serial={}",
                serials[*serial].as_str().unwrap_or_default()
            ));
        }
        if parts.len() > 1 {
            args.extend(["-smbios".to_owned(), parts.join(",")]);
        }
    }
    args
}

pub(super) fn plan(
    profile: &Map<String, Value>,
    machine: &str,
    target: &str,
    vcpus: i64,
) -> Result<Option<AnalysisPlan>, Error> {
    let Some(raw) = profile.get("analysis") else {
        return Ok(None);
    };
    let raw = mapping(raw, "profile.analysis")?;
    const KEYS: &[&str] = &[
        "enabled",
        "profile",
        "identity_seed",
        "clone",
        "collection",
        "overlay",
        "telemetry",
        "patch_revision",
        "smbios",
        "acpi",
        "device_descriptors",
        "sensors",
        "pci",
    ];
    for key in raw.keys() {
        if !KEYS.contains(&key.as_str()) {
            return Err(invalid(format!("profile.analysis.{key} is unsupported")));
        }
    }
    if raw.get("enabled") != Some(&Value::Bool(true))
        || raw.get("profile").and_then(Value::as_str) != Some("malware-analysis")
    {
        return Err(invalid(
            "profile.analysis requires enabled=true and profile=malware-analysis",
        ));
    }
    if !(machine == "q35"
        || machine == "pc"
        || machine.starts_with("pc-q35-")
        || machine.starts_with("pc-i440fx-"))
        || !target.starts_with("x86_64-")
    {
        return Err(invalid(
            "malware-analysis requires an x86_64 q35 or pc machine",
        ));
    }
    let seed = raw
        .get("identity_seed")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && !s.contains('\0'))
        .ok_or_else(|| invalid("analysis.identity_seed must be a non-empty string"))?;
    let clone = raw
        .get("clone")
        .filter(|v| !v.is_null())
        .map(|v| ascii(v, "analysis.clone", 64))
        .transpose()?;
    if let Some(clone) = &clone
        && (!clone
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
            || !clone
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')))
    {
        return Err(invalid("analysis.clone must be a safe identifier"));
    }
    for key in ["collection", "overlay", "telemetry"] {
        if raw.get(key).is_some_and(|v| !v.is_boolean()) {
            return Err(invalid(format!("analysis.{key} must be boolean")));
        }
    }
    let revision = raw
        .get("patch_revision")
        .map(|value| {
            value
                .as_str()
                .filter(|text| !text.is_empty() && !text.contains('\0'))
                .map(str::to_owned)
                .ok_or_else(|| invalid("analysis.patch_revision must be a non-empty string"))
        })
        .transpose()?
        .unwrap_or_else(|| "machineemu-analysis-1".into());
    let effective = clone.as_ref().map_or_else(
        || seed.to_owned(),
        |clone| format!("{seed}\0clone\0{clone}"),
    );
    let uuid_bytes = digest(&effective, "uuid");
    let mut uuid_bytes: [u8; 16] = uuid_bytes[..16].try_into().expect("16 bytes");
    uuid_bytes[6] = (uuid_bytes[6] & 15) | 0x40;
    uuid_bytes[8] = (uuid_bytes[8] & 63) | 0x80;
    let h = hex(&uuid_bytes);
    let uuid = format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    );
    let mac = format!(
        "02:{}",
        hex(&digest(&effective, "mac")[..5])
            .as_bytes()
            .chunks(2)
            .map(|part| std::str::from_utf8(part).unwrap())
            .collect::<Vec<_>>()
            .join(":")
    );
    let seed_hash = hex(&Sha256::digest(seed.as_bytes()));
    let serials: Map<String, Value> = ["bios", "system", "board", "chassis", "processor", "memory"]
        .into_iter()
        .map(|kind| {
            (
                kind.to_owned(),
                Value::String(format!(
                    "AN-{}-{}",
                    kind.to_ascii_uppercase(),
                    &seed_hash[..12]
                )),
            )
        })
        .collect();
    let identity = json!({"uuid":uuid,"mac":mac,"serials":serials,"identity_seed_sha256":seed_hash,"clone":clone});
    let smbios = validate_smbios(raw.get("smbios"))?;
    let acpi = validate_acpi(raw.get("acpi"))?;
    let descriptors = validate_descriptors(raw.get("device_descriptors"))?;
    let sensors = validate_sensors(raw.get("sensors"))?;
    let pci = validate_pci(raw.get("pci"))?;
    let normalized = json!({"schema_version":1,"profile":"malware-analysis","identity_seed_sha256":seed_hash,"identity":identity,"clone":clone,"collection":raw.get("collection").cloned().unwrap_or(json!(false)),"overlay":raw.get("overlay").cloned().unwrap_or(json!(true)),"telemetry":raw.get("telemetry").cloned().unwrap_or(json!(true)),"patch_revision":revision,"smbios":smbios,"acpi":acpi,"device_descriptors":descriptors,"sensors":sensors,"pci":pci});
    let cpu = profile.get("cpu").cloned().unwrap_or(json!("host,kvm=off"));
    let cpu_text = cpu
        .as_str()
        .filter(|text| !text.is_empty() && !text.contains('\0'))
        .ok_or_else(|| invalid("profile.cpu must be a non-empty string"))?;
    let cpu_parts: Vec<_> = cpu_text.split(',').collect();
    if cpu_parts.iter().any(|part| part.is_empty())
        || !cpu_parts.contains(&"kvm=off")
        || cpu_parts.contains(&"hypervisor")
    {
        return Err(invalid(
            "malware-analysis CPU policy must include kvm=off and omit hypervisor",
        ));
    }
    let network = profile
        .get("network")
        .cloned()
        .unwrap_or(json!({"type":"user"}));
    let payload = json!({"analysis":normalized,"network":network,"cpu":cpu,"vcpu":vcpus,"memory":profile.get("resources").and_then(|v| v.get("memory"))});
    let encoded = base64(&serde_json::to_vec(&payload).map_err(|e| invalid(e.to_string()))?);
    let mut suffix = format!("analysis-profile=on,x-analysis-profile-json-base64={encoded}");
    for (source, names) in [
        (
            &acpi,
            &[
                ("oem_id", "x-oem-id"),
                ("oem_table_id", "x-oem-table-id"),
                ("oem_revision", "x-oem-revision"),
                ("creator_id", "x-creator-id"),
                ("creator_revision", "x-creator-revision"),
            ][..],
        ),
        (
            &sensors,
            &[
                ("temperature_celsius", "x-analysis-temp-c"),
                ("passive_celsius", "x-analysis-passive-temp-c"),
                ("critical_celsius", "x-analysis-critical-temp-c"),
                ("fan_rpm", "x-analysis-fan-rpm"),
            ][..],
        ),
        (
            &pci,
            &[
                ("subsystem_vendor_id", "x-analysis-pci-subsystem-vendor-id"),
                ("subsystem_id", "x-analysis-pci-subsystem-id"),
            ][..],
        ),
    ] {
        for (key, name) in names {
            if let Some(value) = source.get(key) {
                suffix.push_str(&format!(",{name}={}", escape_machine(value)));
            }
        }
    }
    let argv = smbios_argv(&smbios, &uuid, &serials);
    Ok(Some(AnalysisPlan {
        machine_suffix: suffix,
        argv,
        payload,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn bundled_profile() -> Value {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles/malware-analysis-x64.json");
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn bundled_analysis_matches_existing_identity_and_encodes_all_descriptors() {
        let profile = bundled_profile();
        let analysis = plan(
            profile.as_object().unwrap(),
            "pc-q35-10.1",
            "x86_64-softmmu",
            8,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            analysis.payload["analysis"]["identity"]["uuid"],
            "e22eef7e-c6cd-4e70-a29b-971e658a541b"
        );
        assert_eq!(
            analysis.payload["analysis"]["identity"]["mac"],
            "02:93:f3:f4:04:ce"
        );
        assert_eq!(
            analysis.payload["analysis"]["device_descriptors"]["display"]["pci_device_id"],
            39497
        );
        assert_eq!(
            analysis.payload["analysis"]["smbios"]["system_product"],
            "NUC11TNKi5"
        );
        assert_eq!(analysis.payload["analysis"]["acpi"]["oem_id"], "INTEL");
        assert!(
            analysis
                .machine_suffix
                .starts_with("analysis-profile=on,x-analysis-profile-json-base64=")
        );
        assert!(
            analysis
                .machine_suffix
                .contains("x-analysis-pci-subsystem-id=12290")
        );
        assert!(
            analysis
                .argv
                .windows(2)
                .any(|part| part == ["-uuid", "e22eef7e-c6cd-4e70-a29b-971e658a541b"])
        );
        assert!(
            analysis
                .argv
                .iter()
                .any(|arg| arg.contains("product=NUC11TNKi5"))
        );
        let json = analysis.payload.to_string();
        assert!(!json.contains("analysis-default"));
    }

    #[test]
    fn analysis_rejects_unsupported_or_invalid_settings() {
        let profile = bundled_profile();
        for (path, value) in [
            ("/analysis/enabled", json!(false)),
            ("/analysis/device_descriptors/display/xres", json!(100)),
            ("/analysis/smbios/system_product", json!("bad,\nvalue")),
            ("/analysis/sensors/fan_rpm", json!(30000)),
        ] {
            let mut broken = profile.clone();
            *broken.pointer_mut(path).unwrap() = value;
            assert!(
                plan(
                    broken.as_object().unwrap(),
                    "pc-q35-10.1",
                    "x86_64-softmmu",
                    8
                )
                .is_err(),
                "{path}"
            );
        }
        assert!(plan(profile.as_object().unwrap(), "virt", "aarch64-softmmu", 8).is_err());
        let mut invalid_cpu = profile.clone();
        invalid_cpu["cpu"] = json!("host,hypervisor=on");
        assert!(
            plan(
                invalid_cpu.as_object().unwrap(),
                "pc-q35-10.1",
                "x86_64-softmmu",
                8
            )
            .is_err()
        );
        let mut default_cpu = profile.clone();
        default_cpu.as_object_mut().unwrap().remove("cpu");
        assert_eq!(
            plan(
                default_cpu.as_object().unwrap(),
                "pc-q35-10.1",
                "x86_64-softmmu",
                8
            )
            .unwrap()
            .unwrap()
            .payload["cpu"],
            "host,kvm=off"
        );
    }
}
