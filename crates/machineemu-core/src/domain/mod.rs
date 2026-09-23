//! Persisted records and identity validation; no filesystem ownership.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct Id(String);

impl TryFrom<String> for Id {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        Self::new("id", value)
    }
}

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
    pub state: InstanceState,
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

/// Persisted lifecycle state. Serde and SQLite retain the existing lowercase format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceState {
    Created,
    Starting,
    Running,
    Paused,
    Stopping,
    Stopped,
    Error,
}

impl InstanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Error => "error",
        }
    }
    pub fn allows(self, next: Self) -> bool {
        use InstanceState::*;
        matches!(
            (self, next),
            (Created | Stopped | Error, Starting)
                | (Starting, Running | Paused | Error)
                | (Running, Paused | Stopping | Error)
                | (Paused, Running | Stopping | Error)
                | (Stopping, Stopped | Error)
        )
    }
    pub fn from_qmp(status: &str) -> Result<Self> {
        match status {
            "running" => Ok(Self::Running),
            "paused" | "prelaunch" => Ok(Self::Paused),
            _ => Err(Error::Qmp(format!("unsupported QEMU status {status}"))),
        }
    }
}
impl std::fmt::Display for InstanceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
impl std::str::FromStr for InstanceState {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "created" => Ok(Self::Created),
            "starting" => Ok(Self::Starting),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "stopping" => Ok(Self::Stopping),
            "stopped" => Ok(Self::Stopped),
            "error" => Ok(Self::Error),
            _ => Err(Error::Process(format!("unknown instance state {value:?}"))),
        }
    }
}
impl PartialEq<&str> for InstanceState {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
impl rusqlite::types::FromSql for InstanceState {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|error| rusqlite::types::FromSqlError::Other(Box::new(error)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deserialization_preserves_id_validation() {
        for value in ["", "../escape", "Upper", "/absolute"] {
            assert!(serde_json::from_value::<Id>(serde_json::json!(value)).is_err());
        }
        let id = Id::new("instance", "lab-01").unwrap();
        assert_eq!(
            serde_json::from_str::<Id>(&serde_json::to_string(&id).unwrap()).unwrap(),
            id
        );
    }
    #[test]
    fn starting_accepts_every_supported_initial_qmp_state() {
        for status in ["running", "paused", "prelaunch"] {
            assert!(InstanceState::Starting.allows(InstanceState::from_qmp(status).unwrap()));
        }
        assert!(!InstanceState::Error.allows(InstanceState::Running));
    }
}
