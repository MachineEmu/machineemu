//! Workspace ownership and persistent publication boundaries.
use crate::{Error, Result, runtime::process_start_identity};
use rusqlite::Connection;
use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
mod blobs;
mod images;
mod instances;
mod lock;
mod operations;
mod runs;
mod snapshots;
use lock::acquire_workspace_lock;

pub struct Workspace {
    root: PathBuf,
    _lease: Arc<WorkspaceLease>,
    db: Connection,
}

struct WorkspaceLease {
    path: PathBuf,
    _file: File,
}

impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl Workspace {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(|source| Error::Io {
            path: root.clone(),
            source,
        })?;
        if !root.is_dir() {
            return Err(Error::WorkspaceNotDirectory(root));
        }
        let lock_path = root.join("workspace.lock");
        let mut lock = acquire_workspace_lock(&lock_path)?;
        lock.write_all(
            format!(
                "{} {}\n",
                std::process::id(),
                process_start_identity(std::process::id()).unwrap_or(0)
            )
            .as_bytes(),
        )
        .map_err(|source| Error::Io {
            path: lock_path.clone(),
            source,
        })?;
        lock.sync_all().map_err(|source| Error::Io {
            path: lock_path.clone(),
            source,
        })?;
        let db_path = root.join("metadata.sqlite3");
        let db = Connection::open(&db_path).map_err(Error::Sqlite)?;
        let result = Self {
            root,
            _lease: Arc::new(WorkspaceLease {
                path: lock_path,
                _file: lock,
            }),
            db,
        };
        result.migrate()?;
        Ok(result)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Open another connection under this process's existing workspace lease.
    pub fn attach(&self) -> Result<Self> {
        let db = Connection::open(self.root.join("metadata.sqlite3"))?;
        db.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(Self {
            root: self.root.clone(),
            _lease: self._lease.clone(),
            db,
        })
    }
}

impl Workspace {
    fn migrate(&self) -> Result<()> {
        self.db.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version(version)
               SELECT 2 WHERE NOT EXISTS (SELECT 1 FROM schema_version);
             CREATE TABLE IF NOT EXISTS images (
               image_id TEXT PRIMARY KEY
             );
             CREATE TABLE IF NOT EXISTS instances (
               instance_id TEXT PRIMARY KEY,
               image_id TEXT NOT NULL REFERENCES images(image_id),
               profile_id TEXT NOT NULL,
               lifecycle TEXT NOT NULL,
               revision INTEGER NOT NULL DEFAULT 1,
               created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             CREATE TABLE IF NOT EXISTS instance_launch (
               instance_id TEXT PRIMARY KEY REFERENCES instances(instance_id),
               plan_json TEXT NOT NULL,
               auto_remove INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS instance_tombstones (
               instance_id TEXT PRIMARY KEY,
               last_run_id TEXT,
               reason TEXT NOT NULL,
               removed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             CREATE INDEX IF NOT EXISTS instances_image_id ON instances(image_id);
             CREATE TABLE IF NOT EXISTS operations (
               operation_id TEXT PRIMARY KEY,
               instance_id TEXT NOT NULL REFERENCES instances(instance_id),
               kind TEXT NOT NULL,
               idempotency_key TEXT NOT NULL,
               input_sha256 TEXT NOT NULL,
               status TEXT NOT NULL,
               result_json TEXT,
               created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
               UNIQUE(instance_id, idempotency_key)
             );
             CREATE TABLE IF NOT EXISTS runs (
               run_id TEXT PRIMARY KEY,
               instance_id TEXT NOT NULL REFERENCES instances(instance_id),
               pid INTEGER NOT NULL,
               process_start INTEGER NOT NULL,
               qmp_socket TEXT NOT NULL,
               status TEXT NOT NULL,
               created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             CREATE TABLE IF NOT EXISTS snapshots (
               snapshot_id TEXT PRIMARY KEY,
               instance_id TEXT NOT NULL REFERENCES instances(instance_id),
               generation INTEGER NOT NULL,
               manifest_json BLOB NOT NULL,
               manifest_sha256 TEXT NOT NULL UNIQUE,
               created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );",
        )?;
        fs::create_dir_all(self.root.join("images")).map_err(|source| Error::Io {
            path: self.root.join("images"),
            source,
        })?;
        fs::create_dir_all(self.root.join("instances")).map_err(|source| Error::Io {
            path: self.root.join("instances"),
            source,
        })?;
        fs::create_dir_all(self.root.join("blobs/sha256")).map_err(|source| Error::Io {
            path: self.root.join("blobs/sha256"),
            source,
        })?;
        fs::create_dir_all(self.root.join("staging")).map_err(|source| Error::Io {
            path: self.root.join("staging"),
            source,
        })?;
        fs::create_dir_all(self.root.join("snapshots")).map_err(|source| Error::Io {
            path: self.root.join("snapshots"),
            source,
        })?;
        self.migrate_image_manifests()?;
        Ok(())
    }
}
