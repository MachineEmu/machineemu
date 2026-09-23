use super::*;
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct LaunchSpec {
    pub(super) argv: Vec<String>,
    pub(super) qmp_socket: PathBuf,
    pub(super) stdout: Option<PathBuf>,
    pub(super) stderr: Option<PathBuf>,
    pub(super) preparation: Option<PreparationSpec>,
    pub(super) helper_argv: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct PreparationSpec {
    pub(super) disk_backing: PathBuf,
    pub(super) backing_format: String,
    pub(super) disk_size: Option<String>,
    pub(super) nvram_seed: Option<PathBuf>,
    pub(super) tpm_seed: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct StartInstance {
    pub(super) operation_id: String,
    pub(super) run_id: String,
    pub(super) idempotency_key: String,
    #[serde(default)]
    pub(super) launch_plan: Option<LaunchSpec>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateSnapshot {
    pub(super) snapshot_id: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct CloneSnapshot {
    pub(super) instance_id: String,
    pub(super) profile_id: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateInstance {
    pub(super) instance_id: String,
    pub(super) image_id: String,
    pub(super) profile_id: String,
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Serialize)]
pub(super) struct ErrorBody {
    pub(super) error: String,
}

#[derive(Debug, Serialize)]
pub(super) struct InstanceStatus {
    pub(super) instance: machineemu_core::domain::Instance,
    pub(super) ip: Option<String>,
}
