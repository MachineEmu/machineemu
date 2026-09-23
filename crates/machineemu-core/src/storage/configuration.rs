//! SQLite owns profile and launch configuration; profile.json is a helper cache.
use super::Workspace;
use crate::{
    Error, Result,
    domain::{Id, Instance},
};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use std::{fs, io::Write, path::Path};

impl Workspace {
    pub(super) fn read_legacy_profile(&self, directory: &Path) -> Result<Option<Value>> {
        let path = directory.join("profile.json");
        let canonical = match path.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::Io { path, source }),
        };
        let root = self.root.canonicalize().map_err(|source| Error::Io {
            path: self.root.clone(),
            source,
        })?;
        if !canonical.starts_with(root) {
            return Err(Error::Process("profile path escapes the workspace".into()));
        }
        let bytes = fs::read(&path).map_err(|source| Error::Io { path, source })?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    pub fn instance_profile(&self, id: &Id) -> Result<Option<Value>> {
        self.instance(id)?;
        let profile: Option<String> = self
            .db
            .query_row(
                "SELECT profile_json FROM instance_configuration WHERE instance_id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        profile
            .map(|value| serde_json::from_str(&value).map_err(Error::from))
            .transpose()
    }

    pub(super) fn migrate_instance_profiles(&self) -> Result<()> {
        for instance in self.instances()? {
            let id = &instance.instance_id;
            if self.instance_profile(id)?.is_none()
                && let Some(profile) =
                    self.read_legacy_profile(&self.root.join("instances").join(id.as_str()))?
            {
                self.db.execute("INSERT OR IGNORE INTO instance_configuration(instance_id, profile_json) VALUES (?1, ?2)", params![id.as_str(), serde_json::to_string(&profile)?])?;
            }
            self.materialize_instance_profile(id)?;
        }
        Ok(())
    }

    /// Commit the profile, plan, and revision together, using optimistic concurrency.
    pub fn replace_instance_configuration(
        &self,
        id: &Id,
        revision: i64,
        plan_json: &str,
        auto_remove: bool,
        profile: Option<&Value>,
    ) -> Result<Instance> {
        let transaction = self.db.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE instances SET revision = revision + 1 WHERE instance_id = ?1 AND revision = ?2",
            params![id.as_str(), revision],
        )?;
        if changed != 1 {
            return Err(Error::Process(
                "instance configuration revision changed".into(),
            ));
        }
        if transaction.execute(
            "UPDATE instance_launch SET plan_json = ?1, auto_remove = ?2 WHERE instance_id = ?3",
            params![plan_json, auto_remove, id.as_str()],
        )? != 1
        {
            return Err(Error::NotFound {
                kind: "instance launch plan",
                id: id.as_str().into(),
            });
        }
        if let Some(profile) = profile {
            transaction.execute("INSERT INTO instance_configuration(instance_id, profile_json) VALUES (?1, ?2) ON CONFLICT(instance_id) DO UPDATE SET profile_json = excluded.profile_json", params![id.as_str(), serde_json::to_string(profile)?])?;
        } else if self.instance_profile(id)?.is_some() {
            return Err(Error::Process(
                "profile cannot be removed from an existing instance".into(),
            ));
        }
        transaction.commit()?;
        // Failure to refresh a derived cache must not turn a committed update into
        // an apparent failure. Startup retries it before any helper reads the file.
        if let Err(error) = self.materialize_instance_profile(id) {
            eprintln!("profile cache refresh for {}: {error}", id.as_str());
        }
        self.instance(id)
    }

    pub fn materialize_instance_profile(&self, id: &Id) -> Result<()> {
        let Some(profile) = self.instance_profile(id)? else {
            return Ok(());
        };
        let directory = self.root.join("instances").join(id.as_str());
        let path = directory.join("profile.json");
        let temporary = directory.join(".profile.json.tmp");
        let canonical = directory.canonicalize().map_err(|source| Error::Io {
            path: directory.clone(),
            source,
        })?;
        let root = self.root.canonicalize().map_err(|source| Error::Io {
            path: self.root.clone(),
            source,
        })?;
        if !canonical.starts_with(root) {
            return Err(Error::Process("profile path escapes the workspace".into()));
        }
        let write = || -> Result<()> {
            // Remove an interrupted cache write before create_new; never follow a link.
            match fs::remove_file(&temporary) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(Error::Io {
                        path: temporary.clone(),
                        source,
                    });
                }
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|source| Error::Io {
                    path: temporary.clone(),
                    source,
                })?;
            file.write_all(&serde_json::to_vec_pretty(&profile)?)
                .and_then(|_| file.sync_all())
                .map_err(|source| Error::Io {
                    path: temporary.clone(),
                    source,
                })?;
            fs::rename(&temporary, &path).map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            fs::File::open(&directory)
                .and_then(|file| file.sync_all())
                .map_err(|source| Error::Io {
                    path: directory.clone(),
                    source,
                })?;
            Ok(())
        };
        write()
    }
}
