use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("workspace path {0} is not a directory")]
    WorkspaceNotDirectory(PathBuf),
    #[error("workspace is already owned by another daemon: {0}")]
    WorkspaceLocked(PathBuf),
    #[error("invalid {kind} {value:?}; use 1-64 lowercase letters, digits, '.', '_' or '-'")]
    InvalidId { kind: &'static str, value: String },
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("filesystem error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("manifest is not valid UTF-8 JSON: {0}")]
    ManifestJson(#[from] serde_json::Error),
    #[error("image {0:?} is already registered with different manifest bytes")]
    ImageConflict(String),
    #[error("operation key {key:?} was reused with different inputs")]
    OperationConflict { key: String },
    #[error("invalid lifecycle transition from {from:?} to {to:?}")]
    InvalidTransition { from: String, to: String },
    #[error("record {kind} {id:?} was not found")]
    NotFound { kind: &'static str, id: String },
    #[error("run {0:?} is already recorded")]
    RunConflict(String),
    #[error("instance {0:?} still has an owned or uncertain run")]
    ActiveRun(String),
    #[error("snapshot {0:?} is already registered")]
    SnapshotConflict(String),
    #[error("snapshots require a stopped instance")]
    SnapshotRequiresStopped,
    #[error("snapshot component name must be a plain file name: {0:?}")]
    InvalidSnapshotComponent(String),
    #[error("image bundle path is invalid: {0:?}")]
    InvalidBundlePath(String),
    #[error("image bundle destination already exists: {0}")]
    BundleExists(PathBuf),
    #[error("imported blob digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("{executable:?} was not found on PATH")]
    ExecutableNotFound { executable: String },
    #[error("cannot execute {executable}: {source}")]
    Spawn {
        executable: String,
        source: std::io::Error,
    },
    #[error("QMP error: {0}")]
    Qmp(String),
    #[error("process error: {0}")]
    Process(String),
}

pub type Result<T> = std::result::Result<T, Error>;
