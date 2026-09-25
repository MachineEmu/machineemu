//! Instance files own configuration; SQLite only owns runtime bookkeeping.
use super::Workspace;
use crate::{
    Error, Result,
    domain::{Id, configuration::InstanceDocument as DomainInstanceDocument},
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceDocument {
    pub schema_version: u32,
    pub instance_id: String,
    pub image_id: String,
    /// Creation template provenance. The shared template is never read at start.
    #[serde(default = "custom_profile_id")]
    pub profile_id: String,
    #[serde(default)]
    pub auto_remove: bool,
    pub profile: Option<Value>,
    pub launch_plan: Option<Value>,
    /// Complete v1 domain document. New creation stores this snapshot and
    /// later start/render operations must use it without rereading sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_document: Option<Value>,
}

fn custom_profile_id() -> String {
    "custom".into()
}

impl InstanceDocument {
    /// Content-based revision also detects edits made outside the daemon.
    pub fn revision(&self) -> Result<i64> {
        let hash = Sha256::digest(serde_json::to_vec(self)?);
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&hash[..8]);
        // Keep the token exact in JavaScript JSON clients.
        Ok(i64::from_be_bytes(bytes) & ((1_i64 << 53) - 1))
    }
}

impl Workspace {
    /// Read the resolved domain document produced by the offline migration or
    /// by the resolver-backed create path.  This is deliberately separate from
    /// [`instance_document`], whose shape is the old lifecycle configuration
    /// envelope and is retained only while callers are being cut over.
    pub fn domain_instance_document(&self, id: &Id) -> Result<DomainInstanceDocument> {
        let instance = self.instance(id)?;
        let directory = self.root.join("instances").join(id.as_str());
        let path = directory.join("instance.yaml");
        self.check_configuration_path(&path)?;
        let value = crate::engine::load_document(&path)
            .map_err(|error| Error::Process(error.to_string()))?;
        if value.get("api_version").and_then(Value::as_str) == Some("machineemu.io/v1")
            && value.get("kind").and_then(Value::as_str) == Some("Instance")
        {
            return Self::validate_domain_instance_document(value, &instance);
        }

        let envelope = self.instance_document(id)?;
        if let Some(value) = envelope.domain_document {
            return Self::validate_domain_instance_document(value, &instance);
        }

        Err(Error::Process(
            "instance.yaml does not contain a complete resolved domain document".into(),
        ))
    }

    fn validate_domain_instance_document(
        value: Value,
        instance: &crate::domain::Instance,
    ) -> Result<DomainInstanceDocument> {
        let document: DomainInstanceDocument = serde_json::from_value(value).map_err(|error| {
            Error::Process(format!("invalid resolved instance document: {error}"))
        })?;
        if document.api_version != "machineemu.io/v1" || document.kind != "Instance" {
            return Err(Error::Process(
                "resolved instance document must have api_version machineemu.io/v1 and kind Instance".into(),
            ));
        }
        if document.metadata.name != instance.instance_id.as_str() {
            return Err(Error::Process(
                "resolved instance metadata.name does not match the instance ID".into(),
            ));
        }
        Ok(document)
    }

    /// Whether this instance has completed the domain-document cutover.
    pub fn has_domain_instance_document(&self, id: &Id) -> Result<bool> {
        let path = self
            .root
            .join("instances")
            .join(id.as_str())
            .join("instance.yaml");
        if fs::symlink_metadata(&path).is_ok() {
            self.check_configuration_path(&path)?;
            let value = crate::engine::load_document(&path)
                .map_err(|error| Error::Process(error.to_string()))?;
            if value.get("api_version").and_then(Value::as_str) == Some("machineemu.io/v1")
                && value.get("kind").and_then(Value::as_str) == Some("Instance")
            {
                return Ok(true);
            }
        }
        if self.instance_document(id)?.domain_document.is_some() {
            return Ok(true);
        }
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                self.check_configuration_path(&path)?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    pub(super) fn read_staged_profile(&self, directory: &Path) -> Result<Option<Value>> {
        let path = directory.join("profile.yaml");
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::Io { path, source }),
        }
        self.check_configuration_path(&path)?;
        let value = crate::engine::load_document(&path)
            .map_err(|error| Error::Process(error.to_string()))?;
        Ok(Some(value))
    }

    pub fn instance_document_path(&self, id: &Id) -> Result<PathBuf> {
        let directory = self.root.join("instances").join(id.as_str());
        let mut selected = None;
        for name in ["instance.yaml", "instance.yml"] {
            let path = directory.join(name);
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    if selected.is_some() {
                        return Err(Error::Process(format!(
                            "multiple instance documents in {}; keep exactly one YAML file",
                            directory.display()
                        )));
                    }
                    selected = Some(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::Io { path, source }),
            }
        }
        Ok(selected.unwrap_or_else(|| directory.join("instance.yaml")))
    }

    fn check_configuration_path(&self, path: &Path) -> Result<()> {
        let root = self.root.canonicalize().map_err(|source| Error::Io {
            path: self.root.clone(),
            source,
        })?;
        let canonical = path.canonicalize().map_err(|source| Error::Io {
            path: path.into(),
            source,
        })?;
        let parent = path
            .parent()
            .unwrap_or(&self.root)
            .canonicalize()
            .map_err(|source| Error::Io {
                path: path.into(),
                source,
            })?;
        if !parent.starts_with(root) || !canonical.starts_with(parent) {
            return Err(Error::Process(
                "configuration path escapes its owning directory".into(),
            ));
        }
        Ok(())
    }

    pub fn instance_document(&self, id: &Id) -> Result<InstanceDocument> {
        let instance = self.instance(id)?;
        let path = self.instance_document_path(id)?;
        self.check_configuration_path(&path)?;
        let mut value = crate::engine::load_document(&path)
            .map_err(|error| Error::Process(error.to_string()))?;
        // API exports include an optimistic-concurrency token, not stored settings.
        if let Some(object) = value.as_object_mut() {
            object.remove("revision");
        }
        let document: InstanceDocument = serde_json::from_value(value)?;
        if document.schema_version != 1 {
            return Err(Error::Process(
                "instance document schema_version must be 1".into(),
            ));
        }
        if document.instance_id != id.as_str()
            || document.image_id != instance.image_id.as_str()
            || document.profile_id != instance.profile_id.as_str()
        {
            return Err(Error::Process(
                "instance, image and template IDs cannot change on an existing instance".into(),
            ));
        }
        if document.auto_remove
            && document.launch_plan.is_none()
            && document.domain_document.is_none()
        {
            return Err(Error::Process(
                "auto_remove requires a launch plan or complete domain document".into(),
            ));
        }
        if let Some(profile_id) = document
            .profile
            .as_ref()
            .and_then(|p| p.get("id"))
            .and_then(Value::as_str)
            && profile_id != document.profile_id
        {
            return Err(Error::Process("profile.id must match profile_id".into()));
        }
        Ok(document)
    }

    pub fn instance_profile(&self, id: &Id) -> Result<Option<Value>> {
        Ok(self.instance_document(id)?.profile)
    }

    /// Persist the resolved domain snapshot without consulting any source
    /// profile, image manifest, or mutable planner defaults at start time.
    pub fn set_domain_document(&self, id: &Id, domain: Value) -> Result<i64> {
        let current = self.instance_document(id)?;
        self.replace_domain_document(id, current.revision()?, domain, current.auto_remove)
    }

    /// Replace the complete resolved domain snapshot.  This is the only
    /// configuration mutation exposed after the v1 cutover; launch plans and
    /// source profile fragments are intentionally not accepted here.
    pub fn replace_domain_document(
        &self,
        id: &Id,
        revision: i64,
        domain: Value,
        auto_remove: bool,
    ) -> Result<i64> {
        if !domain.is_object() {
            return Err(Error::Process("domain document must be an object".into()));
        }
        let mut document = self.instance_document(id)?;
        if document.revision()? != revision {
            return Err(Error::Process(
                "instance configuration revision changed".into(),
            ));
        }
        let instance = self.instance(id)?;
        // Validate before writing so a malformed replacement cannot leave the
        // workspace in a state that the runtime will refuse to start.
        Self::validate_domain_instance_document(domain.clone(), &instance)?;
        document.domain_document = Some(domain);
        document.auto_remove = auto_remove;
        self.write_instance_document_at(&self.instance_document_path(id)?, &document)?;
        document.revision()
    }

    /// Replace only the pinned engine of a stopped, resolved instance.
    ///
    /// Engine upgrades deliberately operate on the saved domain snapshot.  No
    /// profile, image, identity, or mutable host default is consulted.  The
    /// compatibility fields copied into that snapshot at resolution time are
    /// therefore the authority for deciding whether a new build is safe.
    pub fn upgrade_instance_engine(&self, id: &Id, revision: i64, candidate: Value) -> Result<i64> {
        let instance = self.instance(id)?;
        if !matches!(instance.state.as_str(), "created" | "stopped" | "error")
            || self.active_run(id)?.is_some()
        {
            return Err(Error::Process(
                "stop the instance before upgrading its engine".into(),
            ));
        }
        let mut envelope = self.instance_document(id)?;
        if envelope.revision()? != revision {
            return Err(Error::Process(
                "instance configuration revision changed".into(),
            ));
        }
        let mut domain = self.domain_instance_document(id)?;
        let candidate = candidate
            .as_object()
            .ok_or_else(|| Error::Process("engine upgrade must be an object".into()))?
            .clone();
        let track = candidate
            .get("track")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Process("engine upgrade track is required".into()))?
            .to_owned();
        let digest = candidate
            .get("build_digest")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Process("engine upgrade build_digest is required".into()))?
            .to_owned();
        let digest_hex = digest.strip_prefix("sha256:").unwrap_or(&digest);
        if digest_hex.len() != 64 || !digest_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::Process(
                "engine upgrade build_digest must be a SHA-256 digest".into(),
            ));
        }
        let executable = candidate
            .get("executable")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Process("engine upgrade executable is required".into()))?
            .to_owned();
        let manifest_path = self
            .root
            .join("generated-engines")
            .join(&track)
            .join("engine-build.json");
        let manifest_bytes = fs::read(&manifest_path).map_err(|source| Error::Io {
            path: manifest_path.clone(),
            source,
        })?;
        let manifest: Value = serde_json::from_slice(&manifest_bytes).map_err(|error| {
            Error::Process(format!(
                "invalid engine build manifest {}: {error}",
                manifest_path.display()
            ))
        })?;
        if manifest.get("build_digest").and_then(Value::as_str) != Some(digest.as_str()) {
            return Err(Error::Process(format!(
                "engine build manifest for {track} does not provide requested digest {digest}"
            )));
        }
        let manifest_executable = manifest
            .get("executables")
            .and_then(Value::as_object)
            .and_then(|values| values.values().find_map(Value::as_str));
        if manifest_executable != Some(executable.as_str()) {
            return Err(Error::Process(format!(
                "engine executable is not provided by the {track} build"
            )));
        }

        let spec = domain
            .spec
            .as_object_mut()
            .ok_or_else(|| Error::Process("resolved instance spec must be an object".into()))?;
        let old_engine = spec
            .get("engine")
            .and_then(Value::as_object)
            .ok_or_else(|| Error::Process("resolved instance has no pinned engine".into()))?
            .clone();
        let compatibility = old_engine
            .get("compatibility")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for key in ["image_tracks", "profile_tracks"] {
            if let Some(tracks) = compatibility.get(key).and_then(Value::as_array)
                && !tracks
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|value| value == track)
            {
                return Err(Error::Process(format!(
                    "engine track {track} is outside saved {key} compatibility"
                )));
            }
        }
        if let Some(tracks) = spec
            .get("image")
            .and_then(|value| value.get("compatible_engines"))
            .and_then(Value::as_array)
            && !tracks
                .iter()
                .filter_map(Value::as_str)
                .any(|value| value == track)
        {
            return Err(Error::Process(format!(
                "engine track {track} is incompatible with the saved image"
            )));
        }
        let identity_compatibility = spec
            .get("hardware_identity")
            .and_then(|value| value.get("compatibility"))
            .and_then(Value::as_object);
        if let Some(tracks) = identity_compatibility
            .and_then(|value| value.get("engine_tracks"))
            .and_then(Value::as_array)
            && !tracks
                .iter()
                .filter_map(Value::as_str)
                .any(|value| value == track)
        {
            return Err(Error::Process(format!(
                "engine track {track} is incompatible with the saved hardware identity"
            )));
        }
        if let Some(required) = identity_compatibility
            .and_then(|value| value.get("patch_revision"))
            .and_then(Value::as_str)
            && candidate.get("patch_revision").and_then(Value::as_str) != Some(required)
        {
            return Err(Error::Process(format!(
                "engine patch revision must remain {required} for the saved hardware identity"
            )));
        }
        let machines = candidate
            .get("machines")
            .and_then(Value::as_array)
            .or_else(|| manifest.get("machines").and_then(Value::as_array))
            .or_else(|| manifest.get("targets").and_then(Value::as_array));
        if let Some(machines) = machines {
            let machine = spec
                .get("machine")
                .and_then(|value| value.get("type").or(Some(value)))
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Process("resolved instance has no machine type".into()))?;
            if !machines.iter().filter_map(Value::as_str).any(|supported| {
                supported == machine
                    || machine.contains(supported)
                    || machine.starts_with(supported)
                    || supported.starts_with(machine)
            }) {
                return Err(Error::Process(format!(
                    "engine build does not support machine {machine}"
                )));
            }
        }
        if old_engine.get("track").and_then(Value::as_str) == Some(&track)
            && old_engine.get("build_digest").and_then(Value::as_str) == Some(&digest)
        {
            return Err(Error::Process(
                "engine upgrade must advance the pinned build".into(),
            ));
        }
        let mut updated_engine = candidate;
        updated_engine.insert("track".into(), Value::String(track));
        updated_engine.insert("build_digest".into(), Value::String(digest));
        updated_engine.insert("executable".into(), Value::String(executable));
        updated_engine.insert("compatibility".into(), Value::Object(compatibility));
        spec.insert("engine".into(), Value::Object(updated_engine));
        // A rendered plan is derived state.  Removing it forces the next
        // start/render operation to use the newly pinned build.
        if let Some(spec) = domain.spec.as_object_mut() {
            spec.remove("launch_plan");
            spec.remove("rendered_launch_plan");
        }
        domain.metadata.revision = domain.metadata.revision.saturating_add(1);
        envelope.domain_document = Some(serde_json::to_value(domain)?);
        self.write_instance_document_at(&self.instance_document_path(id)?, &envelope)?;
        self.db.execute(
            "UPDATE instances SET revision = revision + 1 WHERE instance_id = ?1",
            [id.as_str()],
        )?;
        envelope.revision()
    }

    /// One-time export. Once exported, legacy configuration tables are removed.
    pub(super) fn migrate_instance_documents(&self) -> Result<()> {
        let table_exists = |name: &str| -> Result<bool> {
            Ok(self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [name],
                |row| row.get(0),
            )?)
        };
        let has_launch = table_exists("instance_launch")?;
        let has_profiles = table_exists("instance_configuration")?;
        if !has_launch && !has_profiles {
            return Ok(());
        }
        for instance in self.instances()? {
            let id = &instance.instance_id;
            let path = self.instance_document_path(id)?;
            if fs::symlink_metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                let launch: Option<(String, bool)> = if has_launch {
                    self.db.query_row("SELECT plan_json, auto_remove FROM instance_launch WHERE instance_id = ?1", [id.as_str()], |row| Ok((row.get(0)?, row.get(1)?))).optional()?
                } else {
                    None
                };
                let profile: Option<String> = if has_profiles {
                    self.db.query_row("SELECT profile_json FROM instance_configuration WHERE instance_id = ?1", [id.as_str()], |row| row.get(0)).optional()?
                } else {
                    None
                };
                let directory = self.root.join("instances").join(id.as_str());
                fs::create_dir_all(&directory).map_err(|source| Error::Io {
                    path: directory.clone(),
                    source,
                })?;
                let profile = match profile {
                    Some(profile) => Some(serde_json::from_str(&profile)?),
                    None => self.read_staged_profile(&directory)?,
                };
                let document = InstanceDocument {
                    schema_version: 1,
                    instance_id: id.as_str().into(),
                    image_id: instance.image_id.as_str().into(),
                    profile_id: instance.profile_id.as_str().into(),
                    auto_remove: launch.as_ref().is_some_and(|(_, remove)| *remove),
                    profile,
                    launch_plan: launch
                        .map(|(plan, _)| serde_json::from_str(&plan))
                        .transpose()?,
                    domain_document: None,
                };
                self.write_instance_document_at(&path, &document)?;
            }
            self.instance_document(id)?;
        }
        // All files have been synced before removing any legacy configuration.
        self.db.execute_batch("BEGIN; DROP TABLE IF EXISTS instance_configuration; DROP TABLE IF EXISTS instance_launch; UPDATE schema_version SET version = 3; COMMIT;")?;
        Ok(())
    }

    pub(super) fn write_instance_document_at(
        &self,
        path: &Path,
        document: &InstanceDocument,
    ) -> Result<()> {
        let bytes = serde_yaml::to_string(document)
            .map_err(|e| Error::Process(e.to_string()))?
            .into_bytes();
        self.atomic_configuration_file(path, &bytes)
    }

    fn atomic_configuration_file(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let directory = path
            .parent()
            .ok_or_else(|| Error::Process("configuration has no parent".into()))?;
        let root = self.root.canonicalize().map_err(|source| Error::Io {
            path: self.root.clone(),
            source,
        })?;
        let owner = directory.canonicalize().map_err(|source| Error::Io {
            path: directory.into(),
            source,
        })?;
        if !owner.starts_with(root) {
            return Err(Error::Process(
                "configuration path escapes the workspace".into(),
            ));
        }
        let temporary = directory.join(format!(
            ".instance-{}-{}.tmp",
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
            let mut file = options.open(&temporary).map_err(|source| Error::Io {
                path: temporary.clone(),
                source,
            })?;
            file.write_all(bytes)
                .and_then(|_| file.sync_all())
                .map_err(|source| Error::Io {
                    path: temporary.clone(),
                    source,
                })?;
            fs::rename(&temporary, path).map_err(|source| Error::Io {
                path: path.into(),
                source,
            })?;
            fs::File::open(directory)
                .and_then(|file| file.sync_all())
                .map_err(|source| Error::Io {
                    path: directory.into(),
                    source,
                })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    /// Atomically replace configuration. A content revision catches direct file edits.
    pub fn replace_instance_configuration(
        &self,
        id: &Id,
        revision: i64,
        plan_json: &str,
        auto_remove: bool,
        profile: Option<&Value>,
    ) -> Result<i64> {
        let mut document = self.instance_document(id)?;
        if document.revision()? != revision {
            return Err(Error::Process(
                "instance configuration revision changed".into(),
            ));
        }
        let plan: Value = serde_json::from_str(plan_json)?;
        if document
            .launch_plan
            .as_ref()
            .and_then(|p| p.get("preparation"))
            != plan.get("preparation")
        {
            return Err(Error::Process(
                "disk, NVRAM and TPM preparation cannot change".into(),
            ));
        }
        if profile.is_none() && document.profile.is_some() {
            return Err(Error::Process(
                "profile cannot be removed from an existing instance".into(),
            ));
        }
        if let Some(profile_id) = profile.and_then(|p| p.get("id")).and_then(Value::as_str)
            && profile_id != document.profile_id
        {
            return Err(Error::Process("profile.id must match profile_id".into()));
        }
        document.profile = profile.cloned();
        document.auto_remove = auto_remove;
        document.launch_plan = Some(plan);
        self.write_instance_document_at(&self.instance_document_path(id)?, &document)?;
        if let Err(error) = self.materialize_instance_profile(id) {
            eprintln!("profile cache refresh for {}: {error}", id.as_str());
        }
        document.revision()
    }

    pub fn materialize_instance_profile(&self, id: &Id) -> Result<()> {
        self.materialize_document_profile(&self.instance_document(id)?)
    }

    /// Render helpers from the same document snapshot used to launch QEMU.
    pub fn materialize_document_profile(&self, document: &InstanceDocument) -> Result<()> {
        let id = Id::new("instance", document.instance_id.clone())?;
        let path = self
            .root
            .join("instances")
            .join(id.as_str())
            .join("profile.yaml");
        if let Some(profile) = &document.profile {
            let bytes = serde_yaml::to_string(profile)
                .map_err(|error| Error::Process(error.to_string()))?;
            self.atomic_configuration_file(&path, bytes.as_bytes())?;
        } else {
            if fs::symlink_metadata(&path).is_ok() {
                self.check_configuration_path(&path)?;
            }
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::Io { path, source }),
            }
        }
        Ok(())
    }
}
