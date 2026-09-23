use super::Workspace;
use super::blobs::hex_digest;
use crate::domain::{Id, Instance, Snapshot};
use crate::{Error, Result};
use rusqlite::{OptionalExtension, params};
use std::{
    fs,
    path::{Path, PathBuf},
};

impl Workspace {
    pub fn create_snapshot(
        &self,
        snapshot_id: Id,
        instance_id: Id,
        components: &[(String, PathBuf)],
    ) -> Result<Snapshot> {
        let instance = self.instance(&instance_id)?;
        if instance.state != "created" && instance.state != "stopped" {
            return Err(Error::SnapshotRequiresStopped);
        }
        let destination = self.root.join("snapshots").join(snapshot_id.as_str());
        if destination.exists() {
            return Err(Error::SnapshotConflict(snapshot_id.as_str().into()));
        }
        let staging = self.root.join("staging").join(format!(
            "snapshot-{}-{}",
            snapshot_id.as_str(),
            std::process::id()
        ));
        fs::create_dir_all(&staging).map_err(|source| Error::Io {
            path: staging.clone(),
            source,
        })?;
        let mut files = std::collections::BTreeMap::new();
        let result = (|| -> Result<()> {
            for (name, source) in components {
                let component = Path::new(name);
                if component.file_name().and_then(|value| value.to_str()) != Some(name.as_str()) {
                    return Err(Error::InvalidSnapshotComponent(name.clone()));
                }
                let bytes = fs::read(source).map_err(|source_error| Error::Io {
                    path: source.clone(),
                    source: source_error,
                })?;
                let digest = hex_digest(&bytes);
                fs::write(staging.join(name), &bytes).map_err(|source_error| Error::Io {
                    path: staging.join(name),
                    source: source_error,
                })?;
                files.insert(name.clone(), digest);
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        let manifest = Snapshot {
            snapshot_id: snapshot_id.clone(),
            instance_id: instance_id.clone(),
            generation: instance.revision,
            files,
        };
        let manifest_json = serde_json::to_vec(&manifest)?;
        let manifest_sha256 = hex_digest(&manifest_json);
        fs::write(staging.join("manifest.json"), &manifest_json).map_err(|source| Error::Io {
            path: staging.join("manifest.json"),
            source,
        })?;
        fs::rename(&staging, &destination).map_err(|source| Error::Io {
            path: destination.clone(),
            source,
        })?;
        self.db
            .execute(
                "INSERT INTO snapshots(snapshot_id, instance_id, generation, manifest_json, manifest_sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    snapshot_id.as_str(),
                    instance_id.as_str(),
                    manifest.generation,
                    manifest_json,
                    manifest_sha256
                ],
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(_, _) => {
                    Error::SnapshotConflict(snapshot_id.as_str().into())
                }
                other => Error::Sqlite(other),
            })?;
        Ok(manifest)
    }

    pub fn create_instance_snapshot(&self, snapshot_id: Id, instance_id: Id) -> Result<Snapshot> {
        let instance_dir = self.root.join("instances").join(instance_id.as_str());
        let mut components = Vec::new();
        for entry in fs::read_dir(&instance_dir).map_err(|source| Error::Io {
            path: instance_dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| Error::Io {
                path: instance_dir.clone(),
                source,
            })?;
            if entry
                .file_type()
                .map_err(|source| Error::Io {
                    path: entry.path(),
                    source,
                })?
                .is_file()
            {
                components.push((
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.path(),
                ));
            }
        }
        self.create_snapshot(snapshot_id, instance_id, &components)
    }

    pub fn snapshot(&self, snapshot_id: &Id) -> Result<Snapshot> {
        let bytes: Vec<u8> = self
            .db
            .query_row(
                "SELECT manifest_json FROM snapshots WHERE snapshot_id = ?1",
                params![snapshot_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound {
                kind: "snapshot",
                id: snapshot_id.as_str().into(),
            })?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn restore_snapshot(&self, snapshot_id: &Id, destination: &Path) -> Result<Snapshot> {
        let snapshot = self.snapshot(snapshot_id)?;
        let destination_preexists = destination.exists();
        if destination_preexists
            && fs::read_dir(destination)
                .map_err(|source| Error::Io {
                    path: destination.to_owned(),
                    source,
                })?
                .next()
                .is_some()
        {
            return Err(Error::SnapshotConflict(destination.display().to_string()));
        }
        let restore_destination = if destination_preexists {
            destination.with_extension("clone-staging")
        } else {
            destination.to_owned()
        };
        let source = self.root.join("snapshots").join(snapshot_id.as_str());
        let staging = restore_destination.with_extension("restore-staging");
        fs::create_dir_all(&staging).map_err(|source_error| Error::Io {
            path: staging.clone(),
            source: source_error,
        })?;
        for name in snapshot.files.keys() {
            let bytes = fs::read(source.join(name)).map_err(|source_error| Error::Io {
                path: source.join(name),
                source: source_error,
            })?;
            if hex_digest(&bytes) != snapshot.files[name] {
                let _ = fs::remove_dir_all(&staging);
                return Err(Error::DigestMismatch {
                    expected: snapshot.files[name].clone(),
                    actual: hex_digest(&bytes),
                });
            }
            fs::write(staging.join(name), bytes).map_err(|source_error| Error::Io {
                path: staging.join(name),
                source: source_error,
            })?;
        }
        fs::rename(&staging, &restore_destination).map_err(|source_error| Error::Io {
            path: restore_destination.clone(),
            source: source_error,
        })?;
        if destination_preexists {
            for entry in fs::read_dir(&restore_destination).map_err(|source| Error::Io {
                path: restore_destination.clone(),
                source,
            })? {
                let entry = entry.map_err(|source| Error::Io {
                    path: restore_destination.clone(),
                    source,
                })?;
                fs::rename(entry.path(), destination.join(entry.file_name())).map_err(
                    |source| Error::Io {
                        path: destination.join(entry.file_name()),
                        source,
                    },
                )?;
            }
            fs::remove_dir(&restore_destination).map_err(|source| Error::Io {
                path: restore_destination,
                source,
            })?;
        }
        Ok(snapshot)
    }

    pub fn clone_snapshot(
        &self,
        snapshot_id: &Id,
        instance_id: Id,
        profile_id: Id,
        destination: &Path,
    ) -> Result<Instance> {
        let snapshot = self.snapshot(snapshot_id)?;
        let source_instance = self.instance(&snapshot.instance_id)?;
        let instance = self.create_instance(instance_id, source_instance.image_id, profile_id)?;
        if let Err(error) = self.restore_snapshot(snapshot_id, destination) {
            let _ = self.db.execute(
                "DELETE FROM instances WHERE instance_id = ?1",
                params![instance.instance_id.as_str()],
            );
            let _ = fs::remove_dir_all(
                self.root
                    .join("instances")
                    .join(instance.instance_id.as_str()),
            );
            return Err(error);
        }
        Ok(instance)
    }
}
