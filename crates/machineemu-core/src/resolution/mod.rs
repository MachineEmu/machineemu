//! Pure profile to instance resolution.
use crate::{Error, Result, domain::configuration::*};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EngineCapability {
    pub track: String,
    #[serde(default)]
    pub build_digest: Option<String>,
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub patch_revision: Option<String>,
    #[serde(default)]
    pub machines: Vec<String>,
}
#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ResolutionContext {
    #[serde(default)]
    pub instance_id: String,
    #[serde(default)]
    pub engines: Vec<EngineCapability>,
    #[serde(default)]
    pub default_network: Option<String>,
    #[serde(default)]
    pub defaults: Value,
}

pub fn resolve_instance(
    profile: PartialProfile,
    hardware_identity: Option<HardwareIdentityDocument>,
    image: ImageManifestDocument,
    overrides: CreateInstanceOverrides,
    context: ResolutionContext,
) -> Result<InstanceDocument> {
    require_kind(&profile.kind, "Profile")?;
    require_kind(&image.kind, "Image")?;
    if let Some(identity) = &hardware_identity {
        require_kind(&identity.kind, "HardwareIdentity")?;
    }
    validate_profile_fields(&profile.spec, "profile.spec")?;
    validate_image_fields(&image.spec, "image.spec")?;
    if let Some(identity) = &hardware_identity {
        validate_identity_fields(&identity.spec, "hardware_identity.spec")?;
    }
    validate_profile_fields(&overrides.spec, "overrides.spec")?;
    let mut spec = object_or_empty(&context.defaults);
    merge_object(&mut spec, object_or_empty(&profile.spec))?;
    normalize_compact_devices(&mut spec);
    if let Some(identity) = &hardware_identity {
        let source = object_or_empty(&identity.spec);
        let mut owned = Map::new();
        for key in [
            "identity",
            "pci",
            "descriptors",
            "smbios",
            "acpi",
            "sensors",
            // Analysis persona data is hardware owned.  Keep it in the
            // resolved top-level analysis namespace because the renderer's
            // analysis adapter consumes that contract, while the source
            // document remains a reusable HardwareIdentity.
            "compatibility",
        ] {
            if let Some(v) = source.get(key) {
                owned.insert(key.into(), v.clone());
            }
        }
        // Identity-owned fields live under the resolved hardware_identity
        // namespace.  Keeping this boundary explicit prevents a reusable
        // identity from overriding profile-owned resources or devices.
        let target = spec
            .entry("hardware_identity")
            .or_insert_with(|| Value::Object(Map::new()));
        let target = target
            .as_object_mut()
            .ok_or_else(|| Error::Process("hardware_identity must be object".into()))?;
        merge_object(target, owned)?;
        if let Some(analysis) = source.get("analysis") {
            let current = spec
                .entry("analysis")
                .or_insert_with(|| Value::Object(Map::new()));
            if current.is_object() && analysis.is_object() {
                let mut merged = current.as_object().cloned().unwrap_or_default();
                merge_object(&mut merged, analysis.as_object().cloned().unwrap())?;
                *current = Value::Object(merged);
            } else {
                *current = analysis.clone();
            }
        }
    }
    let image_spec = object_or_empty(&image.spec);
    // Image data is a binding, not a hardware layer. Keep it namespaced so an
    // image cannot silently replace profile architecture, machine, resources,
    // firmware, or device settings.
    let mut image_binding = Map::new();
    image_binding.insert("name".into(), Value::String(image.metadata.name.clone()));
    for key in [
        "components",
        "compatible_engines",
        "target",
        "architecture",
        "compatible_machines",
    ] {
        if let Some(v) = image_spec.get(key) {
            image_binding.insert(key.into(), v.clone());
        }
    }
    spec.insert("image".into(), Value::Object(image_binding));
    merge_object(&mut spec, object_or_empty(&overrides.spec))?;
    choose_engine(&mut spec, &profile, &image, &context)?;
    expand_defaults(&mut spec, &image, &context)?;
    validate(&spec, &image)?;
    if let Some(identity) = &hardware_identity {
        validate_identity_source_compatibility(&spec, identity)?;
    }
    let instance_id = if context.instance_id.is_empty() {
        profile.metadata.name.clone()
    } else {
        context.instance_id.clone()
    };
    generate_identities(&mut spec, &instance_id);
    Ok(InstanceDocument {
        api_version: "machineemu.io/v1".into(),
        kind: "Instance".into(),
        metadata: DocumentMetadata {
            name: instance_id,
            revision: 1,
            digest: None,
        },
        spec: Value::Object(spec),
        source: Some(SourceProvenance {
            profile: Some(source_ref(&profile.metadata)),
            hardware_identity: hardware_identity.as_ref().map(|v| source_ref(&v.metadata)),
            image: Some(source_ref(&image.metadata)),
            overrides: BTreeMap::new(),
        }),
        status: Some(serde_json::json!({"state":"created"})),
    })
}

/// Accept the compact device mappings used by pre-domain profile documents at
/// the migration boundary. New persisted instance documents always contain
/// the explicit collection form.
fn normalize_compact_devices(spec: &mut Map<String, Value>) {
    let Some(devices) = spec.get_mut("devices").and_then(Value::as_object_mut) else {
        return;
    };
    for (compact, collection, id) in [
        ("console", "graphics", "graphics0"),
        ("video", "video", "video0"),
        ("serial", "serial", "serial0"),
    ] {
        // `video` and `serial` use the same name for the compact field and
        // normalized collection. Convert an object in place before the
        // collection merge/validation pass; otherwise it reaches validation
        // as `devices.video: { ... }` and fails with the misleading array
        // diagnostic.
        let value = if compact == collection {
            if devices.get(collection).is_some_and(Value::is_object) {
                devices.remove(collection)
            } else {
                None
            }
        } else if devices.contains_key(collection) {
            None
        } else {
            devices.remove(compact)
        };
        let Some(value) = value else { continue };
        if let Some(mut item) = value.as_object().cloned() {
            item.insert("id".into(), Value::String(id.into()));
            devices.insert(collection.into(), Value::Array(vec![Value::Object(item)]));
        } else {
            devices.insert(compact.into(), value);
        }
    }
    if !devices.contains_key("interfaces")
        && let Some(nic) = devices.remove("nic")
        && let Some(model) = nic.as_str()
    {
        devices.insert(
            "interfaces".into(),
            serde_json::json!([{"id":"net0","model":model}]),
        );
    }
}

/// The document envelope is strongly typed, while specs intentionally use
/// JSON to keep the resolver independent from the renderer.  Keep the
/// accepted source vocabulary explicit at this boundary so a misspelled field
/// cannot silently become persisted instance state.
fn validate_profile_fields(value: &Value, path: &str) -> Result<()> {
    validate_object_keys(
        value,
        path,
        &[
            "architecture",
            "machine",
            "firmware",
            "resources",
            "devices",
            "boot",
            "lifecycle",
            "hardware_identity",
            "engine",
            "image",
            "network",
            "policy",
            "storage",
            "target",
            "domain",
            "cpu",
            "smm",
            "tpm",
            "state",
            "analysis",
        ],
    )
}

fn validate_identity_fields(value: &Value, path: &str) -> Result<()> {
    validate_object_keys(
        value,
        path,
        &[
            "compatibility",
            "identity",
            "pci",
            "descriptors",
            "smbios",
            "acpi",
            "sensors",
            // Kept for the one-time migration's analysis persona payload;
            // obsolete telemetry/overlay flags are rejected below.
            "analysis",
        ],
    )?;
    if let Some(analysis) = value.get("analysis") {
        reject_obsolete_identity_flags(analysis, &format!("{path}.analysis"))?;
    }
    Ok(())
}

fn validate_image_fields(value: &Value, path: &str) -> Result<()> {
    validate_object_keys(
        value,
        path,
        &[
            "architecture",
            "compatible_machines",
            "components",
            "compatible_engines",
            "engine_track",
            "supported_engine_tracks",
            "target",
            "image_id",
            "schema_version",
            "firmware",
            "disk_sha256",
            "firmware_sha256",
            "tpm_state_sha256",
        ],
    )
}

fn validate_object_keys(value: &Value, path: &str, allowed: &[&str]) -> Result<()> {
    // `spec` and override specs are omitted by older valid v1 documents and
    // deserialize to null through their serde defaults; treat that as an
    // empty object at this boundary.
    if value.is_null() {
        return Ok(());
    }
    let Some(object) = value.as_object() else {
        return Err(Error::Process(format!("{path} must be an object")));
    };
    for key in object.keys() {
        if !allowed.iter().any(|allowed| allowed == key) {
            return Err(Error::Process(format!("unknown field {path}.{key}")));
        }
    }
    Ok(())
}

fn reject_obsolete_identity_flags(value: &Value, path: &str) -> Result<()> {
    if let Some(object) = value.as_object() {
        for key in ["telemetry", "overlay"] {
            if object.contains_key(key) {
                return Err(Error::Process(format!(
                    "unknown field {path}.{key}; telemetry and overlay are not supported"
                )));
            }
        }
        for (key, nested) in object {
            reject_obsolete_identity_flags(nested, &format!("{path}.{key}"))?;
        }
    }
    if let Some(items) = value.as_array() {
        for (index, nested) in items.iter().enumerate() {
            reject_obsolete_identity_flags(nested, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}
fn source_ref(m: &DocumentMetadata) -> SourceRef {
    SourceRef {
        id: m.name.clone(),
        revision: m.revision,
        digest: m.digest.clone(),
    }
}
fn require_kind(actual: &str, expected: &str) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::Process(format!(
            "expected kind {expected}, got {actual}"
        )))
    }
}
fn object_or_empty(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}
pub fn merge_values(base: &mut Value, higher: &Value) -> Result<()> {
    let mut b = object_or_empty(base);
    merge_object(&mut b, object_or_empty(higher))?;
    *base = Value::Object(b);
    Ok(())
}
fn merge_object(base: &mut Map<String, Value>, high: Map<String, Value>) -> Result<()> {
    for (key, value) in high {
        if value.is_null() {
            return Err(Error::Process(format!("null is not valid at {key}")));
        }
        if is_device_collection(&key) && value.is_array() {
            let m = merge_devices(base.get(&key), value.as_array().unwrap(), &key)?;
            base.insert(key, m);
            continue;
        }
        if let (Some(old), Some(new)) = (base.get_mut(&key), value.as_object())
            && old.is_object()
        {
            let mut n = old.as_object().cloned().unwrap();
            merge_object(&mut n, new.clone())?;
            *old = Value::Object(n);
            continue;
        }
        base.insert(key, value);
    }
    Ok(())
}
fn is_device_collection(k: &str) -> bool {
    matches!(
        k,
        "disks" | "interfaces" | "controllers" | "graphics" | "video" | "serial" | "channels"
    )
}
fn merge_devices(old: Option<&Value>, high: &[Value], key: &str) -> Result<Value> {
    let mut out = old.and_then(Value::as_array).cloned().unwrap_or_default();
    let mut seen = BTreeSet::new();
    for item in high {
        let obj = item
            .as_object()
            .ok_or_else(|| Error::Process(format!("{key} entries must be objects")))?;
        let id = obj
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Process(format!("{key} entries require id")))?;
        if !seen.insert(id.to_string()) {
            return Err(Error::Process(format!("duplicate device id {id}")));
        }
        if obj.get("remove").and_then(Value::as_bool) == Some(true) {
            if obj.len() != 2 {
                return Err(Error::Process(format!("removal for {id} has extra fields")));
            }
            let pos = out
                .iter()
                .position(|v| v.get("id").and_then(Value::as_str) == Some(id))
                .ok_or_else(|| Error::Process(format!("cannot remove unknown device {id}")))?;
            out.remove(pos);
            continue;
        }
        if let Some(pos) = out
            .iter()
            .position(|v| v.get("id").and_then(Value::as_str) == Some(id))
        {
            let mut n = out[pos].as_object().cloned().unwrap_or_default();
            merge_object(&mut n, obj.clone())?;
            out[pos] = Value::Object(n);
        } else {
            out.push(item.clone());
        }
    }
    Ok(Value::Array(out))
}
fn choose_engine(
    spec: &mut Map<String, Value>,
    profile: &PartialProfile,
    image: &ImageManifestDocument,
    ctx: &ResolutionContext,
) -> Result<()> {
    if ctx.engines.is_empty() {
        return Err(Error::Process(
            "resolution context has no available engine builds".into(),
        ));
    }
    let preferred = profile.spec.get("engine").and_then(|v| {
        v.get("tracks")
            .and_then(Value::as_array)
            .and_then(|v| v.iter().find_map(Value::as_str))
            .or_else(|| v.get("track").and_then(Value::as_str))
            .or_else(|| v.as_str())
    });
    let tracks = image
        .spec
        .get("compatible_engines")
        .and_then(Value::as_array)
        .map(|v| v.iter().filter_map(Value::as_str).collect::<BTreeSet<_>>());
    let e = ctx
        .engines
        .iter()
        .find(|e| {
            preferred.is_none_or(|p| p == e.track)
                && tracks.as_ref().is_none_or(|s| s.contains(e.track.as_str()))
        })
        .ok_or_else(|| Error::Process("no compatible engine build".into()))?;
    if e.build_digest.is_none() || e.executable.is_none() {
        return Err(Error::Process(format!(
            "engine build {} is not pinned to a digest and executable",
            e.track
        )));
    }
    let profile_tracks = profile
        .spec
        .get("engine")
        .and_then(|value| value.get("tracks").or(Some(value)))
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let image_tracks = image
        .spec
        .get("compatible_engines")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    spec.insert(
        "engine".into(),
        serde_json::json!({
            "track": e.track,
            "build_digest": e.build_digest,
            "executable": e.executable,
            "patch_revision": e.patch_revision,
            "compatibility": {
                "image_tracks": image_tracks,
                "profile_tracks": profile_tracks
            }
        }),
    );
    Ok(())
}
fn expand_defaults(
    spec: &mut Map<String, Value>,
    image: &ImageManifestDocument,
    context: &ResolutionContext,
) -> Result<()> {
    // These are the only schema defaults.  Keep them here, after all source
    // documents and overrides have been merged, so dependent values (disk
    // format/bus, vCPU topology, and device identities) see final input.
    if !spec.contains_key("architecture")
        && let Some(architecture) = image.spec.get("architecture")
    {
        spec.insert("architecture".into(), architecture.clone());
    }
    if !spec.contains_key("machine") {
        spec.insert("machine".into(), serde_json::json!({"type":"q35"}));
    } else if spec.get("machine").is_some_and(Value::is_string) {
        let machine = spec.remove("machine").unwrap();
        spec.insert("machine".into(), serde_json::json!({"type":machine}));
    }
    let resources = spec
        .entry("resources")
        .or_insert_with(|| Value::Object(Map::new()));
    let r = resources
        .as_object_mut()
        .ok_or_else(|| Error::Process("resources must be object".into()))?;
    let count = r
        .get("vcpus")
        .and_then(Value::as_u64)
        .or_else(|| {
            r.get("vcpus")
                .and_then(|v| v.get("count"))
                .and_then(Value::as_u64)
        })
        .unwrap_or(1);
    if !r.contains_key("vcpus") || r.get("vcpus").is_some_and(Value::is_number) {
        r.insert(
            "vcpus".into(),
            serde_json::json!({"count":count,"sockets":1,"cores":count,"threads":1}),
        );
    }
    let image_spec = object_or_empty(&image.spec);
    let components = image_spec.get("components").and_then(Value::as_object);
    if spec.get("devices").is_none() {
        spec.insert("devices".into(), Value::Object(Map::new()));
    }
    let architecture = spec.get("architecture").cloned();
    let machine = spec.get("machine").cloned();
    if let Some(devices) = spec.get_mut("devices").and_then(Value::as_object_mut) {
        if !devices.contains_key("disks")
            && let Some(components) = components
            && let Some(component) = ["root_disk", "disk"]
                .iter()
                .find(|name| components.contains_key(**name))
        {
            let format = components[*component]
                .get("format")
                .cloned()
                .unwrap_or_else(|| Value::String("qcow2".into()));
            devices.insert("disks".into(), serde_json::json!([{"id":"root","role":"root","source":{"image_component":component},"driver":{"name":"qemu","type":format},"target":{"bus":"virtio","dev":"vda"}}]));
        }
        if !devices.contains_key("interfaces") {
            let network = context
                .default_network
                .clone()
                .unwrap_or_else(|| "default".into());
            devices.insert(
                "interfaces".into(),
                serde_json::json!([{"id":"net0","network":network,"model":"virtio-net-pci"}]),
            );
        }
        default_disks(devices, components)?;
        default_interfaces(devices)?;
        default_graphics(devices);
        default_video(devices, architecture.as_ref(), machine.as_ref());
        default_serial(devices);
        default_channels(devices);
    }
    Ok(())
}

fn default_disks(
    devices: &mut Map<String, Value>,
    components: Option<&Map<String, Value>>,
) -> Result<()> {
    let Some(disks) = devices.get_mut("disks").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    let mut next_virtio = 0u8;
    let mut next_sd = 0u8;
    for disk in disks {
        let d = disk
            .as_object_mut()
            .ok_or_else(|| Error::Process("disk entries must be objects".into()))?;
        let source = d.entry("source").or_insert_with(|| serde_json::json!({}));
        let source_obj = source
            .as_object_mut()
            .ok_or_else(|| Error::Process("disk source must be object".into()))?;
        let component = source_obj
            .get("component")
            .and_then(Value::as_str)
            .or_else(|| source_obj.get("image_component").and_then(Value::as_str))
            .unwrap_or("root_disk")
            .to_owned();
        source_obj
            .entry("component")
            .or_insert_with(|| Value::String(component.clone()));
        if let Some(c) = components.and_then(|m| m.get(&component))
            && let Some(format) = c.get("format")
        {
            d.entry("driver").or_insert_with(|| serde_json::json!({}));
            let driver = d.get_mut("driver").and_then(Value::as_object_mut).unwrap();
            driver.entry("type").or_insert_with(|| format.clone());
        }
        let driver = d
            .entry("driver")
            .or_insert_with(|| serde_json::json!({"name":"qemu"}));
        let driver = driver
            .as_object_mut()
            .ok_or_else(|| Error::Process("disk driver must be object".into()))?;
        driver
            .entry("name")
            .or_insert_with(|| Value::String("qemu".into()));
        driver
            .entry("cache")
            .or_insert_with(|| Value::String("none".into()));
        if driver.get("type").and_then(Value::as_str) == Some("qcow2") {
            driver
                .entry("discard")
                .or_insert_with(|| Value::String("unmap".into()));
        }
        let target = d.entry("target").or_insert_with(|| serde_json::json!({}));
        let target = target
            .as_object_mut()
            .ok_or_else(|| Error::Process("disk target must be object".into()))?;
        let bus = target
            .entry("bus")
            .or_insert_with(|| Value::String("virtio".into()));
        let bus = bus
            .as_str()
            .ok_or_else(|| Error::Process("disk target.bus must be string".into()))?
            .to_string();
        if !target.contains_key("dev") {
            let dev = match bus.as_str() {
                "virtio" => {
                    let n = next_virtio;
                    next_virtio += 1;
                    format!("vd{}", (b'a' + n) as char)
                }
                "sata" | "scsi" => {
                    let n = next_sd;
                    next_sd += 1;
                    format!("sd{}", (b'a' + n) as char)
                }
                _ => return Err(Error::Process(format!("unsupported disk bus {bus}"))),
            };
            target.insert("dev".into(), Value::String(dev));
        }
    }
    Ok(())
}

fn default_interfaces(devices: &mut Map<String, Value>) -> Result<()> {
    let Some(interfaces) = devices.get_mut("interfaces").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for interface in interfaces {
        let i = interface
            .as_object_mut()
            .ok_or_else(|| Error::Process("interface entries must be objects".into()))?;
        i.entry("model")
            .or_insert_with(|| Value::String("virtio-net-pci".into()));
    }
    Ok(())
}

fn default_graphics(devices: &mut Map<String, Value>) {
    if let Some(graphics) = devices.get_mut("graphics").and_then(Value::as_array_mut) {
        for graphic in graphics.iter_mut().filter_map(Value::as_object_mut) {
            graphic
                .entry("listen")
                .or_insert_with(|| Value::String("127.0.0.1".into()));
            graphic
                .entry("port")
                .or_insert_with(|| Value::String("auto".into()));
        }
    }
}

fn default_video(
    devices: &mut Map<String, Value>,
    architecture: Option<&Value>,
    machine: Option<&Value>,
) {
    let is_x86_q35 = architecture
        .and_then(Value::as_str)
        .is_none_or(|a| a == "x86_64")
        && machine
            .and_then(|m| m.get("type").and_then(Value::as_str))
            .is_none_or(|m| m.contains("q35"));
    if is_x86_q35 && !devices.contains_key("video") {
        devices.insert(
            "video".into(),
            serde_json::json!([{"id":"video0","model":"virtio-vga","primary":true,"heads":1}]),
        );
    } else if let Some(video) = devices.get_mut("video").and_then(Value::as_array_mut) {
        for v in video.iter_mut().filter_map(Value::as_object_mut) {
            v.entry("model")
                .or_insert_with(|| Value::String("virtio-vga".into()));
        }
    }
}

fn default_serial(devices: &mut Map<String, Value>) {
    if let Some(serial) = devices.get_mut("serial").and_then(Value::as_array_mut) {
        for s in serial.iter_mut().filter_map(Value::as_object_mut) {
            s.entry("path")
                .or_insert_with(|| Value::String("sockets/serial.sock".into()));
        }
    }
}

fn default_channels(devices: &mut Map<String, Value>) {
    if let Some(channels) = devices.get_mut("channels").and_then(Value::as_array_mut) {
        for c in channels.iter_mut().filter_map(Value::as_object_mut) {
            if c.get("id").and_then(Value::as_str) == Some("qmp") {
                c.entry("path")
                    .or_insert_with(|| Value::String("sockets/qmp.sock".into()));
            }
        }
    }
}
fn validate(spec: &Map<String, Value>, image: &ImageManifestDocument) -> Result<()> {
    if spec
        .get("architecture")
        .and_then(Value::as_str)
        .is_some_and(str::is_empty)
    {
        return Err(Error::Process("architecture cannot be empty".into()));
    }
    if let (Some(profile_arch), Some(image_arch)) = (
        spec.get("architecture").and_then(Value::as_str),
        image.spec.get("architecture").and_then(Value::as_str),
    ) && profile_arch != image_arch
    {
        return Err(Error::Process(format!(
            "image architecture {image_arch} is incompatible with instance architecture {profile_arch}"
        )));
    }
    if let Some(machines) = image
        .spec
        .get("compatible_machines")
        .and_then(Value::as_array)
    {
        let machine = spec
            .get("machine")
            .and_then(|v| v.get("type").and_then(Value::as_str).or_else(|| v.as_str()));
        if let Some(machine) = machine
            && !machines.iter().filter_map(Value::as_str).any(|allowed| {
                machine == allowed || machine.starts_with(allowed) || allowed.starts_with(machine)
            })
        {
            return Err(Error::Process(format!(
                "machine {machine} is incompatible with image"
            )));
        }
    }
    validate_resources(spec)?;
    validate_devices(spec)?;
    validate_image_bindings(spec, image)?;
    validate_state_paths(spec)?;
    Ok(())
}

fn validate_state_paths(spec: &Map<String, Value>) -> Result<()> {
    let mut paths = Vec::new();
    if let Some(firmware) = spec.get("firmware").and_then(Value::as_object)
        && let Some(path) = firmware
            .get("nvram")
            .and_then(|v| v.get("path"))
            .and_then(Value::as_str)
    {
        paths.push(("firmware.nvram.path", path));
    }
    if let Some(devices) = spec.get("devices").and_then(Value::as_object) {
        for collection in ["serial", "channels"] {
            if let Some(items) = devices.get(collection).and_then(Value::as_array) {
                for item in items {
                    if let Some(path) = item.get("path").and_then(Value::as_str) {
                        paths.push((collection, path));
                    }
                }
            }
        }
    }
    for (field, path) in paths {
        let candidate = std::path::Path::new(path);
        if candidate.is_absolute()
            || candidate
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(Error::Process(format!(
                "{field} must remain inside the instance directory"
            )));
        }
    }
    Ok(())
}

fn validate_image_bindings(spec: &Map<String, Value>, image: &ImageManifestDocument) -> Result<()> {
    let components = image
        .spec
        .get("components")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let Some(devices) = spec.get("devices").and_then(Value::as_object) else {
        return Ok(());
    };
    if let Some(disks) = devices.get("disks").and_then(Value::as_array) {
        for disk in disks {
            let Some(component) = disk
                .pointer("/source/component")
                .and_then(Value::as_str)
                .or_else(|| {
                    disk.pointer("/source/image_component")
                        .and_then(Value::as_str)
                })
            else {
                continue;
            };
            let Some(documented) = components.get(component).and_then(Value::as_object) else {
                return Err(Error::Process(format!(
                    "disk references missing image component {component}"
                )));
            };
            if let Some(artifact) = documented.get("artifact") {
                validate_artifact(
                    artifact.as_object().ok_or_else(|| {
                        Error::Process(format!(
                            "image component {component}.artifact must be an object"
                        ))
                    })?,
                    component,
                )?;
            } else {
                return Err(Error::Process(format!(
                    "image component {component} has no immutable artifact"
                )));
            }
        }
    }
    if let Some(firmware) = spec.get("firmware").and_then(Value::as_object) {
        for part in ["loader", "nvram"] {
            let Some(section) = firmware.get(part).and_then(Value::as_object) else {
                continue;
            };
            if let Some(artifact) = section.get("artifact") {
                validate_artifact(
                    artifact.as_object().ok_or_else(|| {
                        Error::Process(format!("firmware.{part}.artifact must be an object"))
                    })?,
                    part,
                )?;
            }
            if let Some(template) = section.get("template").and_then(Value::as_object)
                && let Some(artifact) = template.get("artifact").and_then(Value::as_object)
            {
                validate_artifact(artifact, part)?;
            }
        }
    }
    Ok(())
}

fn validate_artifact(artifact: &Map<String, Value>, name: &str) -> Result<()> {
    let digest = artifact
        .get("digest")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Process(format!("artifact for {name} has no digest")))?;
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::Process(format!(
            "artifact for {name} has invalid SHA-256 digest"
        )));
    }
    let path = artifact
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Process(format!("artifact for {name} has no path")))?;
    if path != format!("objects/sha256/{hex}") {
        return Err(Error::Process(format!(
            "artifact for {name} is not bound to its immutable object"
        )));
    }
    Ok(())
}

fn validate_identity_source_compatibility(
    spec: &Map<String, Value>,
    identity: &HardwareIdentityDocument,
) -> Result<()> {
    let Some(compatibility) = identity
        .spec
        .get("compatibility")
        .and_then(Value::as_object)
    else {
        return Ok(());
    };
    if let (Some(required), Some(actual)) = (
        compatibility.get("architecture").and_then(Value::as_str),
        spec.get("architecture").and_then(Value::as_str),
    ) && required != actual
    {
        return Err(Error::Process(format!(
            "hardware identity architecture {required} is incompatible with instance architecture {actual}"
        )));
    }
    if let Some(machines) = compatibility.get("machines").and_then(Value::as_array) {
        let machine = spec
            .get("machine")
            .and_then(|v| v.get("type").and_then(Value::as_str).or_else(|| v.as_str()));
        if let Some(machine) = machine
            && !machines
                .iter()
                .filter_map(Value::as_str)
                .any(|m| machine == m || machine.starts_with(m) || m.starts_with(machine))
        {
            return Err(Error::Process(format!(
                "hardware identity machine {machine} is incompatible"
            )));
        }
    }
    if let Some(tracks) = compatibility.get("engine_tracks").and_then(Value::as_array) {
        let track = spec
            .get("engine")
            .and_then(|v| v.get("track"))
            .and_then(Value::as_str);
        if let Some(track) = track
            && !tracks.iter().filter_map(Value::as_str).any(|t| t == track)
        {
            return Err(Error::Process(format!(
                "hardware identity engine track {track} is incompatible"
            )));
        }
    }
    if let (Some(required), Some(actual)) = (
        compatibility.get("patch_revision").and_then(Value::as_str),
        spec.get("engine")
            .and_then(|v| v.get("patch_revision"))
            .and_then(Value::as_str),
    ) && required != actual
    {
        return Err(Error::Process(format!(
            "hardware identity patch revision {required} is incompatible"
        )));
    }
    Ok(())
}

fn validate_resources(spec: &Map<String, Value>) -> Result<()> {
    let Some(resources) = spec.get("resources") else {
        return Ok(());
    };
    let resources = resources
        .as_object()
        .ok_or_else(|| Error::Process("resources must be object".into()))?;
    let vcpus = resources
        .get("vcpus")
        .ok_or_else(|| Error::Process("resources.vcpus is required".into()))?;
    let (count, sockets, cores, threads) = if let Some(n) = vcpus.as_u64() {
        (n, 1, n, 1)
    } else {
        let o = vcpus
            .as_object()
            .ok_or_else(|| Error::Process("resources.vcpus must be number or object".into()))?;
        (
            o.get("count")
                .and_then(Value::as_u64)
                .ok_or_else(|| Error::Process("resources.vcpus.count is required".into()))?,
            o.get("sockets").and_then(Value::as_u64).unwrap_or(1),
            o.get("cores").and_then(Value::as_u64).unwrap_or(1),
            o.get("threads").and_then(Value::as_u64).unwrap_or(1),
        )
    };
    if count == 0
        || sockets == 0
        || cores == 0
        || threads == 0
        || sockets.saturating_mul(cores).saturating_mul(threads) != count
    {
        return Err(Error::Process(format!(
            "vCPU topology does not match count {count}"
        )));
    }
    Ok(())
}

fn validate_devices(spec: &Map<String, Value>) -> Result<()> {
    let Some(devices) = spec.get("devices") else {
        return Ok(());
    };
    let devices = devices
        .as_object()
        .ok_or_else(|| Error::Process("devices must be object".into()))?;
    let collections = [
        "disks",
        "interfaces",
        "controllers",
        "graphics",
        "video",
        "serial",
        "channels",
    ];
    let mut all_ids = BTreeSet::new();
    for collection in collections {
        let Some(items) = devices.get(collection) else {
            continue;
        };
        let items = items
            .as_array()
            .ok_or_else(|| Error::Process(format!("devices.{collection} must be array")))?;
        for item in items {
            let item = item.as_object().ok_or_else(|| {
                Error::Process(format!("devices.{collection} entries must be objects"))
            })?;
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    Error::Process(format!("devices.{collection} entries require id"))
                })?;
            if item.contains_key("remove") {
                return Err(Error::Process(format!(
                    "merge directive remains for device {id}"
                )));
            }
            if !all_ids.insert(id.to_string()) {
                return Err(Error::Process(format!("duplicate device id {id}")));
            }
            if collection == "interfaces"
                && let Some(mac) = item.get("mac").and_then(Value::as_str)
            {
                validate_mac(mac)?;
            }
            if let Some(address) = item.get("address").and_then(Value::as_str)
                && address.is_empty()
            {
                return Err(Error::Process(format!("device {id} has empty PCI address")));
            }
        }
    }
    let mut targets = BTreeSet::new();
    if let Some(disks) = devices.get("disks").and_then(Value::as_array) {
        for disk in disks {
            if let Some(dev) = disk
                .get("target")
                .and_then(|t| t.get("dev"))
                .and_then(Value::as_str)
                && !targets.insert(dev)
            {
                return Err(Error::Process(format!("duplicate disk target {dev}")));
            }
        }
    }
    if let Some(boot) = spec
        .get("boot")
        .and_then(|b| b.get("order"))
        .and_then(Value::as_array)
    {
        let ids: BTreeSet<&str> = devices
            .values()
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|v| v.get("id").and_then(Value::as_str))
            .collect();
        for entry in boot {
            let id = entry
                .as_str()
                .ok_or_else(|| Error::Process("boot.order entries must be strings".into()))?;
            if !ids.contains(id) {
                return Err(Error::Process(format!(
                    "boot references unknown device {id}"
                )));
            }
        }
    }
    Ok(())
}

fn validate_mac(mac: &str) -> Result<()> {
    let octets: Vec<&str> = mac.split(':').collect();
    if octets.len() != 6
        || octets
            .iter()
            .any(|part| part.len() != 2 || u8::from_str_radix(part, 16).is_err())
    {
        return Err(Error::Process(format!("invalid MAC address {mac}")));
    }
    Ok(())
}

fn generate_identities(spec: &mut Map<String, Value>, instance: &str) {
    let mut h = Sha256::new();
    h.update(instance.as_bytes());
    let d = h.finalize();
    let identity = spec
        .entry("hardware_identity")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(identity) = identity.as_object_mut() {
        let values = identity
            .entry("identity")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(values) = values.as_object_mut() {
            if !values.contains_key("uuid") {
                let mut uuid = [0u8; 16];
                uuid.copy_from_slice(&d[..16]);
                uuid[6] = (uuid[6] & 0x0f) | 0x40;
                uuid[8] = (uuid[8] & 0x3f) | 0x80;
                values.insert("uuid".into(), Value::String(format!(
                    "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                    uuid[0],uuid[1],uuid[2],uuid[3],uuid[4],uuid[5],uuid[6],uuid[7],uuid[8],uuid[9],uuid[10],uuid[11],uuid[12],uuid[13],uuid[14],uuid[15]
                )));
            }
            if !values.contains_key("system_serial") {
                values.insert(
                    "system_serial".into(),
                    Value::String(format!(
                        "ME-{:02X}{:02X}{:02X}{:02X}",
                        d[0], d[1], d[2], d[3]
                    )),
                );
            }
        }
    }
    if let Some(xs) = spec
        .get_mut("devices")
        .and_then(|v| v.get_mut("interfaces"))
        .and_then(Value::as_array_mut)
    {
        for (i, nic) in xs.iter_mut().enumerate() {
            if nic.get("mac").is_none() {
                let n = nic.as_object_mut().unwrap();
                n.insert(
                    "mac".into(),
                    Value::String(format!(
                        "52:54:00:{:02x}:{:02x}:{:02x}",
                        d[0],
                        d[1],
                        d[2].wrapping_add(i as u8)
                    )),
                );
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn docs(profile_spec: Value, image_spec: Value) -> (ProfileDocument, ImageDocument) {
        let mut image_spec = image_spec;
        if let Some(component) = image_spec
            .pointer_mut("/components/root_disk")
            .and_then(Value::as_object_mut)
            && !component.contains_key("artifact")
        {
            component.insert(
                "artifact".into(),
                serde_json::json!({
                    "digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                    "path": "objects/sha256/0000000000000000000000000000000000000000000000000000000000000000"
                }),
            );
        }
        (
            ProfileDocument {
                api_version: "machineemu.io/v1".into(),
                kind: "Profile".into(),
                metadata: DocumentMetadata {
                    name: "demo".into(),
                    revision: 1,
                    digest: None,
                },
                spec: profile_spec,
            },
            ImageDocument {
                api_version: "machineemu.io/v1".into(),
                kind: "Image".into(),
                metadata: DocumentMetadata {
                    name: "disk".into(),
                    revision: 1,
                    digest: None,
                },
                spec: image_spec,
            },
        )
    }
    fn context() -> ResolutionContext {
        ResolutionContext {
            instance_id: "vm-a".into(),
            engines: vec![EngineCapability {
                track: "qemu".into(),
                build_digest: Some(
                    "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                        .into(),
                ),
                executable: Some("/bin/true".into()),
                patch_revision: Some("p1".into()),
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    #[test]
    fn device_merge_preserves_siblings() {
        let mut v =
            serde_json::json!({"disks":[{"id":"root","target":{"bus":"virtio"}},{"id":"data"}]});
        merge_values(
            &mut v,
            &serde_json::json!({"disks":[{"id":"root","target":{"bus":"sata"}}]}),
        )
        .unwrap();
        assert_eq!(v["disks"][0]["target"]["bus"], "sata");
        assert_eq!(v["disks"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn compact_video_and_serial_objects_are_normalized_to_collections() {
        let (profile, image) = docs(
            serde_json::json!({
                "machine": "q35",
                "resources": {"vcpus": 1},
                "devices": {
                    "video": {"model": "vga", "primary": true},
                    "serial": {"type": "file"}
                }
            }),
            serde_json::json!({
                "components": {"root_disk": {"format": "qcow2"}},
                "compatible_engines": ["qemu"]
            }),
        );
        let resolved = resolve_instance(
            profile,
            None,
            image,
            CreateInstanceOverrides::default(),
            context(),
        )
        .unwrap();
        assert!(resolved.spec["devices"]["video"].is_array());
        assert_eq!(resolved.spec["devices"]["video"][0]["id"], "video0");
        assert!(resolved.spec["devices"]["serial"].is_array());
        assert_eq!(resolved.spec["devices"]["serial"][0]["id"], "serial0");
    }

    #[test]
    fn duplicate_device_ids_rejected() {
        let mut v = serde_json::json!({});
        assert!(
            merge_values(
                &mut v,
                &serde_json::json!({"interfaces":[{"id":"n"},{"id":"n"}]})
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_source_fields_are_rejected_before_resolution() {
        let (mut profile, image) = docs(
            serde_json::json!({"architecture":"x86_64", "typo": true}),
            serde_json::json!({"architecture":"x86_64"}),
        );
        let error = resolve_instance(
            profile.clone(),
            None,
            image.clone(),
            CreateInstanceOverrides::default(),
            context(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown field profile.spec.typo"), "{error}");

        profile.spec = serde_json::json!({"architecture":"x86_64"});
        let mut image = image;
        image.spec["unexpected"] = true.into();
        let error = resolve_instance(
            profile,
            None,
            image,
            CreateInstanceOverrides::default(),
            context(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("unknown field image.spec.unexpected"),
            "{error}"
        );
    }

    #[test]
    fn obsolete_identity_flags_are_rejected() {
        let (profile, image) = docs(
            serde_json::json!({"architecture":"x86_64"}),
            serde_json::json!({"architecture":"x86_64"}),
        );
        let identity = HardwareIdentityDocument {
            api_version: "machineemu.io/v1".into(),
            kind: "HardwareIdentity".into(),
            metadata: DocumentMetadata {
                name: "id".into(),
                revision: 1,
                digest: None,
            },
            spec: serde_json::json!({"analysis":{"telemetry":true}}),
        };
        let error = resolve_instance(
            profile,
            Some(identity),
            image,
            CreateInstanceOverrides::default(),
            context(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("telemetry"), "{error}");
    }

    #[test]
    fn image_hardware_fields_are_namespaced_and_engine_track_is_selected() {
        let profile = ProfileDocument {
            api_version: "machineemu.io/v1".into(),
            kind: "Profile".into(),
            metadata: DocumentMetadata {
                name: "demo".into(),
                revision: 1,
                digest: None,
            },
            spec: serde_json::json!({
                "architecture": "x86_64",
                "machine": {"type": "q35"},
                "engine": {"track": "qemu-analysis"}
            }),
        };
        let image = ImageDocument {
            api_version: "machineemu.io/v1".into(),
            kind: "Image".into(),
            metadata: DocumentMetadata {
                name: "disk".into(),
                revision: 1,
                digest: None,
            },
            spec: serde_json::json!({
                "architecture": "x86_64",
                "compatible_machines": ["q35"],
                "compatible_engines": ["qemu-analysis"],
                "components": {"root_disk": {"format": "qcow2", "artifact": {"digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000", "path": "objects/sha256/0000000000000000000000000000000000000000000000000000000000000000"}}}
            }),
        };
        let resolved = resolve_instance(
            profile,
            None,
            image,
            CreateInstanceOverrides::default(),
            ResolutionContext {
                engines: vec![EngineCapability {
                    track: "qemu-analysis".into(),
                    build_digest: Some(
                        "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                            .into(),
                    ),
                    executable: Some("/bin/true".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(resolved.spec["architecture"], "x86_64");
        assert_eq!(resolved.spec["engine"]["track"], "qemu-analysis");
        assert_eq!(resolved.spec["image"]["architecture"], "x86_64");
    }

    #[test]
    fn count_override_expands_topology_and_assigns_stable_distinct_macs() {
        let (profile, image) = docs(
            serde_json::json!({"architecture":"x86_64", "machine":{"type":"q35"}, "devices":{"interfaces":[{"id":"a"},{"id":"b"}]}}),
            serde_json::json!({"architecture":"x86_64", "components":{"root_disk":{"format":"qcow2"}}}),
        );
        let resolved = resolve_instance(
            profile,
            None,
            image,
            CreateInstanceOverrides {
                spec: serde_json::json!({"resources":{"vcpus":4}}),
            },
            context(),
        )
        .unwrap();
        assert_eq!(
            resolved.spec["resources"]["vcpus"],
            serde_json::json!({"count":4,"sockets":1,"cores":4,"threads":1})
        );
        assert_ne!(
            resolved.spec["devices"]["interfaces"][0]["mac"],
            resolved.spec["devices"]["interfaces"][1]["mac"]
        );
        assert_eq!(resolved.spec["devices"]["disks"][0]["target"]["dev"], "vda");
    }

    #[test]
    fn explicit_incompatible_topology_is_rejected() {
        let (profile, image) = docs(
            serde_json::json!({"architecture":"x86_64", "machine":{"type":"q35"}, "resources":{"vcpus":{"count":4,"sockets":1,"cores":2,"threads":1}}}),
            serde_json::json!({"architecture":"x86_64"}),
        );
        let error = resolve_instance(
            profile,
            None,
            image,
            CreateInstanceOverrides::default(),
            context(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("topology"), "{error}");
    }

    #[test]
    fn identity_compatibility_is_enforced() {
        let (profile, image) = docs(
            serde_json::json!({"architecture":"x86_64", "machine":{"type":"q35"}}),
            serde_json::json!({"architecture":"x86_64"}),
        );
        let identity = HardwareIdentityDocument {
            api_version: "machineemu.io/v1".into(),
            kind: "HardwareIdentity".into(),
            metadata: DocumentMetadata {
                name: "id".into(),
                revision: 1,
                digest: None,
            },
            spec: serde_json::json!({"compatibility":{"architecture":"aarch64"}}),
        };
        let error = resolve_instance(
            profile,
            Some(identity),
            image,
            CreateInstanceOverrides::default(),
            context(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("architecture"), "{error}");
    }
}
