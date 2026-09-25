use super::{InstanceDocument, Workspace};
use crate::domain::{Id, Instance, InstanceState};
use crate::{Error, Result};
use rusqlite::{OptionalExtension, params};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn empty_legacy_plan_directory(directory: &Path) -> Result<bool> {
    let entries = fs::read_dir(directory).map_err(|source| Error::Io {
        path: directory.to_owned(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: directory.to_owned(),
            source,
        })?;
        let name = entry.file_name();
        if name != "sockets" && name != "control" {
            return Ok(false);
        }
        let file_type = entry.file_type().map_err(|source| Error::Io {
            path: entry.path(),
            source,
        })?;
        if !file_type.is_dir()
            || fs::read_dir(entry.path())
                .map_err(|source| Error::Io {
                    path: entry.path(),
                    source,
                })?
                .next()
                .is_some()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Workspace {
    pub fn save_instance_launch(
        &self,
        instance_id: &Id,
        _plan_json: &str,
        auto_remove: bool,
    ) -> Result<()> {
        let mut document = self.instance_document(instance_id)?;
        if document.domain_document.is_none() {
            return Err(Error::Process(
                "cannot save a launch plan; instance requires a complete domain document".into(),
            ));
        }
        // Launch plans are derived data.  Keep this method only for the
        // migration-era storage API; a runtime caller must never replace the
        // authoritative domain snapshot with client-generated arguments.
        document.auto_remove = auto_remove;
        self.write_instance_document_at(&self.instance_document_path(instance_id)?, &document)
    }

    pub fn instance_launch(&self, instance_id: &Id) -> Result<Option<(String, bool)>> {
        if matches!(self.instance(instance_id), Err(Error::NotFound { .. })) {
            return Ok(None);
        }
        let document = self.instance_document(instance_id)?;
        if document.domain_document.is_some() {
            Ok(Some((String::new(), document.auto_remove)))
        } else {
            Ok(None)
        }
    }

    pub fn record_instance_tombstone(
        &self,
        instance_id: &Id,
        run_id: Option<&Id>,
        reason: &str,
    ) -> Result<()> {
        self.db.execute(
            "INSERT INTO instance_tombstones(instance_id, last_run_id, reason) VALUES (?1, ?2, ?3)
             ON CONFLICT(instance_id) DO UPDATE SET last_run_id = excluded.last_run_id, reason = excluded.reason, removed_at = CURRENT_TIMESTAMP",
            params![instance_id.as_str(), run_id.map(Id::as_str), reason],
        )?;
        self.db.execute("DELETE FROM instance_tombstones WHERE instance_id NOT IN (SELECT instance_id FROM instance_tombstones ORDER BY removed_at DESC, rowid DESC LIMIT 1000)", [])?;
        Ok(())
    }

    pub fn instance_tombstone(
        &self,
        instance_id: &Id,
    ) -> Result<Option<(Option<String>, String, String)>> {
        self.db.query_row(
            "SELECT last_run_id, reason, removed_at FROM instance_tombstones WHERE instance_id = ?1",
            params![instance_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(Error::from)
    }

    pub fn create_instance(
        &self,
        instance_id: Id,
        image_id: Id,
        profile_id: Id,
    ) -> Result<Instance> {
        let staging_root = self.root.join("staging");
        let staged = staging_root.join(format!(
            "create-{}-{}-{}",
            instance_id.as_str(),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir(&staged).map_err(|source| Error::Io {
            path: staged.clone(),
            source,
        })?;
        let result = self.publish_prepared_instance(
            instance_id,
            image_id,
            profile_id,
            &staged,
            "null",
            false,
        );
        if result.is_err() {
            let _ = fs::remove_dir_all(staged);
        }
        result
    }

    /// Materialize the writable files owned by one instance from immutable
    /// workspace assets. Existing files are preserved so retries are safe.
    pub fn prepare_instance_files(
        &self,
        instance_id: &Id,
        disk_backing: &Path,
        backing_format: &str,
        nvram_seed: Option<&Path>,
        tpm_seed: Option<&Path>,
    ) -> Result<PathBuf> {
        self.prepare_instance_files_sized(
            instance_id,
            disk_backing,
            backing_format,
            None,
            nvram_seed,
            tpm_seed,
        )
    }

    pub fn prepare_instance_files_sized(
        &self,
        instance_id: &Id,
        disk_backing: &Path,
        backing_format: &str,
        disk_size: Option<&str>,
        nvram_seed: Option<&Path>,
        tpm_seed: Option<&Path>,
    ) -> Result<PathBuf> {
        let directory = self.root.join("instances").join(instance_id.as_str());
        self.prepare_instance_files_at(
            &directory,
            disk_backing,
            backing_format,
            disk_size,
            nvram_seed,
            tpm_seed,
        )
    }

    /// Prepare writable files in a private staging directory before publishing
    /// a new instance. The paths stored in the launch plan still name the final
    /// instance directory.
    pub fn prepare_instance_files_at(
        &self,
        directory: &Path,
        disk_backing: &Path,
        backing_format: &str,
        disk_size: Option<&str>,
        nvram_seed: Option<&Path>,
        tpm_seed: Option<&Path>,
    ) -> Result<PathBuf> {
        fs::create_dir_all(directory).map_err(|source| Error::Io {
            path: directory.to_owned(),
            source,
        })?;
        let overlay = directory.join("overlay.qcow2");
        if !overlay.exists() {
            let output = Command::new("qemu-img")
                .args([
                    "create",
                    "-f",
                    "qcow2",
                    "-F",
                    backing_format,
                    "-b",
                    &disk_backing.to_string_lossy(),
                    &overlay.to_string_lossy(),
                ])
                .output()
                .map_err(|source| Error::Io {
                    path: PathBuf::from("qemu-img"),
                    source,
                })?;
            if !output.status.success() {
                return Err(Error::Process(format!(
                    "qemu-img create exited with {}; stderr: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        if let Some(size) = disk_size {
            let status = Command::new("qemu-img")
                .args(["resize", &overlay.to_string_lossy(), size])
                .status()
                .map_err(|source| Error::Io {
                    path: PathBuf::from("qemu-img"),
                    source,
                })?;
            if !status.success() {
                return Err(Error::Process(format!(
                    "qemu-img resize exited with {status}; requested disk size {size:?} may be smaller than the existing disk"
                )));
            }
        }
        if let Some(seed) = nvram_seed {
            let destination = directory.join("OVMF_VARS.fd");
            if !destination.exists() {
                fs::copy(seed, &destination).map_err(|source| Error::Io {
                    path: destination.clone(),
                    source,
                })?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&destination, fs::Permissions::from_mode(0o600)).map_err(
                    |source| Error::Io {
                        path: destination.clone(),
                        source,
                    },
                )?;
            }
        }
        let tpm_directory = directory.join("tpm");
        fs::create_dir_all(&tpm_directory).map_err(|source| Error::Io {
            path: tpm_directory.clone(),
            source,
        })?;
        if let Some(seed) = tpm_seed {
            let destination = tpm_directory.join("tpm2-00.permall");
            if !destination.exists() {
                fs::copy(seed, &destination).map_err(|source| Error::Io {
                    path: destination,
                    source,
                })?;
            }
        }
        Ok(overlay)
    }

    pub fn publish_prepared_instance(
        &self,
        instance_id: Id,
        image_id: Id,
        profile_id: Id,
        staged: &Path,
        plan_json: &str,
        auto_remove: bool,
    ) -> Result<Instance> {
        self.finish_instance_deletion(&instance_id)?;
        self.image(&image_id)?;
        let destination = self.root.join("instances").join(instance_id.as_str());
        if destination.exists() {
            // A crash after the rename leaves a marked directory. Older create
            // requests could also leave only empty runtime directories.
            let directory = fs::symlink_metadata(&destination).map_err(|source| Error::Io {
                path: destination.clone(),
                source,
            })?;
            if directory.file_type().is_dir()
                && matches!(self.instance(&instance_id), Err(Error::NotFound { .. }))
                && (destination.join(".machineemu-create").is_file()
                    || empty_legacy_plan_directory(&destination)?)
            {
                fs::remove_dir_all(&destination).map_err(|source| Error::Io {
                    path: destination.clone(),
                    source,
                })?;
            } else {
                return Err(Error::Process(format!(
                    "instance directory already exists: {}",
                    destination.display()
                )));
            }
        }
        let profile = self.read_staged_profile(staged)?;
        let domain_document = {
            let path = staged.join("domain_document.yaml");
            match fs::read(&path) {
                Ok(_) => Some(
                    crate::engine::load_document(&path)
                        .map_err(|error| Error::Process(error.to_string()))?,
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => return Err(Error::Io { path, source }),
            }
        };
        let document = InstanceDocument {
            schema_version: 1,
            instance_id: instance_id.as_str().into(),
            image_id: image_id.as_str().into(),
            profile_id: profile_id.as_str().into(),
            auto_remove,
            profile,
            launch_plan: serde_json::from_str(plan_json)?,
            domain_document,
        };
        self.write_instance_document_at(&staged.join("instance.yaml"), &document)?;
        fs::write(staged.join(".machineemu-create"), b"staged\n").map_err(|source| Error::Io {
            path: staged.join(".machineemu-create"),
            source,
        })?;
        fs::rename(staged, &destination).map_err(|source| Error::Io {
            path: destination.clone(),
            source,
        })?;
        let result = (|| -> Result<()> {
            let transaction = self.db.unchecked_transaction()?;
            transaction.execute(
                "INSERT OR IGNORE INTO images(image_id) VALUES (?1)",
                params![image_id.as_str()],
            )?;
            transaction.execute(
                "INSERT INTO instances(instance_id, image_id, profile_id, lifecycle) VALUES (?1, ?2, ?3, 'created')",
                params![instance_id.as_str(), image_id.as_str(), profile_id.as_str()],
            )?;
            transaction.execute(
                "DELETE FROM instance_tombstones WHERE instance_id = ?1",
                params![instance_id.as_str()],
            )?;
            transaction.commit()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&destination);
        } else {
            let _ = fs::remove_file(destination.join(".machineemu-create"));
        }
        result?;
        self.instance(&instance_id)
    }

    pub fn instance(&self, instance_id: &Id) -> Result<Instance> {
        self.db.query_row(
            "SELECT instance_id, image_id, profile_id, lifecycle, revision FROM instances WHERE instance_id = ?1",
            params![instance_id.as_str()],
            |row| Ok(Instance {
                instance_id: Id::from_stored(row.get(0)?), image_id: Id::from_stored(row.get(1)?), profile_id: Id::from_stored(row.get(2)?),
                state: row.get(3)?, revision: row.get(4)?,
            }),
        ).optional()?.ok_or_else(|| Error::NotFound { kind: "instance", id: instance_id.as_str().to_owned() })
    }

    pub fn instances(&self) -> Result<Vec<Instance>> {
        let mut statement = self.db.prepare(
            "SELECT instance_id, image_id, profile_id, lifecycle, revision
             FROM instances ORDER BY instance_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(Instance {
                instance_id: Id::from_stored(row.get(0)?),
                image_id: Id::from_stored(row.get(1)?),
                profile_id: Id::from_stored(row.get(2)?),
                state: row.get(3)?,
                revision: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::Sqlite)
    }

    pub fn transition_instance(&self, instance_id: &Id, next: &str) -> Result<Instance> {
        let current = self.instance(instance_id)?;
        let next: InstanceState = next.parse()?;
        if !current.state.allows(next) {
            return Err(Error::InvalidTransition {
                from: current.state.to_string(),
                to: next.to_string(),
            });
        }
        self.db.execute(
            "UPDATE instances SET lifecycle = ?1, revision = revision + 1 WHERE instance_id = ?2",
            params![next.as_str(), instance_id.as_str()],
        )?;
        self.instance(instance_id)
    }

    /// Remove an instance and its owned state after it has stopped.
    pub fn remove_instance(&self, instance_id: &Id) -> Result<()> {
        if self.finish_instance_deletion(instance_id)? {
            return Ok(());
        }
        let mut instance = self.instance(instance_id)?;
        if let Some(active) = self.active_run(instance_id)? {
            let reconciled = self.reconcile_run(&active.run_id)?;
            if reconciled.status == "running" {
                return Err(Error::ActiveRun(instance_id.as_str().to_owned()));
            }
            self.finish_run(&active.run_id, "failed")?;
            self.db.execute(
                "UPDATE instances SET lifecycle = 'stopped', revision = revision + 1 WHERE instance_id = ?1",
                params![instance_id.as_str()],
            )?;
            instance = self.instance(instance_id)?;
        }
        if instance.state != "created" && instance.state != "stopped" && instance.state != "error" {
            return Err(Error::Process(format!(
                "instance {:?} must be stopped before removal (state {:?})",
                instance_id.as_str(),
                instance.state
            )));
        }
        let snapshots: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM snapshots WHERE instance_id = ?1",
            params![instance_id.as_str()],
            |row| row.get(0),
        )?;
        if snapshots != 0 {
            return Err(Error::Process(format!(
                "instance {:?} still has {snapshots} snapshot(s)",
                instance_id.as_str()
            )));
        }
        let transaction = self.db.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO pending_instance_deletions(instance_id) VALUES (?1)",
            params![instance_id.as_str()],
        )?;
        for table in ["operations", "runs", "instances"] {
            transaction.execute(
                &format!("DELETE FROM {table} WHERE instance_id = ?1"),
                params![instance_id.as_str()],
            )?;
        }
        transaction.commit()?;
        self.finish_instance_deletion(instance_id)?;
        Ok(())
    }

    /// A committed deletion retains its ID until filesystem cleanup completes.
    /// New instances cannot reuse that directory, and retries/reopen resume cleanup.
    pub(super) fn finish_instance_deletion(&self, instance_id: &Id) -> Result<bool> {
        let pending: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pending_instance_deletions WHERE instance_id = ?1)",
            params![instance_id.as_str()],
            |row| row.get(0),
        )?;
        if !pending {
            return Ok(false);
        }
        let directory = self.root.join("instances").join(instance_id.as_str());
        match fs::remove_dir_all(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Io {
                    path: directory,
                    source,
                });
            }
        }
        fs::File::open(self.root.join("instances"))
            .and_then(|file| file.sync_all())
            .map_err(|source| Error::Io {
                path: self.root.join("instances"),
                source,
            })?;
        self.db.execute(
            "DELETE FROM pending_instance_deletions WHERE instance_id = ?1",
            params![instance_id.as_str()],
        )?;
        Ok(true)
    }

    pub(super) fn reconcile_instance_deletions(&self) -> Result<()> {
        let mut statement = self
            .db
            .prepare("SELECT instance_id FROM pending_instance_deletions")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for id in ids {
            self.finish_instance_deletion(&Id::new("instance", id)?)?;
        }
        Ok(())
    }
}
