use super::Workspace;
use crate::domain::{Id, Instance, allowed_transition};
use crate::{Error, Result};
use rusqlite::{OptionalExtension, params};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

impl Workspace {
    pub fn create_instance(
        &self,
        instance_id: Id,
        image_id: Id,
        profile_id: Id,
    ) -> Result<Instance> {
        self.image(&image_id)?;
        // A hand-written manifest is sufficient to create an instance. SQLite
        // only indexes its ID for runtime foreign-key integrity.
        self.db.execute(
            "INSERT OR IGNORE INTO images(image_id) VALUES (?1)",
            params![image_id.as_str()],
        )?;
        self.db.execute(
            "INSERT INTO instances(instance_id, image_id, profile_id, lifecycle) VALUES (?1, ?2, ?3, 'created')",
            params![instance_id.as_str(), image_id.as_str(), profile_id.as_str()],
        )?;
        fs::create_dir_all(self.root.join("instances").join(instance_id.as_str())).map_err(
            |source| Error::Io {
                path: self.root.join("instances").join(instance_id.as_str()),
                source,
            },
        )?;
        self.instance(&instance_id)
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
        fs::create_dir_all(&directory).map_err(|source| Error::Io {
            path: directory.clone(),
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
        if !allowed_transition(&current.state, next) {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: next.to_owned(),
            });
        }
        self.db.execute(
            "UPDATE instances SET lifecycle = ?1, revision = revision + 1 WHERE instance_id = ?2",
            params![next, instance_id.as_str()],
        )?;
        self.instance(instance_id)
    }

    /// Remove an instance and its owned state after it has stopped.
    pub fn remove_instance(&self, instance_id: &Id) -> Result<()> {
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
        self.db.execute(
            "DELETE FROM operations WHERE instance_id = ?1",
            params![instance_id.as_str()],
        )?;
        self.db.execute(
            "DELETE FROM runs WHERE instance_id = ?1",
            params![instance_id.as_str()],
        )?;
        self.db.execute(
            "DELETE FROM instances WHERE instance_id = ?1",
            params![instance_id.as_str()],
        )?;
        let directory = self.root.join("instances").join(instance_id.as_str());
        if directory.exists() {
            fs::remove_dir_all(&directory).map_err(|source| Error::Io {
                path: directory,
                source,
            })?;
        }
        Ok(())
    }
}
