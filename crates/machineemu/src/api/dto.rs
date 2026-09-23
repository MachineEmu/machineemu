use super::*;
pub(super) use machineemu_core::launch::{HelperSpec, LaunchSpec, PreparationSpec};
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub(super) struct StartInstance {
    #[serde(default)]
    pub(super) operation_id: Option<String>,
    #[serde(default)]
    pub(super) run_id: Option<String>,
    #[serde(default)]
    pub(super) idempotency_key: Option<String>,
    #[serde(default)]
    pub(super) launch_plan: Option<LaunchSpec>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(super) struct CreateSnapshot {
    pub(super) snapshot_id: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(super) struct CloneSnapshot {
    pub(super) instance_id: String,
    pub(super) profile_id: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(super) struct CreateInstance {
    pub(super) instance_id: String,
    pub(super) image_id: String,
    pub(super) profile_id: String,
    #[serde(default)]
    pub(super) launch_plan: Option<LaunchSpec>,
    #[serde(default)]
    pub(super) auto_remove: bool,
    #[serde(default)]
    pub(super) profile: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(super) struct RegisterImage {
    pub(super) image_id: String,
    pub(super) engine_track: String,
    #[serde(default)]
    pub(super) supported_engine_tracks: Vec<String>,
    pub(super) target: String,
    pub(super) disk_sha256: String,
    pub(super) firmware_sha256: Option<String>,
    pub(super) tpm_state_sha256: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(super) struct ErrorBody {
    pub(super) error: String,
}

#[derive(Debug, Serialize)]
pub(super) struct InstanceStatus {
    pub(super) instance: machineemu_core::domain::Instance,
    pub(super) ip: Option<String>,
    pub(super) configured: bool,
    pub(super) auto_remove: bool,
}
