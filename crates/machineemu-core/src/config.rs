use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MachineEmuConfig {
    pub server: Option<ServerConfig>,
    pub client: Option<ClientConfig>,
    pub helpers: Option<HelperConfig>,
    #[serde(default)]
    pub engines: std::collections::BTreeMap<String, EngineConfig>,
}

/// Host programs a run needs beside the engine. They are resolved through PATH
/// unless a path is configured here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HelperConfig {
    pub swtpm: Option<PathBuf>,
    pub bluetooth_simulator: Option<PathBuf>,
    pub unifi_hub: Option<PathBuf>,
    pub wifi_simulator: Option<PathBuf>,
    /// The privileged bridge helper, usually a setuid or capability wrapper
    /// outside the QEMU build it serves.
    pub qemu_bridge_helper: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineConfig {
    /// An executable, or a QEMU build directory containing the target executable.
    pub path: PathBuf,
    pub version: Option<String>,
    pub build_digest: Option<String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerConfig {
    pub workspace: Option<PathBuf>,
    pub listen: Option<String>,
    pub unix_socket: Option<PathBuf>,
    pub bearer_token: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientConfig {
    pub endpoint: Option<String>,
    pub unix_socket: Option<PathBuf>,
    pub token: Option<String>,
    pub workspace: Option<PathBuf>,
}

/// Load the first configuration found: an explicit path, `./machineemu.yaml`,
/// or `$XDG_CONFIG_HOME/machineemu/config.yaml` (falling back to `~/.config`).
pub fn load_config(explicit: Option<&Path>) -> Result<(MachineEmuConfig, Option<PathBuf>)> {
    let path = if let Some(path) = explicit {
        Some(path.to_owned())
    } else if Path::new("machineemu.yaml").is_file() {
        Some(PathBuf::from("machineemu.yaml"))
    } else if Path::new("machineemu.yml").is_file() {
        Some(PathBuf::from("machineemu.yml"))
    } else {
        let config_root = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
        config_root
            .map(|root| root.join("machineemu/config.yaml"))
            .filter(|path| path.is_file())
    };
    let Some(path) = path else {
        return Ok((MachineEmuConfig::default(), None));
    };
    let bytes = fs::read(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    let config = serde_yaml::from_slice(&bytes)
        .map_err(|error| Error::Process(format!("invalid config {}: {error}", path.display())))?;
    Ok((config, Some(path)))
}

pub fn resolve_config_path(config_path: Option<&Path>, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    let resolved = config_path
        .and_then(Path::parent)
        .map(|parent| parent.join(&path))
        .unwrap_or(path);
    if resolved.is_absolute() {
        resolved
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(&resolved))
            .unwrap_or(resolved)
    }
}
