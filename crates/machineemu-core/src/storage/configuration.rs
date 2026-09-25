//! Instance files own configuration; SQLite only owns runtime bookkeeping.
use super::Workspace;
use crate::{Error, Result, domain::Id};
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
    pub(super) fn read_legacy_profile(&self, directory: &Path) -> Result<Option<Value>> {
        let path = directory.join("profile.json");
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::Io { path, source }),
        }
        self.check_configuration_path(&path)?;
        let bytes = fs::read(&path).map_err(|source| Error::Io { path, source })?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    pub fn instance_document_path(&self, id: &Id) -> Result<PathBuf> {
        let directory = self.root.join("instances").join(id.as_str());
        let mut selected = None;
        for name in ["instance.json", "instance.yaml", "instance.yml"] {
            let path = directory.join(name);
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    if selected.is_some() {
                        return Err(Error::Process(format!(
                            "multiple instance documents in {}; keep exactly one JSON or YAML file",
                            directory.display()
                        )));
                    }
                    selected = Some(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::Io { path, source }),
            }
        }
        Ok(selected.unwrap_or_else(|| directory.join("instance.json")))
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
        if document.auto_remove && document.launch_plan.is_none() {
            return Err(Error::Process("auto_remove requires a launch plan".into()));
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
                    None => self.read_legacy_profile(&directory)?,
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
        let bytes = if matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("yaml" | "yml")
        ) {
            serde_yaml::to_string(document)
                .map_err(|e| Error::Process(e.to_string()))?
                .into_bytes()
        } else {
            let mut bytes = serde_json::to_vec_pretty(document)?;
            bytes.push(b'\n');
            bytes
        };
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
            .join("profile.json");
        if let Some(profile) = &document.profile {
            self.atomic_configuration_file(&path, &serde_json::to_vec_pretty(profile)?)?;
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
