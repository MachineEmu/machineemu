//! Shared, serializable launch contract used by the CLI and daemon.
use crate::engine::{Error, LaunchPlan};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct LaunchSpec {
    pub argv: Vec<String>,
    #[serde(default)]
    pub vnc_auto: bool,
    #[cfg_attr(feature = "api-schema", schema(value_type = String))]
    pub qmp_socket: PathBuf,
    #[cfg_attr(feature = "api-schema", schema(value_type = Option<String>))]
    pub stdout: Option<PathBuf>,
    #[cfg_attr(feature = "api-schema", schema(value_type = Option<String>))]
    pub stderr: Option<PathBuf>,
    pub preparation: Option<PreparationSpec>,
    pub helper_argv: Option<Vec<String>>,
    #[serde(default)]
    pub helpers: Vec<HelperSpec>,
}

#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct HelperSpec {
    pub name: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub after_qemu: bool,
    #[cfg_attr(feature = "api-schema", schema(value_type = Option<String>))]
    pub ready_socket: Option<PathBuf>,
}

#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct PreparationSpec {
    #[cfg_attr(feature = "api-schema", schema(value_type = String))]
    pub disk_backing: PathBuf,
    pub backing_format: String,
    pub disk_size: Option<String>,
    #[cfg_attr(feature = "api-schema", schema(value_type = Option<String>))]
    pub nvram_seed: Option<PathBuf>,
    #[cfg_attr(feature = "api-schema", schema(value_type = Option<String>))]
    pub tpm_seed: Option<PathBuf>,
}

/// Runtime-specific paths and helpers added to a planner result.
pub struct LaunchContext<'a> {
    pub workspace: &'a Path,
    pub qmp_socket: &'a Path,
    pub stdout: Option<&'a Path>,
    pub stderr: Option<&'a Path>,
    pub tpm_seed: Option<&'a Path>,
    pub vnc_auto: bool,
    pub helpers: Vec<HelperSpec>,
}

impl LaunchSpec {
    pub fn from_plan(plan: LaunchPlan, context: LaunchContext<'_>) -> Result<Self, Error> {
        let relative = |path: &Path| -> Result<PathBuf, Error> {
            path.strip_prefix(context.workspace)
                .map(Path::to_owned)
                .map_err(|_| {
                    Error::Invalid(format!(
                        "runtime path is outside workspace: {}",
                        path.display()
                    ))
                })
        };
        let preparation = plan
            .preparation
            .disk_overlay
            .as_ref()
            .map(|disk| -> Result<PreparationSpec, Error> {
                Ok(PreparationSpec {
                    disk_backing: relative(&disk.backing)?,
                    backing_format: disk.backing_format.clone(),
                    disk_size: disk.size.clone(),
                    nvram_seed: plan
                        .preparation
                        .nvram
                        .as_ref()
                        .map(|file| relative(&file.seed))
                        .transpose()?,
                    tpm_seed: context.tpm_seed.map(relative).transpose()?,
                })
            })
            .transpose()?;
        Ok(Self {
            argv: plan.argv,
            vnc_auto: context.vnc_auto,
            qmp_socket: relative(context.qmp_socket)?,
            stdout: context.stdout.map(relative).transpose()?,
            stderr: context.stderr.map(relative).transpose()?,
            preparation,
            helper_argv: plan.helper_argv,
            helpers: context.helpers,
        })
    }
}
