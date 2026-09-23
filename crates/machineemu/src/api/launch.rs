use super::*;
fn workspace_path(root: &std::path::Path, path: &std::path::Path) -> Result<PathBuf, RuntimeError> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(RuntimeError::Process(format!(
            "runtime path is outside workspace: {}",
            path.display()
        )));
    }
    Ok(root.join(path))
}

pub(super) fn plan_paths(
    root: &std::path::Path,
    plan: &LaunchSpec,
) -> Result<(PathBuf, Option<PathBuf>, Option<PathBuf>), RuntimeError> {
    plan_paths_inner(root, plan, true)
}

pub(super) fn validate_plan_paths(
    root: &std::path::Path,
    plan: &LaunchSpec,
) -> Result<(PathBuf, Option<PathBuf>, Option<PathBuf>), RuntimeError> {
    plan_paths_inner(root, plan, false)
}

fn plan_paths_inner(
    root: &std::path::Path,
    plan: &LaunchSpec,
    create_dirs: bool,
) -> Result<(PathBuf, Option<PathBuf>, Option<PathBuf>), RuntimeError> {
    if plan.argv.is_empty() {
        return Err(RuntimeError::Process("launch plan argv is empty".into()));
    }
    let qmp = workspace_path(root, &plan.qmp_socket)?;
    let stdout = plan
        .stdout
        .as_deref()
        .map(|path| workspace_path(root, path))
        .transpose()?;
    let stderr = plan
        .stderr
        .as_deref()
        .map(|path| workspace_path(root, path))
        .transpose()?;
    if create_dirs {
        fs::create_dir_all(qmp.parent().unwrap_or(root)).map_err(|source| RuntimeError::Io {
            path: qmp.parent().unwrap_or(root).to_owned(),
            source,
        })?;
    }
    for path in [stdout.as_deref(), stderr.as_deref()].into_iter().flatten() {
        if create_dirs && let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| RuntimeError::Io {
                path: parent.to_owned(),
                source,
            })?;
        }
    }
    for argument in plan.argv.iter().skip(1) {
        let candidate = argument
            .strip_prefix("file:")
            .or_else(|| argument.strip_prefix("unix:"))
            .or_else(|| argument.split_once("path=").map(|(_, value)| value))
            .map(|value| value.split(',').next().unwrap_or(value));
        if let Some(candidate) = candidate {
            let path = PathBuf::from(candidate);
            if create_dirs
                && path.is_absolute()
                && let Some(parent) = path.parent()
            {
                fs::create_dir_all(parent).map_err(|source| RuntimeError::Io {
                    path: parent.to_owned(),
                    source,
                })?;
            }
        }
    }
    for pair in plan.argv.windows(2) {
        if pair[0] == "-pidfile" {
            let path = PathBuf::from(&pair[1]);
            if create_dirs && let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|source| RuntimeError::Io {
                    path: parent.to_owned(),
                    source,
                })?;
            }
        }
    }
    Ok((qmp, stdout, stderr))
}

pub(super) fn prepare_paths(
    root: &std::path::Path,
    preparation: &PreparationSpec,
) -> Result<(PathBuf, Option<PathBuf>, Option<PathBuf>), RuntimeError> {
    let backing = workspace_path(root, &preparation.disk_backing)?;
    if !backing.is_file() {
        return Err(RuntimeError::Process(format!(
            "disk backing does not exist: {}",
            backing.display()
        )));
    }
    let nvram = preparation
        .nvram_seed
        .as_deref()
        .map(|path| workspace_path(root, path))
        .transpose()?;
    let tpm = preparation
        .tpm_seed
        .as_deref()
        .map(|path| workspace_path(root, path))
        .transpose()?;
    for (kind, path) in [("NVRAM", nvram.as_ref()), ("TPM", tpm.as_ref())] {
        if let Some(path) = path
            && !path.is_file()
        {
            return Err(RuntimeError::Process(format!(
                "{kind} seed does not exist: {}",
                path.display()
            )));
        }
    }
    Ok((backing, nvram, tpm))
}
