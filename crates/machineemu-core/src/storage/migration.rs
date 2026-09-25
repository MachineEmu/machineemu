//! Offline conversion of the pre-domain workspace layout.
//!
//! The daemon does not use this module to read old documents.  It is deliberately
//! a small, filesystem-only converter so a repository tree and a live workspace
//! can be converted by the same code before the new runtime is deployed.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationEntry {
    pub source: String,
    pub destination: String,
    pub kind: String,
    pub disposition: String,
    pub digest: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MigrationOptions {
    /// Write converted documents. When false this is a preflight only.
    pub apply: bool,
    /// Remove the old JSON documents after every conversion succeeds.
    pub remove_legacy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MigrationReport {
    pub source_root: String,
    pub entries: Vec<MigrationEntry>,
    pub preserved_paths: Vec<String>,
    pub errors: Vec<String>,
}

/// Inventory and, when requested, convert legacy documents below `root`.
///
/// The operation is idempotent: an existing destination must contain the same
/// bytes, otherwise migration fails rather than overwriting an operator edit.
/// A journal is written last, making an interrupted run safe to repeat.
pub fn migrate_legacy_tree(
    root: impl AsRef<Path>,
    options: MigrationOptions,
) -> Result<MigrationReport> {
    let root = root.as_ref();
    if !root.is_dir() {
        return Err(Error::WorkspaceNotDirectory(root.to_owned()));
    }
    let mut report = MigrationReport {
        source_root: root.display().to_string(),
        ..Default::default()
    };
    // Keep the journal as an append-only migration record.  A retry may find
    // no legacy files (or only a newly discovered one); replacing the journal
    // in that case would erase evidence of the earlier conversion.
    let journal = root.join("migration-v1.json");
    let previous = if options.apply && journal.is_file() {
        let bytes = fs::read(&journal).map_err(io(&journal))?;
        Some(
            serde_json::from_slice::<MigrationReport>(&bytes).map_err(|error| {
                Error::Process(format!(
                    "invalid migration journal {}: {error}",
                    journal.display()
                ))
            })?,
        )
    } else {
        None
    };
    let mut jobs = Vec::new();
    collect_profiles(root, &mut jobs)?;
    collect_images(root, &mut jobs)?;
    collect_instances(root, &mut jobs)?;
    for (source, destination, kind) in jobs {
        match convert_one(
            &source,
            &destination,
            &kind,
            root,
            options.apply,
            options.remove_legacy,
        ) {
            Ok(entry) => report.entries.push(entry),
            Err(error) => report.errors.push(format!("{}: {error}", source.display())),
        }
    }
    if options.apply && report.errors.is_empty() {
        if let Some(previous) = previous {
            for entry in previous.entries {
                if !report.entries.iter().any(|current| {
                    current.source == entry.source
                        && current.destination == entry.destination
                        && current.digest == entry.digest
                }) {
                    report.entries.push(entry);
                }
            }
            for path in previous.preserved_paths {
                if !report.preserved_paths.contains(&path) {
                    report.preserved_paths.push(path);
                }
            }
        }
        let bytes = serde_json::to_vec_pretty(&report)?;
        atomic_write(&journal, &bytes)?;
    }
    if report.errors.is_empty() {
        Ok(report)
    } else {
        Err(Error::Process(report.errors.join("; ")))
    }
}

type Job = (PathBuf, PathBuf, String);

fn collect_profiles(root: &Path, jobs: &mut Vec<Job>) -> Result<()> {
    let dir = root.join("profiles");
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&dir).map_err(io(&dir))? {
        let path = entry.map_err(io(&dir))?.path();
        if path.extension().and_then(|x| x.to_str()) == Some("json") {
            jobs.push((path.clone(), path.with_extension("yaml"), "Profile".into()));
        }
    }
    Ok(())
}

fn collect_images(root: &Path, jobs: &mut Vec<Job>) -> Result<()> {
    let dir = root.join("images");
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&dir).map_err(io(&dir))? {
        let path = entry.map_err(io(&dir))?.path();
        if !path.is_dir() {
            continue;
        }
        // Embedded profile documents are independent migration units.  A
        // previous interrupted run may already have converted the image
        // manifest, so requiring manifest.json here would strand profile.json
        // indefinitely and leave a legacy reader path behind.
        if path.join("manifest.json").is_file() {
            jobs.push((
                path.join("manifest.json"),
                path.join("manifest.yaml"),
                "Image".into(),
            ));
        }
        if path.join("manifest.yaml").is_file() {
            jobs.push((
                path.join("manifest.yaml"),
                path.join("manifest.yaml"),
                "Image".into(),
            ));
        }
        if path.join("profile.json").is_file() {
            jobs.push((
                path.join("profile.json"),
                path.join("profile.yaml"),
                "Profile".into(),
            ));
        }
    }
    Ok(())
}

fn collect_instances(root: &Path, jobs: &mut Vec<Job>) -> Result<()> {
    let dir = root.join("instances");
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&dir).map_err(io(&dir))? {
        let path = entry.map_err(io(&dir))?.path();
        if path.is_dir() {
            for name in ["instance.json", "instance.yml", "instance.yaml"] {
                let source = path.join(name);
                if source.is_file() {
                    jobs.push((source, path.join("instance.yaml"), "Instance".into()));
                    break;
                }
            }
            for item in fs::read_dir(&path).map_err(io(&path))? {
                let item = item.map_err(io(&path))?.path();
                if item.file_name().and_then(|x| x.to_str()) == Some("profile.json") {
                    jobs.push((item.clone(), item.with_extension("yaml"), "Profile".into()));
                } else if item.file_name().and_then(|x| x.to_str()).is_some_and(|x| {
                    x != "instance.json" && x != "instance.yaml" && x != "instance.yml"
                }) && item.is_file()
                {
                    // State files are intentionally preserved.
                }
            }
        }
    }
    Ok(())
}

fn convert_one(
    source: &Path,
    destination: &Path,
    kind: &str,
    root: &Path,
    apply: bool,
    remove: bool,
) -> Result<MigrationEntry> {
    reject_non_regular(source)?;
    let bytes = fs::read(source).map_err(io(source))?;
    let value: Value = if source.extension().and_then(|x| x.to_str()) == Some("json") {
        serde_json::from_slice(&bytes)?
    } else {
        serde_yaml::from_slice(&bytes).map_err(|e| Error::Process(e.to_string()))?
    };
    let mut converted = convert_document(value, kind, source)?;
    if kind == "Image" {
        bind_image_artifacts(&mut converted, source, root)?;
    }
    let output = serde_yaml::to_string(&converted).map_err(|e| Error::Process(e.to_string()))?;
    let output = output.into_bytes();
    let digest = format!("sha256:{:x}", Sha256::digest(&output));
    let disposition = if source == destination && apply {
        atomic_write(destination, &output)?;
        "updated"
    } else if fs::symlink_metadata(destination).is_ok() {
        reject_non_symlink(destination)?;
        if fs::read(destination).map_err(io(destination))? != output {
            return Err(Error::Process("destination differs from conversion".into()));
        }
        "already-converted"
    } else if apply {
        atomic_write(destination, &output)?;
        "converted"
    } else {
        "planned"
    };
    if apply && remove && source != destination {
        fs::remove_file(source).map_err(io(source))?;
    }
    let relative = |path: &Path| {
        path.strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string()
    };
    Ok(MigrationEntry {
        source: relative(source),
        destination: relative(destination),
        kind: kind.into(),
        disposition: disposition.into(),
        digest,
    })
}

fn bind_image_artifacts(document: &mut Value, source: &Path, root: &Path) -> Result<()> {
    let Some(components) = document
        .pointer_mut("/spec/components")
        .and_then(Value::as_object_mut)
    else {
        return Ok(());
    };
    for component in components.values_mut() {
        let Some(object) = component.as_object_mut() else {
            continue;
        };
        let Some(raw_path) = object.get("path").and_then(Value::as_str) else {
            continue;
        };
        let digest = object
            .get("sha256")
            .and_then(Value::as_str)
            .map(|s| s.strip_prefix("sha256:").unwrap_or(s).to_owned());
        let Some(digest) = digest else {
            continue;
        };
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Process(format!(
                "invalid image component digest {digest}"
            )));
        }
        let path = PathBuf::from(raw_path);
        let source_path = if path.is_absolute() {
            path
        } else {
            source.parent().unwrap_or(root).join(path)
        };
        if !source_path.is_file() {
            return Err(Error::Process(format!(
                "image component is unavailable: {}",
                source_path.display()
            )));
        }
        let actual = file_digest(&source_path)?;
        if actual != digest {
            return Err(Error::Process(format!(
                "image component digest mismatch for {}: declared sha256:{digest}, found sha256:{actual}",
                source_path.display()
            )));
        }
        let object_path = root.join("objects").join("sha256").join(&digest);
        if !object_path.is_file() {
            let parent = object_path.parent().expect("object path parent");
            fs::create_dir_all(parent).map_err(io(parent))?;
            fs::copy(&source_path, &object_path).map_err(io(&object_path))?;
        }
        object.insert("artifact".into(), json!({"digest": format!("sha256:{digest}"), "path": format!("objects/sha256/{digest}")}));
    }
    Ok(())
}

fn file_digest(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).map_err(io(path))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io(path))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn convert_document(mut value: Value, kind: &str, source: &Path) -> Result<Value> {
    if !value.is_object() {
        return Err(Error::Process(format!(
            "{kind} document must be a mapping: {}",
            source.display()
        )));
    }
    let is_new = value.get("api_version").and_then(Value::as_str) == Some("machineemu.io/v1")
        && value.get("kind").and_then(Value::as_str) == Some(kind);
    if is_new {
        if kind == "Instance" {
            validate_complete_instance(&value, source)?;
        }
        return Ok(value);
    }
    let required_id = match kind {
        "Profile" => value.get("id").and_then(Value::as_str),
        "Image" => value.get("image_id").and_then(Value::as_str),
        "Instance" => value.get("instance_id").and_then(Value::as_str),
        _ => None,
    };
    if required_id.is_none() {
        return Err(Error::Process(format!(
            "legacy {kind} document has no stable identifier (expected {})",
            match kind {
                "Profile" => "id",
                "Image" => "image_id",
                "Instance" => "instance_id",
                _ => "identifier",
            }
        )));
    }
    // An old instance envelope contains a launch plan and a copy of the
    // creation profile.  Neither is sufficient to reconstruct a domain: the
    // launch plan has already lost field ownership and the profile is only a
    // partial template.  Only accept an embedded, already-resolved domain
    // snapshot.  Silently wrapping the envelope in `spec` would publish a
    // document which looks migrated but cannot be started without rereading
    // the old sources.
    if kind == "Instance" {
        return convert_legacy_instance(value, source);
    }

    let mut metadata = Map::new();
    let name = value
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| value.get("instance_id").and_then(Value::as_str))
        .or_else(|| value.get("image_id").and_then(Value::as_str))
        .or_else(|| {
            source
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|x| x.to_str())
        })
        .unwrap_or("unnamed");
    metadata.insert("name".into(), Value::String(name.into()));
    if let Some(revision) = value.get("revision").cloned() {
        metadata.insert("revision".into(), revision);
    }
    if let Some(object) = value.as_object_mut() {
        // The old envelope version describes the source format, not the
        // contents of the new v1 spec. Keeping it would make the migrated
        // document appear to be a hybrid of two schemas.
        object.remove("schema_version");
    }
    if kind == "Profile"
        && let Some(o) = value.as_object_mut()
    {
        o.remove("id");
        o.remove("name");
        if let Some(analysis) = o.get_mut("analysis").and_then(Value::as_object_mut) {
            analysis.remove("telemetry");
            analysis.remove("overlay");
        }
    }
    Ok(json!({"api_version":"machineemu.io/v1", "kind":kind, "metadata":metadata, "spec":value}))
}

/// Convert the historical lifecycle envelope only when it carries a complete
/// resolver result.  The old `launch_plan` and `profile` fields are retained as
/// provenance in neither the new runtime document nor its spec: they are
/// inputs, not a domain snapshot.
fn convert_legacy_instance(mut value: Value, source: &Path) -> Result<Value> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| Error::Process("legacy Instance document must be a mapping".into()))?;
    let instance_id = object
        .get("instance_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::Process(format!(
                "legacy Instance document has no stable identifier (expected instance_id): {}",
                source.display()
            ))
        })?
        .to_owned();

    // `domain_document` was used by the staged cutover implementation and is
    // the only lossless bridge from the envelope format.  Accept the alias
    // `resolved_document` too because early migration rehearsals used that
    // name; both must contain the same complete v1 document when present.
    let embedded = object
        .remove("domain_document")
        .or_else(|| object.remove("resolved_document"));
    let Some(mut document) = embedded else {
        return Err(unresolved_instance_error(
            source,
            &instance_id,
            &[
                "spec",
                "spec.engine",
                "spec.machine",
                "spec.firmware",
                "spec.resources",
                "spec.devices",
                "spec.image",
            ],
            "legacy launch_plan/profile fields do not contain a complete domain snapshot",
        ));
    };
    validate_complete_instance(&document, source)?;

    let root = document
        .as_object_mut()
        .ok_or_else(|| Error::Process("embedded resolved Instance must be a mapping".into()))?;
    let metadata = root
        .entry("metadata")
        .or_insert_with(|| Value::Object(Map::new()));
    let metadata = metadata
        .as_object_mut()
        .ok_or_else(|| Error::Process("resolved Instance metadata must be a mapping".into()))?;
    let metadata_name = metadata
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Process("resolved Instance metadata.name is required".into()))?;
    if metadata_name != instance_id {
        return Err(Error::Process(format!(
            "{}: resolved Instance metadata.name {metadata_name:?} does not match instance_id {instance_id:?}",
            source.display()
        )));
    }

    // Preserve lifecycle state that lived beside the old launch plan.  The
    // domain snapshot remains authoritative for hardware and launch inputs;
    // status is allowed to carry bookkeeping which does not affect rendering.
    if let Some(status) = object.remove("status") {
        root.entry("status").or_insert(status);
    }
    if let Some(auto_remove) = object.get("auto_remove").cloned() {
        let status = root
            .entry("status")
            .or_insert_with(|| Value::Object(Map::new()));
        let status = status
            .as_object_mut()
            .ok_or_else(|| Error::Process("resolved Instance status must be a mapping".into()))?;
        status.entry("auto_remove").or_insert(auto_remove);
    }
    if let Some(profile_id) = object.get("profile_id").and_then(Value::as_str) {
        add_source_ref(root, "profile", profile_id);
    }
    if let Some(image_id) = object.get("image_id").and_then(Value::as_str) {
        add_source_ref(root, "image", image_id);
    }
    Ok(document)
}

fn add_source_ref(root: &mut Map<String, Value>, kind: &str, id: &str) {
    let source = root
        .entry("source")
        .or_insert_with(|| Value::Object(Map::new()));
    let source = source.as_object_mut().expect("source created as object");
    source
        .entry(kind)
        .or_insert_with(|| json!({"id": id, "revision": 0}));
}

fn validate_complete_instance(value: &Value, source: &Path) -> Result<()> {
    let Some(root) = value.as_object() else {
        return Err(Error::Process(format!(
            "{}: resolved Instance must be a mapping",
            source.display()
        )));
    };
    let mut unresolved = Vec::new();
    if root.get("api_version").and_then(Value::as_str) != Some("machineemu.io/v1") {
        unresolved.push("api_version".to_owned());
    }
    if root.get("kind").and_then(Value::as_str) != Some("Instance") {
        unresolved.push("kind".to_owned());
    }
    let Some(metadata) = root.get("metadata").and_then(Value::as_object) else {
        unresolved.push("metadata".to_owned());
        return Err(unresolved_instance_error(
            source,
            "unknown",
            &unresolved.iter().map(String::as_str).collect::<Vec<_>>(),
            "resolved Instance metadata is missing",
        ));
    };
    if metadata.get("name").and_then(Value::as_str).is_none() {
        unresolved.push("metadata.name".to_owned());
    }
    let Some(spec) = root.get("spec").and_then(Value::as_object) else {
        unresolved.push("spec".to_owned());
        return Err(unresolved_instance_error(
            source,
            metadata
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            &unresolved.iter().map(String::as_str).collect::<Vec<_>>(),
            "resolved Instance spec is missing",
        ));
    };
    for key in [
        "architecture",
        "engine",
        "machine",
        "firmware",
        "resources",
        "devices",
        "image",
    ] {
        if !spec.contains_key(key) {
            unresolved.push(format!("spec.{key}"));
        }
    }
    if !unresolved.is_empty() {
        let refs = unresolved.iter().map(String::as_str).collect::<Vec<_>>();
        return Err(unresolved_instance_error(
            source,
            metadata
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            &refs,
            "resolved Instance is incomplete",
        ));
    }
    Ok(())
}

fn unresolved_instance_error(
    source: &Path,
    instance_id: &str,
    fields: &[&str],
    reason: &str,
) -> Error {
    Error::Process(format!(
        "{}: instance {instance_id:?} cannot be migrated: {reason}; unresolved fields: {}",
        source.display(),
        fields.join(", ")
    ))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Process("migration destination has no parent".into()))?;
    fs::create_dir_all(parent).map_err(io(parent))?;
    // A migration can be resumed after a process crash, and the offline
    // command is often run from automation.  Do not let two invocations
    // share a predictable temporary pathname or publish a file whose bytes
    // have not reached stable storage.
    let temporary = parent.join(format!(
        ".migration-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(io(&temporary))?;
        use std::io::Write;
        file.write_all(bytes).map_err(io(&temporary))?;
        file.sync_all().map_err(io(&temporary))?;
        fs::rename(&temporary, path).map_err(io(path))?;
        // The rename is not durable until the containing directory is synced.
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(io(parent))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn reject_non_regular(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(io(path))?;
    if !metadata.file_type().is_file() {
        return Err(Error::Process(format!(
            "migration source must be a regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn reject_non_symlink(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(io(path))?;
    if metadata.file_type().is_symlink() {
        return Err(Error::Process(format!(
            "migration destination must not be a symlink: {}",
            path.display()
        )));
    }
    Ok(())
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_conversion_is_idempotent_and_keeps_state_files() {
        let root = std::env::temp_dir().join(format!(
            "machineemu-migration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("profiles")).unwrap();
        fs::write(
            root.join("profiles/default.json"),
            br#"{"schema_version":2,"id":"default","machine":"q35"}"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("instances/vm01")).unwrap();
        fs::write(root.join("instances/vm01/overlay.qcow2"), b"state").unwrap();
        let first = migrate_legacy_tree(
            &root,
            MigrationOptions {
                apply: true,
                remove_legacy: false,
            },
        )
        .unwrap();
        assert_eq!(first.entries.len(), 1);
        // A later run can discover another legacy document.  Its journal must
        // retain the first conversion as well as recording the new one.
        fs::write(
            root.join("profiles/second.json"),
            br#"{"schema_version":2,"id":"second","machine":"q35"}"#,
        )
        .unwrap();
        let second = migrate_legacy_tree(
            &root,
            MigrationOptions {
                apply: true,
                remove_legacy: true,
            },
        )
        .unwrap();
        assert_eq!(second.entries.len(), 2);
        assert!(
            second
                .entries
                .iter()
                .any(|entry| entry.source == "profiles/default.json")
        );
        assert!(
            second
                .entries
                .iter()
                .any(|entry| entry.source == "profiles/second.json")
        );
        assert!(!root.join("profiles/default.json").is_file());
        assert!(root.join("instances/vm01/overlay.qcow2").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn instance_conversion_rejects_lossy_launch_plan_envelope() {
        let source = PathBuf::from("/workspace/instances/guest-a/instance.json");
        let error = convert_document(
            serde_json::json!({
                "schema_version": 1,
                "instance_id": "saved-id",
                "image_id": "base",
                "profile_id": "default",
                "launch_plan": {"argv": ["qemu"]}
            }),
            "Instance",
            &source,
        )
        .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("saved-id"));
        assert!(text.contains("spec.engine"));
        assert!(text.contains("spec.devices"));
        assert!(text.contains("launch_plan/profile"));
    }

    #[test]
    fn instance_conversion_promotes_complete_embedded_domain_and_preserves_state() {
        let source = PathBuf::from("/workspace/instances/guest-a/instance.json");
        let converted = convert_document(
            serde_json::json!({
                "schema_version": 1,
                "instance_id": "saved-id",
                "image_id": "base",
                "profile_id": "default",
                "auto_remove": true,
                "status": {"state": "stopped"},
                "domain_document": {
                    "api_version": "machineemu.io/v1",
                    "kind": "Instance",
                    "metadata": {"name": "saved-id", "revision": 7},
                    "spec": {
                        "architecture": "x86_64",
                        "engine": {"track": "qemu-system"},
                        "machine": {"type": "q35"},
                        "firmware": {"type": "bios"},
                        "resources": {"memory": {"bytes": 1}, "vcpus": {"count": 1}},
                        "devices": {"disks": [], "interfaces": []},
                        "image": {"name": "base"}
                    }
                }
            }),
            "Instance",
            &source,
        )
        .unwrap();
        assert_eq!(converted["metadata"]["name"], "saved-id");
        assert_eq!(converted["metadata"]["revision"], 7);
        assert_eq!(converted["source"]["profile"]["id"], "default");
        assert_eq!(converted["source"]["image"]["id"], "base");
        assert_eq!(converted["status"]["state"], "stopped");
        assert_eq!(converted["status"]["auto_remove"], true);
    }

    #[test]
    fn incomplete_v1_instance_is_rejected_with_all_unresolved_fields() {
        let source = PathBuf::from("/workspace/instances/guest-a/instance.yaml");
        let error = convert_document(
            serde_json::json!({
                "api_version": "machineemu.io/v1",
                "kind": "Instance",
                "metadata": {"name": "guest-a"},
                "spec": {"architecture": "x86_64"}
            }),
            "Instance",
            &source,
        )
        .unwrap_err();
        let text = error.to_string();
        for field in [
            "spec.engine",
            "spec.machine",
            "spec.firmware",
            "spec.resources",
            "spec.devices",
            "spec.image",
        ] {
            assert!(text.contains(field), "missing {field} in {text}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn migration_rejects_symlink_sources() {
        let root = std::env::temp_dir().join(format!(
            "machineemu-migration-symlink-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("profiles")).unwrap();
        let real = root.join("real.json");
        fs::write(&real, br#"{"id":"default"}"#).unwrap();
        std::os::unix::fs::symlink(&real, root.join("profiles/default.json")).unwrap();
        let error = migrate_legacy_tree(&root, MigrationOptions::default()).unwrap_err();
        assert!(error.to_string().contains("regular file"));
        fs::remove_dir_all(root).unwrap();
    }
}
