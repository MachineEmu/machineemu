//! Persisted records and identity validation; no filesystem ownership.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Id(String);

impl Id {
    /// Rehydrate an ID already stored by this workspace.
    pub(crate) fn from_stored(value: String) -> Self {
        Self(value)
    }

    pub fn new(kind: &'static str, value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 64
            && value.as_bytes()[0].is_ascii_lowercase()
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            });
        if !valid {
            return Err(Error::InvalidId { kind, value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageManifest {
    pub image_id: Id,
    pub engine_track: Id,
    /// Additional compatible tracks; the original engine_track is always supported.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_engine_tracks: Vec<Id>,
    pub target: String,
    pub disk_sha256: String,
    pub firmware_sha256: Option<String>,
    pub tpm_state_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBundleComponent {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBundleManifest {
    pub schema_version: i64,
    pub image_id: Id,
    pub engine_track: Id,
    /// Additional compatible tracks; the original engine_track is always supported.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_engine_tracks: Vec<Id>,
    pub target: String,
    pub components: std::collections::BTreeMap<String, ImageBundleComponent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub instance_id: Id,
    pub image_id: Id,
    pub profile_id: Id,
    pub state: String,
    pub revision: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    pub operation_id: Id,
    pub instance_id: Id,
    pub kind: String,
    pub idempotency_key: String,
    pub status: String,
    pub result_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub run_id: Id,
    pub instance_id: Id,
    pub pid: u32,
    pub process_start: u64,
    pub qmp_socket: PathBuf,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub snapshot_id: Id,
    pub instance_id: Id,
    pub generation: i64,
    pub files: std::collections::BTreeMap<String, String>,
}

pub(crate) fn allowed_transition(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("created", "starting")
            | ("stopped", "starting")
            | ("starting", "running")
            | ("starting", "error")
            | ("running", "paused")
            | ("running", "stopping")
            | ("running", "error")
            | ("paused", "running")
            | ("paused", "stopping")
            | ("paused", "error")
            | ("stopping", "stopped")
            | ("stopping", "error")
            | ("error", "starting")
    )
}
