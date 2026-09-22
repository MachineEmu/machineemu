use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::Instant;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::time::Duration;
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
    pub launch_plans: Option<PathBuf>,
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

#[cfg(unix)]
pub fn guest_ipv4(socket: &Path) -> Result<Option<String>> {
    use std::io::BufRead;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let stream = UnixStream::connect(socket).map_err(|source| Error::Io {
        path: socket.to_owned(),
        source,
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|source| Error::Io {
            path: socket.to_owned(),
            source,
        })?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|source| Error::Io {
            path: socket.to_owned(),
            source,
        })?;
    let mut writer = stream.try_clone().map_err(|source| Error::Io {
        path: socket.to_owned(),
        source,
    })?;
    writer
        .write_all(b"{\"execute\":\"guest-sync\",\"arguments\":{\"id\":1}}\n{\"execute\":\"guest-network-get-interfaces\"}\n")
        .map_err(|source| Error::Io {
            path: socket.to_owned(),
            source,
        })?;
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    for _ in 0..4 {
        line.clear();
        if reader.read_line(&mut line).map_err(|source| Error::Io {
            path: socket.to_owned(),
            source,
        })? == 0
        {
            break;
        }
        let value: serde_json::Value = match serde_json::from_str(line.trim()) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let Some(interfaces) = value.get("return").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for interface in interfaces {
            let Some(addresses) = interface
                .get("ip-addresses")
                .and_then(serde_json::Value::as_array)
            else {
                continue;
            };
            for address in addresses {
                if address
                    .get("ip-address-type")
                    .and_then(serde_json::Value::as_str)
                    == Some("ipv4")
                {
                    let Some(ip) = address
                        .get("ip-address")
                        .and_then(serde_json::Value::as_str)
                    else {
                        continue;
                    };
                    if !ip.starts_with("127.") && !ip.starts_with("169.254.") {
                        return Ok(Some(ip.to_owned()));
                    }
                }
            }
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExit {
    pub code: Option<i32>,
    pub success: bool,
}

pub struct ManagedProcess {
    child: Child,
    pub pid: u32,
    pub run_id: Id,
}

#[cfg(unix)]
pub struct RunningInstance {
    pub process: ManagedProcess,
    pub qmp: QmpClient,
    pub run_id: Id,
}

#[cfg(unix)]
impl RunningInstance {
    pub fn pause(&mut self) -> Result<()> {
        self.qmp.execute("stop", serde_json::Value::Null)?;
        Ok(())
    }

    pub fn resume(&mut self) -> Result<()> {
        self.qmp.execute("cont", serde_json::Value::Null)?;
        Ok(())
    }

    pub fn reset(&mut self) -> Result<()> {
        self.qmp.execute("system_reset", serde_json::Value::Null)?;
        Ok(())
    }
}

impl ManagedProcess {
    pub fn spawn(
        run_id: Id,
        argv: &[String],
        stdout: Option<&Path>,
        stderr: Option<&Path>,
    ) -> Result<Self> {
        let executable = argv
            .first()
            .ok_or_else(|| Error::Process("empty process argv".into()))?;
        let mut command = Command::new(executable);
        command.args(argv.iter().skip(1));
        if let Some(path) = stdout {
            let file = File::create(path).map_err(|source| Error::Io {
                path: path.to_owned(),
                source,
            })?;
            command.stdout(Stdio::from(file));
        } else {
            command.stdout(Stdio::null());
        }
        if let Some(path) = stderr {
            let file = File::create(path).map_err(|source| Error::Io {
                path: path.to_owned(),
                source,
            })?;
            command.stderr(Stdio::from(file));
        } else {
            command.stderr(Stdio::null());
        }
        // A failed spawn is not a filesystem error about a path: the usual cause
        // is a helper that is not installed, and reporting it as one sent the
        // reader looking for a missing directory instead of a missing program.
        let child = command.spawn().map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound
                && !executable.contains(std::path::MAIN_SEPARATOR)
            {
                Error::ExecutableNotFound {
                    executable: executable.clone(),
                }
            } else {
                Error::Spawn {
                    executable: executable.clone(),
                    source,
                }
            }
        })?;
        let pid = child.id();
        Ok(Self { child, pid, run_id })
    }

    pub fn try_wait(&mut self) -> Result<Option<ProcessExit>> {
        let Some(status) = self
            .child
            .try_wait()
            .map_err(|source| Error::Process(source.to_string()))?
        else {
            return Ok(None);
        };
        Ok(Some(ProcessExit {
            code: status.code(),
            success: status.success(),
        }))
    }

    pub fn wait(&mut self) -> Result<ProcessExit> {
        let status = self
            .child
            .wait()
            .map_err(|source| Error::Process(source.to_string()))?;
        Ok(ProcessExit {
            code: status.code(),
            success: status.success(),
        })
    }

    pub fn terminate(&mut self) -> Result<()> {
        self.child
            .kill()
            .map_err(|source| Error::Process(source.to_string()))
    }

    pub fn process_start(&self) -> Result<u64> {
        process_start_identity(self.pid).ok_or_else(|| {
            Error::Process(format!("cannot read process identity for pid {}", self.pid))
        })
    }
}

#[cfg(unix)]
pub struct QmpClient {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

#[cfg(unix)]
impl QmpClient {
    pub fn connect(path: impl AsRef<Path>, timeout: Duration) -> Result<Self> {
        let path = path.as_ref();
        let stream = UnixStream::connect(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|source| Error::Io {
                path: path.to_owned(),
                source,
            })?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|source| Error::Io {
                path: path.to_owned(),
                source,
            })?;
        let reader_stream = stream.try_clone().map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        let mut client = Self {
            writer: stream,
            reader: BufReader::new(reader_stream),
            next_id: 1,
        };
        let greeting = client.read_message()?;
        if greeting.get("QMP").is_none() {
            return Err(Error::Qmp("QMP greeting is missing the QMP field".into()));
        }
        client.execute("qmp_capabilities", serde_json::Value::Null)?;
        Ok(client)
    }

    pub fn execute(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        if command.is_empty() || command.contains(char::is_whitespace) {
            return Err(Error::Qmp("QMP command must be a non-empty token".into()));
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut request = serde_json::Map::new();
        request.insert("execute".into(), serde_json::Value::String(command.into()));
        request.insert("id".into(), serde_json::Value::from(id));
        if !arguments.is_null() {
            request.insert("arguments".into(), arguments);
        }
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.extend_from_slice(b"\r\n");
        self.writer.write_all(&bytes).map_err(|source| Error::Io {
            path: PathBuf::from("QMP socket"),
            source,
        })?;
        self.writer.flush().map_err(|source| Error::Io {
            path: PathBuf::from("QMP socket"),
            source,
        })?;
        loop {
            let message = self.read_message()?;
            if message.get("event").is_some() {
                continue;
            }
            if message.get("id") != Some(&serde_json::Value::from(id)) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(Error::Qmp(error.to_string()));
            }
            return Ok(message
                .get("return")
                .cloned()
                .unwrap_or(serde_json::Value::Null));
        }
    }

    fn read_message(&mut self) -> Result<serde_json::Value> {
        let mut line = String::new();
        let count = self
            .reader
            .read_line(&mut line)
            .map_err(|source| Error::Io {
                path: PathBuf::from("QMP socket"),
                source,
            })?;
        if count == 0 {
            return Err(Error::Qmp("QMP socket closed".into()));
        }
        serde_json::from_str(line.trim()).map_err(Error::from)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Id(String);

impl Id {
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

pub struct Workspace {
    root: PathBuf,
    _lock: File,
    db: Connection,
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
            _lock: lock,
            db,
        };
        result.migrate()?;
        Ok(result)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn migrate(&self) -> Result<()> {
        self.db.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version(version)
               SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM schema_version);
             CREATE TABLE IF NOT EXISTS images (
               image_id TEXT PRIMARY KEY,
               manifest_json BLOB NOT NULL,
               manifest_sha256 TEXT NOT NULL UNIQUE,
               imported_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             CREATE TABLE IF NOT EXISTS instances (
               instance_id TEXT PRIMARY KEY,
               image_id TEXT NOT NULL REFERENCES images(image_id),
               profile_id TEXT NOT NULL,
               lifecycle TEXT NOT NULL,
               revision INTEGER NOT NULL DEFAULT 1,
               created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
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
        Ok(())
    }

    pub fn import_blob(&self, source: impl AsRef<Path>, expected_sha256: &str) -> Result<PathBuf> {
        let source = source.as_ref();
        let expected_sha256 = expected_sha256
            .strip_prefix("sha256:")
            .unwrap_or(expected_sha256);
        let staging = self
            .root
            .join("staging")
            .join(format!("import-{}", std::process::id()));
        let destination = self.root.join("blobs/sha256").join(expected_sha256);
        let mut input = File::open(source).map_err(|source_error| Error::Io {
            path: source.to_owned(),
            source: source_error,
        })?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .map_err(|source_error| Error::Io {
                path: staging.clone(),
                source: source_error,
            })?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 1024 * 1024];
        loop {
            let count = input.read(&mut buffer).map_err(|source_error| Error::Io {
                path: source.to_owned(),
                source: source_error,
            })?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            output
                .write_all(&buffer[..count])
                .map_err(|source_error| Error::Io {
                    path: staging.clone(),
                    source: source_error,
                })?;
        }
        output.sync_all().map_err(|source_error| Error::Io {
            path: staging.clone(),
            source: source_error,
        })?;
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected_sha256 {
            let _ = fs::remove_file(&staging);
            return Err(Error::DigestMismatch {
                expected: expected_sha256.to_owned(),
                actual,
            });
        }
        if destination.exists() {
            let _ = fs::remove_file(&staging);
            return Ok(destination);
        }
        fs::rename(&staging, &destination).map_err(|source_error| Error::Io {
            path: destination.clone(),
            source: source_error,
        })?;
        Ok(destination)
    }

    pub fn import_file(&self, source: impl AsRef<Path>) -> Result<String> {
        self.import_blob_computed(source.as_ref())
    }

    fn import_blob_computed(&self, source: &Path) -> Result<String> {
        let staging = self
            .root
            .join("staging")
            .join(format!("import-computed-{}", std::process::id()));
        let digest = digest_file(source)?;
        fs::copy(source, &staging).map_err(|source_error| Error::Io {
            path: staging.clone(),
            source: source_error,
        })?;
        let destination = self.root.join("blobs/sha256").join(&digest);
        if destination.exists() {
            let _ = fs::remove_file(&staging);
        } else {
            fs::rename(&staging, &destination).map_err(|source_error| Error::Io {
                path: destination.clone(),
                source: source_error,
            })?;
        }
        Ok(digest)
    }

    /// Imports a vmmanager-sh base directory without treating an instance
    /// overlay as the reusable image. The TPM lock and PID files are omitted;
    /// only the persistent TPM state file is imported.
    pub fn import_vmmanager_base(
        &self,
        source: impl AsRef<Path>,
        image_id: Id,
        engine_track: Id,
        target: impl Into<String>,
    ) -> Result<ImageManifest> {
        let source = source.as_ref();
        let disk = source.join("disk.qcow2");
        let firmware = source.join("OVMF_VARS.fd");
        let tpm = source.join("tpm/tpm2-00.permall");
        if !disk.is_file() {
            return Err(Error::InvalidBundlePath(disk.display().to_string()));
        }
        let import_named = |path: &Path| -> Result<String> { self.import_blob_computed(path) };
        let disk_sha256 = import_named(&disk)?;
        let firmware_sha256 = firmware
            .is_file()
            .then(|| import_named(&firmware))
            .transpose()?;
        let tpm_state_sha256 = tpm.is_file().then(|| import_named(&tpm)).transpose()?;
        let manifest = ImageManifest {
            image_id,
            engine_track,
            target: target.into(),
            disk_sha256,
            firmware_sha256,
            tpm_state_sha256,
        };
        self.register_image(&manifest)?;
        Ok(manifest)
    }

    pub fn export_image_bundle(
        &self,
        image_id: &Id,
        destination: &Path,
    ) -> Result<ImageBundleManifest> {
        if destination.exists() {
            return Err(Error::BundleExists(destination.to_owned()));
        }
        let image = self.image(image_id)?;
        fs::create_dir_all(destination.join("components")).map_err(|source| Error::Io {
            path: destination.join("components"),
            source,
        })?;
        let mut components = std::collections::BTreeMap::new();
        let mut export_component =
            |name: &str, path: &str, digest: Option<&String>| -> Result<()> {
                let Some(digest) = digest else {
                    return Ok(());
                };
                let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
                let source = self.root.join("blobs/sha256").join(digest);
                let target = destination.join(path);
                fs::copy(&source, &target).map_err(|source_error| Error::Io {
                    path: target.clone(),
                    source: source_error,
                })?;
                components.insert(
                    name.to_owned(),
                    ImageBundleComponent {
                        path: path.to_owned(),
                        sha256: format!("sha256:{digest}"),
                    },
                );
                Ok(())
            };
        export_component("disk", "components/disk.qcow2", Some(&image.disk_sha256))?;
        export_component(
            "firmware",
            "components/firmware.fd",
            image.firmware_sha256.as_ref(),
        )?;
        export_component(
            "tpm_state",
            "components/tpm-state",
            image.tpm_state_sha256.as_ref(),
        )?;
        let bundle = ImageBundleManifest {
            schema_version: 1,
            image_id: image.image_id,
            engine_track: image.engine_track,
            target: image.target,
            components,
        };
        let manifest = serde_json::to_vec_pretty(&bundle)?;
        fs::write(destination.join("manifest.json"), manifest).map_err(|source| Error::Io {
            path: destination.join("manifest.json"),
            source,
        })?;
        Ok(bundle)
    }

    pub fn import_image_bundle(&self, source: &Path) -> Result<(ImageManifest, String)> {
        let manifest_path = source.join("manifest.json");
        let manifest: ImageBundleManifest =
            serde_json::from_slice(&fs::read(&manifest_path).map_err(|source_error| {
                Error::Io {
                    path: manifest_path.clone(),
                    source: source_error,
                }
            })?)?;
        if manifest.schema_version != 1 {
            return Err(Error::InvalidBundlePath(
                "unsupported schema_version".into(),
            ));
        }
        let component = |name: &str| -> Result<Option<String>> {
            let Some(component) = manifest.components.get(name) else {
                return Ok(None);
            };
            let path = Path::new(&component.path);
            if path.is_absolute()
                || path
                    .components()
                    .any(|part| part == std::path::Component::ParentDir)
            {
                return Err(Error::InvalidBundlePath(component.path.clone()));
            }
            let file = source.join(path);
            if !file.is_file() {
                return Err(Error::InvalidBundlePath(component.path.clone()));
            }
            let imported = self.import_blob(&file, &component.sha256)?;
            Ok(imported
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()))
        };
        let disk = component("disk")?
            .ok_or_else(|| Error::InvalidBundlePath("disk component is required".into()))?;
        let image = ImageManifest {
            image_id: manifest.image_id,
            engine_track: manifest.engine_track,
            target: manifest.target,
            disk_sha256: disk,
            firmware_sha256: component("firmware")?,
            tpm_state_sha256: component("tpm_state")?,
        };
        let digest = self.register_image(&image)?;
        Ok((image, digest))
    }

    pub fn register_image(&self, manifest: &ImageManifest) -> Result<String> {
        let bytes = serde_json::to_vec(manifest)?;
        let digest = hex_digest(&bytes);
        let existing: Option<(String, Vec<u8>)> = self
            .db
            .query_row(
                "SELECT manifest_sha256, manifest_json FROM images WHERE image_id = ?1",
                params![manifest.image_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((existing_digest, _)) = existing {
            if existing_digest != digest {
                return Err(Error::ImageConflict(manifest.image_id.as_str().to_owned()));
            }
            return Ok(digest);
        }
        self.db.execute(
            "INSERT INTO images(image_id, manifest_json, manifest_sha256) VALUES (?1, ?2, ?3)",
            params![manifest.image_id.as_str(), bytes, digest],
        )?;
        Ok(digest)
    }

    pub fn image(&self, image_id: &Id) -> Result<ImageManifest> {
        let bytes: Vec<u8> = self
            .db
            .query_row(
                "SELECT manifest_json FROM images WHERE image_id = ?1",
                params![image_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound {
                kind: "image",
                id: image_id.as_str().to_owned(),
            })?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn create_instance(
        &self,
        instance_id: Id,
        image_id: Id,
        profile_id: Id,
    ) -> Result<Instance> {
        self.image(&image_id)?;
        self.db.execute(
            "INSERT INTO instances(instance_id, image_id, profile_id, lifecycle) VALUES (?1, ?2, ?3, 'created')",
            params![instance_id.as_str(), image_id.as_str(), profile_id.as_str()],
        )?;
        fs::create_dir_all(self.root.join("instances").join(instance_id.as_str())).map_err(
            |source| Error::Io {
                path: self.root.join("instances").join(instance_id.as_str()),
                source,
            },
        )?;
        self.instance(&instance_id)
    }

    /// Materialize the writable files owned by one instance from immutable
    /// workspace assets. Existing files are preserved so retries are safe.
    pub fn prepare_instance_files(
        &self,
        instance_id: &Id,
        disk_backing: &Path,
        backing_format: &str,
        nvram_seed: Option<&Path>,
        tpm_seed: Option<&Path>,
    ) -> Result<PathBuf> {
        self.prepare_instance_files_sized(
            instance_id,
            disk_backing,
            backing_format,
            None,
            nvram_seed,
            tpm_seed,
        )
    }

    pub fn prepare_instance_files_sized(
        &self,
        instance_id: &Id,
        disk_backing: &Path,
        backing_format: &str,
        disk_size: Option<&str>,
        nvram_seed: Option<&Path>,
        tpm_seed: Option<&Path>,
    ) -> Result<PathBuf> {
        let directory = self.root.join("instances").join(instance_id.as_str());
        fs::create_dir_all(&directory).map_err(|source| Error::Io {
            path: directory.clone(),
            source,
        })?;
        let overlay = directory.join("overlay.qcow2");
        if !overlay.exists() {
            let output = Command::new("qemu-img")
                .args([
                    "create",
                    "-f",
                    "qcow2",
                    "-F",
                    backing_format,
                    "-b",
                    &disk_backing.to_string_lossy(),
                    &overlay.to_string_lossy(),
                ])
                .output()
                .map_err(|source| Error::Io {
                    path: PathBuf::from("qemu-img"),
                    source,
                })?;
            if !output.status.success() {
                return Err(Error::Process(format!(
                    "qemu-img create exited with {}; stderr: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        if let Some(size) = disk_size {
            let status = Command::new("qemu-img")
                .args(["resize", &overlay.to_string_lossy(), size])
                .status()
                .map_err(|source| Error::Io {
                    path: PathBuf::from("qemu-img"),
                    source,
                })?;
            if !status.success() {
                return Err(Error::Process(format!(
                    "qemu-img resize exited with {status}; requested disk size {size:?} may be smaller than the existing disk"
                )));
            }
        }
        if let Some(seed) = nvram_seed {
            let destination = directory.join("OVMF_VARS.fd");
            if !destination.exists() {
                fs::copy(seed, &destination).map_err(|source| Error::Io {
                    path: destination.clone(),
                    source,
                })?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&destination, fs::Permissions::from_mode(0o600)).map_err(
                    |source| Error::Io {
                        path: destination.clone(),
                        source,
                    },
                )?;
            }
        }
        let tpm_directory = directory.join("tpm");
        fs::create_dir_all(&tpm_directory).map_err(|source| Error::Io {
            path: tpm_directory.clone(),
            source,
        })?;
        if let Some(seed) = tpm_seed {
            let destination = tpm_directory.join("tpm2-00.permall");
            if !destination.exists() {
                fs::copy(seed, &destination).map_err(|source| Error::Io {
                    path: destination,
                    source,
                })?;
            }
        }
        Ok(overlay)
    }

    pub fn instance(&self, instance_id: &Id) -> Result<Instance> {
        self.db.query_row(
            "SELECT instance_id, image_id, profile_id, lifecycle, revision FROM instances WHERE instance_id = ?1",
            params![instance_id.as_str()],
            |row| Ok(Instance {
                instance_id: Id(row.get(0)?), image_id: Id(row.get(1)?), profile_id: Id(row.get(2)?),
                state: row.get(3)?, revision: row.get(4)?,
            }),
        ).optional()?.ok_or_else(|| Error::NotFound { kind: "instance", id: instance_id.as_str().to_owned() })
    }

    pub fn instances(&self) -> Result<Vec<Instance>> {
        let mut statement = self.db.prepare(
            "SELECT instance_id, image_id, profile_id, lifecycle, revision
             FROM instances ORDER BY instance_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(Instance {
                instance_id: Id(row.get(0)?),
                image_id: Id(row.get(1)?),
                profile_id: Id(row.get(2)?),
                state: row.get(3)?,
                revision: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::Sqlite)
    }

    pub fn transition_instance(&self, instance_id: &Id, next: &str) -> Result<Instance> {
        let current = self.instance(instance_id)?;
        if !allowed_transition(&current.state, next) {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: next.to_owned(),
            });
        }
        self.db.execute(
            "UPDATE instances SET lifecycle = ?1, revision = revision + 1 WHERE instance_id = ?2",
            params![next, instance_id.as_str()],
        )?;
        self.instance(instance_id)
    }

    /// Remove an instance and its owned state after it has stopped.
    pub fn remove_instance(&self, instance_id: &Id) -> Result<()> {
        let mut instance = self.instance(instance_id)?;
        if let Some(active) = self.active_run(instance_id)? {
            let reconciled = self.reconcile_run(&active.run_id)?;
            if reconciled.status == "running" {
                return Err(Error::ActiveRun(instance_id.as_str().to_owned()));
            }
            self.finish_run(&active.run_id, "failed")?;
            self.db.execute(
                "UPDATE instances SET lifecycle = 'stopped', revision = revision + 1 WHERE instance_id = ?1",
                params![instance_id.as_str()],
            )?;
            instance = self.instance(instance_id)?;
        }
        if instance.state != "created" && instance.state != "stopped" && instance.state != "error" {
            return Err(Error::Process(format!(
                "instance {:?} must be stopped before removal (state {:?})",
                instance_id.as_str(),
                instance.state
            )));
        }
        let snapshots: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM snapshots WHERE instance_id = ?1",
            params![instance_id.as_str()],
            |row| row.get(0),
        )?;
        if snapshots != 0 {
            return Err(Error::Process(format!(
                "instance {:?} still has {snapshots} snapshot(s)",
                instance_id.as_str()
            )));
        }
        self.db.execute(
            "DELETE FROM operations WHERE instance_id = ?1",
            params![instance_id.as_str()],
        )?;
        self.db.execute(
            "DELETE FROM runs WHERE instance_id = ?1",
            params![instance_id.as_str()],
        )?;
        self.db.execute(
            "DELETE FROM instances WHERE instance_id = ?1",
            params![instance_id.as_str()],
        )?;
        let directory = self.root.join("instances").join(instance_id.as_str());
        if directory.exists() {
            fs::remove_dir_all(&directory).map_err(|source| Error::Io {
                path: directory,
                source,
            })?;
        }
        Ok(())
    }

    pub fn begin_operation(
        &self,
        operation_id: Id,
        instance_id: Id,
        kind: &str,
        idempotency_key: &str,
        input_json: &str,
    ) -> Result<Operation> {
        self.instance(&instance_id)?;
        let input_sha256 = hex_digest(input_json.as_bytes());
        let existing: Option<(String, String, String, String, Option<String>)> = self
            .db
            .query_row(
                "SELECT operation_id, kind, input_sha256, status, result_json
             FROM operations WHERE instance_id = ?1 AND idempotency_key = ?2",
                params![instance_id.as_str(), idempotency_key],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        if let Some((existing_id, existing_kind, existing_hash, status, result_json)) = existing {
            if existing_kind != kind || existing_hash != input_sha256 {
                return Err(Error::OperationConflict {
                    key: idempotency_key.to_owned(),
                });
            }
            return Ok(Operation {
                operation_id: Id(existing_id),
                instance_id,
                kind: existing_kind,
                idempotency_key: idempotency_key.to_owned(),
                status,
                result_json,
            });
        }
        self.db.execute(
            "INSERT INTO operations(operation_id, instance_id, kind, idempotency_key, input_sha256, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'accepted')",
            params![operation_id.as_str(), instance_id.as_str(), kind, idempotency_key, input_sha256],
        )?;
        Ok(Operation {
            operation_id,
            instance_id,
            kind: kind.to_owned(),
            idempotency_key: idempotency_key.to_owned(),
            status: "accepted".into(),
            result_json: None,
        })
    }

    pub fn complete_operation(&self, operation_id: &Id, result_json: &str) -> Result<Operation> {
        let changed = self.db.execute(
            "UPDATE operations SET status = 'completed', result_json = ?1 WHERE operation_id = ?2",
            params![result_json, operation_id.as_str()],
        )?;
        if changed == 0 {
            return Err(Error::NotFound {
                kind: "operation",
                id: operation_id.as_str().to_owned(),
            });
        }
        self.operation(operation_id)
    }

    pub fn operation(&self, operation_id: &Id) -> Result<Operation> {
        self.db
            .query_row(
                "SELECT operation_id, instance_id, kind, idempotency_key, status, result_json
             FROM operations WHERE operation_id = ?1",
                params![operation_id.as_str()],
                |row| {
                    Ok(Operation {
                        operation_id: Id(row.get(0)?),
                        instance_id: Id(row.get(1)?),
                        kind: row.get(2)?,
                        idempotency_key: row.get(3)?,
                        status: row.get(4)?,
                        result_json: row.get(5)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| Error::NotFound {
                kind: "operation",
                id: operation_id.as_str().to_owned(),
            })
    }

    pub fn record_run(
        &self,
        run_id: Id,
        instance_id: Id,
        pid: u32,
        process_start: u64,
        qmp_socket: PathBuf,
    ) -> Result<Run> {
        self.instance(&instance_id)?;
        self.db
            .execute(
                "INSERT INTO runs(run_id, instance_id, pid, process_start, qmp_socket, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'running')",
                params![
                    run_id.as_str(),
                    instance_id.as_str(),
                    pid,
                    process_start,
                    qmp_socket.to_string_lossy().as_ref()
                ],
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(_, _) => Error::RunConflict(run_id.as_str().into()),
                other => Error::Sqlite(other),
            })?;
        self.run(&run_id)
    }

    pub fn run(&self, run_id: &Id) -> Result<Run> {
        self.db
            .query_row(
                "SELECT run_id, instance_id, pid, process_start, qmp_socket, status
                 FROM runs WHERE run_id = ?1",
                params![run_id.as_str()],
                |row| {
                    Ok(Run {
                        run_id: Id(row.get(0)?),
                        instance_id: Id(row.get(1)?),
                        pid: row.get(2)?,
                        process_start: row.get(3)?,
                        qmp_socket: PathBuf::from(row.get::<_, String>(4)?),
                        status: row.get(5)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| Error::NotFound {
                kind: "run",
                id: run_id.as_str().into(),
            })
    }

    pub fn reconcile_run(&self, run_id: &Id) -> Result<Run> {
        let run = self.run(run_id)?;
        let status = if process_identity_matches(run.pid, run.process_start) {
            "running"
        } else {
            "uncertain"
        };
        self.db.execute(
            "UPDATE runs SET status = ?1 WHERE run_id = ?2",
            params![status, run_id.as_str()],
        )?;
        self.run(run_id)
    }

    pub fn reconcile_active_runs(&self) -> Result<Vec<Run>> {
        let ids = {
            let mut statement = self.db.prepare(
                "SELECT run_id FROM runs WHERE status IN ('running', 'starting', 'uncertain')",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        ids.into_iter()
            .map(|id| self.reconcile_run(&Id(id)))
            .collect()
    }

    fn active_run(&self, instance_id: &Id) -> Result<Option<Run>> {
        self.db
            .query_row(
                "SELECT run_id, instance_id, pid, process_start, qmp_socket, status
                 FROM runs
                 WHERE instance_id = ?1 AND status IN ('running', 'starting', 'uncertain')
                 ORDER BY created_at DESC LIMIT 1",
                params![instance_id.as_str()],
                |row| {
                    Ok(Run {
                        run_id: Id(row.get(0)?),
                        instance_id: Id(row.get(1)?),
                        pid: row.get(2)?,
                        process_start: row.get(3)?,
                        qmp_socket: PathBuf::from(row.get::<_, String>(4)?),
                        status: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Error::from)
    }

    pub fn finish_run(&self, run_id: &Id, status: &str) -> Result<Run> {
        if !matches!(status, "exited" | "failed" | "uncertain") {
            return Err(Error::Process(format!(
                "invalid terminal run status {status:?}"
            )));
        }
        self.db.execute(
            "UPDATE runs SET status = ?1 WHERE run_id = ?2",
            params![status, run_id.as_str()],
        )?;
        self.run(run_id)
    }

    pub fn create_snapshot(
        &self,
        snapshot_id: Id,
        instance_id: Id,
        components: &[(String, PathBuf)],
    ) -> Result<Snapshot> {
        let instance = self.instance(&instance_id)?;
        if instance.state != "created" && instance.state != "stopped" {
            return Err(Error::SnapshotRequiresStopped);
        }
        let destination = self.root.join("snapshots").join(snapshot_id.as_str());
        if destination.exists() {
            return Err(Error::SnapshotConflict(snapshot_id.as_str().into()));
        }
        let staging = self.root.join("staging").join(format!(
            "snapshot-{}-{}",
            snapshot_id.as_str(),
            std::process::id()
        ));
        fs::create_dir_all(&staging).map_err(|source| Error::Io {
            path: staging.clone(),
            source,
        })?;
        let mut files = std::collections::BTreeMap::new();
        let result = (|| -> Result<()> {
            for (name, source) in components {
                let component = Path::new(name);
                if component.file_name().and_then(|value| value.to_str()) != Some(name.as_str()) {
                    return Err(Error::InvalidSnapshotComponent(name.clone()));
                }
                let bytes = fs::read(source).map_err(|source_error| Error::Io {
                    path: source.clone(),
                    source: source_error,
                })?;
                let digest = hex_digest(&bytes);
                fs::write(staging.join(name), &bytes).map_err(|source_error| Error::Io {
                    path: staging.join(name),
                    source: source_error,
                })?;
                files.insert(name.clone(), digest);
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        let manifest = Snapshot {
            snapshot_id: snapshot_id.clone(),
            instance_id: instance_id.clone(),
            generation: instance.revision,
            files,
        };
        let manifest_json = serde_json::to_vec(&manifest)?;
        let manifest_sha256 = hex_digest(&manifest_json);
        fs::write(staging.join("manifest.json"), &manifest_json).map_err(|source| Error::Io {
            path: staging.join("manifest.json"),
            source,
        })?;
        fs::rename(&staging, &destination).map_err(|source| Error::Io {
            path: destination.clone(),
            source,
        })?;
        self.db
            .execute(
                "INSERT INTO snapshots(snapshot_id, instance_id, generation, manifest_json, manifest_sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    snapshot_id.as_str(),
                    instance_id.as_str(),
                    manifest.generation,
                    manifest_json,
                    manifest_sha256
                ],
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(_, _) => {
                    Error::SnapshotConflict(snapshot_id.as_str().into())
                }
                other => Error::Sqlite(other),
            })?;
        Ok(manifest)
    }

    pub fn create_instance_snapshot(&self, snapshot_id: Id, instance_id: Id) -> Result<Snapshot> {
        let instance_dir = self.root.join("instances").join(instance_id.as_str());
        let mut components = Vec::new();
        for entry in fs::read_dir(&instance_dir).map_err(|source| Error::Io {
            path: instance_dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| Error::Io {
                path: instance_dir.clone(),
                source,
            })?;
            if entry
                .file_type()
                .map_err(|source| Error::Io {
                    path: entry.path(),
                    source,
                })?
                .is_file()
            {
                components.push((
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.path(),
                ));
            }
        }
        self.create_snapshot(snapshot_id, instance_id, &components)
    }

    pub fn snapshot(&self, snapshot_id: &Id) -> Result<Snapshot> {
        let bytes: Vec<u8> = self
            .db
            .query_row(
                "SELECT manifest_json FROM snapshots WHERE snapshot_id = ?1",
                params![snapshot_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound {
                kind: "snapshot",
                id: snapshot_id.as_str().into(),
            })?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn restore_snapshot(&self, snapshot_id: &Id, destination: &Path) -> Result<Snapshot> {
        let snapshot = self.snapshot(snapshot_id)?;
        let destination_preexists = destination.exists();
        if destination_preexists
            && fs::read_dir(destination)
                .map_err(|source| Error::Io {
                    path: destination.to_owned(),
                    source,
                })?
                .next()
                .is_some()
        {
            return Err(Error::SnapshotConflict(destination.display().to_string()));
        }
        let restore_destination = if destination_preexists {
            destination.with_extension("clone-staging")
        } else {
            destination.to_owned()
        };
        let source = self.root.join("snapshots").join(snapshot_id.as_str());
        let staging = restore_destination.with_extension("restore-staging");
        fs::create_dir_all(&staging).map_err(|source_error| Error::Io {
            path: staging.clone(),
            source: source_error,
        })?;
        for name in snapshot.files.keys() {
            let bytes = fs::read(source.join(name)).map_err(|source_error| Error::Io {
                path: source.join(name),
                source: source_error,
            })?;
            if hex_digest(&bytes) != snapshot.files[name] {
                let _ = fs::remove_dir_all(&staging);
                return Err(Error::DigestMismatch {
                    expected: snapshot.files[name].clone(),
                    actual: hex_digest(&bytes),
                });
            }
            fs::write(staging.join(name), bytes).map_err(|source_error| Error::Io {
                path: staging.join(name),
                source: source_error,
            })?;
        }
        fs::rename(&staging, &restore_destination).map_err(|source_error| Error::Io {
            path: restore_destination.clone(),
            source: source_error,
        })?;
        if destination_preexists {
            for entry in fs::read_dir(&restore_destination).map_err(|source| Error::Io {
                path: restore_destination.clone(),
                source,
            })? {
                let entry = entry.map_err(|source| Error::Io {
                    path: restore_destination.clone(),
                    source,
                })?;
                fs::rename(entry.path(), destination.join(entry.file_name())).map_err(
                    |source| Error::Io {
                        path: destination.join(entry.file_name()),
                        source,
                    },
                )?;
            }
            fs::remove_dir(&restore_destination).map_err(|source| Error::Io {
                path: restore_destination,
                source,
            })?;
        }
        Ok(snapshot)
    }

    pub fn clone_snapshot(
        &self,
        snapshot_id: &Id,
        instance_id: Id,
        profile_id: Id,
        destination: &Path,
    ) -> Result<Instance> {
        let snapshot = self.snapshot(snapshot_id)?;
        let source_instance = self.instance(&snapshot.instance_id)?;
        let instance = self.create_instance(instance_id, source_instance.image_id, profile_id)?;
        if let Err(error) = self.restore_snapshot(snapshot_id, destination) {
            let _ = self.db.execute(
                "DELETE FROM instances WHERE instance_id = ?1",
                params![instance.instance_id.as_str()],
            );
            let _ = fs::remove_dir_all(
                self.root
                    .join("instances")
                    .join(instance.instance_id.as_str()),
            );
            return Err(error);
        }
        Ok(instance)
    }

    #[cfg(unix)]
    pub fn start_instance(
        &self,
        operation_id: Id,
        run_id: Id,
        instance_id: Id,
        idempotency_key: &str,
        input_json: &str,
        argv: &[String],
        qmp_socket: &Path,
        stdout: Option<&Path>,
        stderr: Option<&Path>,
        qmp_timeout: Duration,
    ) -> Result<RunningInstance> {
        if let Some(active) = self.active_run(&instance_id)? {
            let reconciled = self.reconcile_run(&active.run_id)?;
            if reconciled.status == "running" {
                return Err(Error::ActiveRun(instance_id.as_str().into()));
            }
            self.finish_run(&active.run_id, "failed")?;
            let instance = self.instance(&instance_id)?;
            if instance.state == "running" || instance.state == "starting" {
                self.transition_instance(&instance_id, "error")?;
            }
        }
        let operation = self.begin_operation(
            operation_id,
            instance_id.clone(),
            "start",
            idempotency_key,
            input_json,
        )?;
        if operation.status != "accepted" {
            return Err(Error::Process(format!(
                "start operation {} is already {}",
                operation.operation_id.as_str(),
                operation.status
            )));
        }
        self.transition_instance(&instance_id, "starting")?;
        let mut process = match ManagedProcess::spawn(run_id.clone(), argv, stdout, stderr) {
            Ok(process) => process,
            Err(error) => {
                let _ = self.transition_instance(&instance_id, "error");
                return Err(error);
            }
        };
        let process_start = match process.process_start() {
            Ok(value) => value,
            Err(error) => {
                let _ = process.terminate();
                let _ = process.wait();
                let _ = self.transition_instance(&instance_id, "error");
                return Err(error);
            }
        };
        self.record_run(
            run_id.clone(),
            instance_id.clone(),
            process.pid,
            process_start,
            qmp_socket.to_owned(),
        )?;
        let deadline = Instant::now() + qmp_timeout;
        let mut qmp = loop {
            match QmpClient::connect(qmp_socket, qmp_timeout.min(Duration::from_millis(250))) {
                Ok(client) => break client,
                Err(error) if Instant::now() < deadline => {
                    if process.try_wait()?.is_some() {
                        let _ = self.finish_run(&run_id, "failed");
                        let _ = self.transition_instance(&instance_id, "error");
                        return Err(error);
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    let _ = process.terminate();
                    let _ = process.wait();
                    let _ = self.finish_run(&run_id, "failed");
                    let _ = self.transition_instance(&instance_id, "error");
                    return Err(error);
                }
            }
        };
        if let Err(error) = self.transition_instance(&instance_id, "running") {
            let _ = qmp.execute("quit", serde_json::Value::Null);
            let _ = process.wait();
            let _ = self.finish_run(&run_id, "failed");
            return Err(error);
        }
        self.complete_operation(&operation.operation_id, r#"{"state":"running"}"#)?;
        Ok(RunningInstance {
            process,
            qmp,
            run_id,
        })
    }

    #[cfg(unix)]
    pub fn pause_instance(
        &self,
        instance_id: &Id,
        running: &mut RunningInstance,
    ) -> Result<Instance> {
        let current = self.instance(instance_id)?;
        if current.state != "running" {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: "paused".into(),
            });
        }
        running.pause()?;
        self.transition_instance(instance_id, "paused")
    }

    #[cfg(unix)]
    pub fn resume_instance(
        &self,
        instance_id: &Id,
        running: &mut RunningInstance,
    ) -> Result<Instance> {
        let current = self.instance(instance_id)?;
        if current.state != "paused" {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: "running".into(),
            });
        }
        running.resume()?;
        self.transition_instance(instance_id, "running")
    }

    #[cfg(unix)]
    pub fn reset_instance(&self, instance_id: &Id, running: &mut RunningInstance) -> Result<()> {
        let current = self.instance(instance_id)?;
        if current.state != "running" && current.state != "paused" {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: "running".into(),
            });
        }
        running.reset()
    }

    #[cfg(unix)]
    pub fn stop_instance(
        &self,
        instance_id: &Id,
        running: &mut RunningInstance,
    ) -> Result<Instance> {
        let current = self.instance(instance_id)?;
        if current.state != "running" && current.state != "paused" {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: "stopped".into(),
            });
        }
        self.transition_instance(instance_id, "stopping")?;
        let result = running.qmp.execute("quit", serde_json::Value::Null);
        if result.is_err() {
            let _ = running.process.terminate();
        }
        let exit = running.process.wait()?;
        let status = if exit.success { "exited" } else { "failed" };
        self.finish_run(&running.run_id, status)?;
        if !exit.success {
            self.transition_instance(instance_id, "error")
        } else {
            self.transition_instance(instance_id, "stopped")
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.root.join("workspace.lock"));
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn digest_file(path: &Path) -> Result<String> {
    let mut input = File::open(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let count = input.read(&mut buffer).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn acquire_workspace_lock(path: &Path) -> Result<File> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => Ok(file),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            #[cfg(target_os = "linux")]
            {
                let stale = fs::read_to_string(path)
                    .ok()
                    .and_then(|value| {
                        let mut fields = value.split_whitespace();
                        let pid = fields.next()?.parse::<u32>().ok()?;
                        let start = fields.next()?.parse::<u64>().ok()?;
                        Some(!process_identity_matches(pid, start))
                    })
                    .unwrap_or(false);
                if stale {
                    fs::remove_file(path).map_err(|source| Error::Io {
                        path: path.to_owned(),
                        source,
                    })?;
                    return OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(path)
                        .map_err(|source| Error::Io {
                            path: path.to_owned(),
                            source,
                        });
                }
            }
            Err(Error::WorkspaceLocked(path.to_owned()))
        }
        Err(source) => Err(Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

fn allowed_transition(from: &str, to: &str) -> bool {
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
            | ("stopping", "stopped")
            | ("stopping", "error")
            | ("error", "starting")
    )
}

#[cfg(target_os = "linux")]
fn process_identity_matches(pid: u32, expected_start: u64) -> bool {
    process_start_identity(pid).is_some_and(|actual| actual == expected_start)
}

#[cfg(target_os = "linux")]
fn process_start_identity(pid: u32) -> Option<u64> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let Ok(contents) = fs::read_to_string(path) else {
        return None;
    };
    let Some(after_name) = contents.rsplit_once(") ").map(|(_, rest)| rest) else {
        return None;
    };
    after_name
        .split_whitespace()
        .nth(19)
        .and_then(|value| value.parse::<u64>().ok())
}

#[cfg(not(target_os = "linux"))]
fn process_identity_matches(_pid: u32, _expected_start: u64) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
fn process_start_identity(_pid: u32) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("machineemu-runtime-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn yaml_config_supports_server_only_and_relative_paths() {
        let root = temp_root("config");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("machineemu.yaml");
        fs::write(
            &path,
            "server:\n  workspace: ./workspace\n  unix_socket: ./control.sock\n",
        )
        .unwrap();
        let (config, loaded) = load_config(Some(&path)).unwrap();
        assert!(config.client.is_none());
        let server = config.server.unwrap();
        assert_eq!(server.workspace, Some(PathBuf::from("./workspace")));
        assert_eq!(
            resolve_config_path(loaded.as_deref(), server.unix_socket.unwrap()),
            root.join("control.sock")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn yaml_config_supports_engine_registry_and_optional_digest() {
        let root = temp_root("engine-config");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("machineemu.yaml");
        fs::write(
            &path,
            "engines:\n  unifi-10.2:\n    path: ./qemu-build\n    version: 10.2.4\n    build_digest: sha256:abc\n",
        )
        .unwrap();
        let (config, _) = load_config(Some(&path)).unwrap();
        let engine = config.engines.get("unifi-10.2").unwrap();
        assert_eq!(engine.path, PathBuf::from("./qemu-build"));
        assert_eq!(engine.version.as_deref(), Some("10.2.4"));
        assert_eq!(engine.build_digest.as_deref(), Some("sha256:abc"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_missing_helper_is_reported_as_a_missing_executable() {
        let argv = vec!["machineemu-absent-helper".to_string(), "socket".into()];
        let Err(error) = ManagedProcess::spawn(Id::new("run", "run01").unwrap(), &argv, None, None)
        else {
            panic!("an uninstalled helper cannot start");
        };
        assert!(
            matches!(&error, Error::ExecutableNotFound { executable } if executable == "machineemu-absent-helper"),
            "unexpected error: {error}"
        );
        assert_eq!(
            error.to_string(),
            "\"machineemu-absent-helper\" was not found on PATH"
        );
    }

    #[test]
    fn a_helper_path_that_cannot_run_keeps_its_cause() {
        let root = temp_root("spawn-not-executable");
        fs::create_dir_all(&root).unwrap();
        let helper = root.join("swtpm");
        fs::write(&helper, b"not an executable\n").unwrap();
        let argv = vec![helper.to_string_lossy().into_owned()];
        let Err(error) = ManagedProcess::spawn(Id::new("run", "run01").unwrap(), &argv, None, None)
        else {
            panic!("a non-executable file cannot start");
        };
        assert!(
            matches!(&error, Error::Spawn { executable, .. } if executable == &argv[0]),
            "unexpected error: {error}"
        );
        assert!(error.to_string().starts_with("cannot execute "));
        let _ = fs::remove_dir_all(root);
    }

    fn manifest() -> ImageManifest {
        ImageManifest {
            image_id: Id::new("image", "debian13-cloud").unwrap(),
            engine_track: Id::new("engine track", "unifi-10-2").unwrap(),
            target: "x86_64-softmmu".into(),
            disk_sha256: "a".repeat(64),
            firmware_sha256: Some("b".repeat(64)),
            tpm_state_sha256: None,
        }
    }

    #[test]
    fn workspace_owns_one_root_and_persists_records() {
        let root = temp_root("records");
        let workspace = Workspace::open(&root).unwrap();
        assert!(matches!(
            Workspace::open(&root),
            Err(Error::WorkspaceLocked(_))
        ));
        let image = manifest();
        let digest = workspace.register_image(&image).unwrap();
        assert_eq!(workspace.image(&image.image_id).unwrap(), image);
        let instance = workspace
            .create_instance(
                Id::new("instance", "lab01").unwrap(),
                image.image_id.clone(),
                Id::new("profile", "debian13-cloud").unwrap(),
            )
            .unwrap();
        assert_eq!(instance.state, "created");
        drop(workspace);
        let reopened = Workspace::open(&root).unwrap();
        assert_eq!(reopened.image(&image.image_id).unwrap(), image);
        assert_eq!(reopened.instance(&instance.instance_id).unwrap(), instance);
        assert_eq!(digest.len(), 64);
        drop(reopened);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ids_reject_unsafe_values() {
        assert!(Id::new("instance", "../escape").is_err());
        assert!(Id::new("instance", "UpperCase").is_err());
        assert!(Id::new("instance", "safe-01").is_ok());
        assert!(Id::new("engine track", "unifi-10.2").is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_workspace_lock_is_reclaimed_but_live_lock_is_rejected() {
        let root = temp_root("stale-lock");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("workspace.lock"), b"999999 1\n").unwrap();
        let workspace = Workspace::open(&root).unwrap();
        assert!(matches!(
            Workspace::open(&root),
            Err(Error::WorkspaceLocked(_))
        ));
        drop(workspace);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn operation_retries_are_idempotent_and_transitions_are_guarded() {
        let root = temp_root("operations");
        let workspace = Workspace::open(&root).unwrap();
        let image = manifest();
        workspace.register_image(&image).unwrap();
        let instance = workspace
            .create_instance(
                Id::new("instance", "lab01").unwrap(),
                image.image_id,
                Id::new("profile", "debian13-cloud").unwrap(),
            )
            .unwrap();
        assert!(matches!(
            workspace.transition_instance(&instance.instance_id, "running"),
            Err(Error::InvalidTransition { .. })
        ));
        let accepted = workspace
            .begin_operation(
                Id::new("operation", "op01").unwrap(),
                instance.instance_id.clone(),
                "start",
                "request-01",
                r#"{"revision":1}"#,
            )
            .unwrap();
        let retry = workspace
            .begin_operation(
                Id::new("operation", "op02").unwrap(),
                instance.instance_id.clone(),
                "start",
                "request-01",
                r#"{"revision":1}"#,
            )
            .unwrap();
        assert_eq!(retry.operation_id, accepted.operation_id);
        assert!(matches!(
            workspace.begin_operation(
                Id::new("operation", "op03").unwrap(),
                instance.instance_id.clone(),
                "start",
                "request-01",
                r#"{"revision":2}"#,
            ),
            Err(Error::OperationConflict { .. })
        ));
        workspace
            .transition_instance(&instance.instance_id, "starting")
            .unwrap();
        workspace
            .transition_instance(&instance.instance_id, "running")
            .unwrap();
        let completed = workspace
            .complete_operation(&accepted.operation_id, r#"{"ok":true}"#)
            .unwrap();
        assert_eq!(completed.status, "completed");
        drop(workspace);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stopped_instance_can_be_removed_but_snapshotted_instance_cannot() {
        let root = temp_root("remove-instance");
        let workspace = Workspace::open(&root).unwrap();
        let image = manifest();
        workspace.register_image(&image).unwrap();
        let instance_id = Id::new("instance", "lab01").unwrap();
        workspace
            .create_instance(
                instance_id.clone(),
                image.image_id,
                Id::new("profile", "debian13-cloud").unwrap(),
            )
            .unwrap();
        workspace.remove_instance(&instance_id).unwrap();
        assert!(matches!(
            workspace.instance(&instance_id),
            Err(Error::NotFound {
                kind: "instance",
                ..
            })
        ));
        drop(workspace);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn blob_import_verifies_before_atomic_publication() {
        let root = temp_root("blobs");
        let workspace = Workspace::open(&root).unwrap();
        let source = root.join("source.bin");
        fs::write(&source, b"immutable input").unwrap();
        let digest = hex_digest(b"immutable input");
        let imported = workspace.import_blob(&source, &digest).unwrap();
        assert_eq!(fs::read(&imported).unwrap(), b"immutable input");
        assert!(workspace.import_blob(&source, &"0".repeat(64)).is_err());
        assert!(!root.join("staging/import-").exists());
        drop(workspace);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn portable_image_bundle_round_trips_named_components() {
        let root = temp_root("image-bundle");
        let workspace = Workspace::open(&root).unwrap();
        let bundle = root.join("portable-image");
        fs::create_dir_all(bundle.join("components")).unwrap();
        let disk = b"portable disk contents";
        fs::write(bundle.join("components/disk.qcow2"), disk).unwrap();
        let manifest = ImageBundleManifest {
            schema_version: 1,
            image_id: Id::new("image", "portable-test").unwrap(),
            engine_track: Id::new("engine track", "unifi-10-2").unwrap(),
            target: "x86_64-softmmu".into(),
            components: [(
                "disk".into(),
                ImageBundleComponent {
                    path: "components/disk.qcow2".into(),
                    sha256: format!("sha256:{}", hex_digest(disk)),
                },
            )]
            .into_iter()
            .collect(),
        };
        fs::write(
            bundle.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let (image, _) = workspace.import_image_bundle(&bundle).unwrap();
        assert_eq!(image.disk_sha256, hex_digest(disk));
        let exported = root.join("exported-image");
        let exported_manifest = workspace
            .export_image_bundle(&image.image_id, &exported)
            .unwrap();
        assert_eq!(
            fs::read(exported.join("components/disk.qcow2")).unwrap(),
            disk
        );
        assert_eq!(exported_manifest.image_id, image.image_id);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn vmmanager_base_import_ignores_runtime_lock_files() {
        let root = temp_root("vmmanager-base");
        let source = root.join("source");
        fs::create_dir_all(source.join("tpm")).unwrap();
        fs::write(source.join("disk.qcow2"), b"base disk").unwrap();
        fs::write(source.join("OVMF_VARS.fd"), b"vars").unwrap();
        fs::write(source.join("tpm/tpm2-00.permall"), b"tpm state").unwrap();
        fs::write(source.join("tpm/.lock"), b"").unwrap();
        fs::write(source.join("tpm/swtpm.pid"), b"1234\n").unwrap();
        let workspace = Workspace::open(root.join("workspace")).unwrap();
        let image = workspace
            .import_vmmanager_base(
                &source,
                Id::new("image", "win11-dev").unwrap(),
                Id::new("engine", "qemu-10-2").unwrap(),
                "x86_64-softmmu",
            )
            .unwrap();
        assert_eq!(
            fs::read(
                workspace
                    .root()
                    .join("blobs/sha256")
                    .join(&image.disk_sha256)
            )
            .unwrap(),
            b"base disk"
        );
        assert!(image.firmware_sha256.is_some());
        assert!(image.tpm_state_sha256.is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn instance_preparation_creates_overlay_nvram_and_tpm_state() {
        let root = temp_root("instance-preparation");
        let workspace = Workspace::open(&root).unwrap();
        let image_root = root.join("source");
        fs::create_dir_all(&image_root).unwrap();
        let backing = image_root.join("disk.qcow2");
        let status = std::process::Command::new("qemu-img")
            .args(["create", "-f", "qcow2", backing.to_str().unwrap(), "1M"])
            .status()
            .unwrap();
        assert!(status.success());
        let nvram = image_root.join("vars.fd");
        let tpm = image_root.join("tpm.permall");
        fs::write(&nvram, b"vars").unwrap();
        fs::write(&tpm, b"tpm").unwrap();
        let instance = Id::new("instance", "lab01").unwrap();
        fs::create_dir_all(workspace.root().join("instances/lab01")).unwrap();
        let overlay = workspace
            .prepare_instance_files(&instance, &backing, "qcow2", Some(&nvram), Some(&tpm))
            .unwrap();
        assert!(overlay.is_file());
        assert_eq!(
            fs::read(workspace.root().join("instances/lab01/OVMF_VARS.fd")).unwrap(),
            b"vars"
        );
        assert_eq!(
            fs::read(workspace.root().join("instances/lab01/tpm/tpm2-00.permall")).unwrap(),
            b"tpm"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn qmp_client_negotiates_ignores_events_and_returns_commands() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;
        use std::thread;

        let root = temp_root("qmp");
        fs::create_dir_all(&root).unwrap();
        let socket = root.join("qmp.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(b"{\"QMP\":{\"version\":{},\"capabilities\":[]}}\r\n")
                .unwrap();
            let reader_stream = stream.try_clone().unwrap();
            let mut reader = BufReader::new(reader_stream);
            for _ in 0..2 {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                let id = request["id"].clone();
                if request["execute"] == "qmp_capabilities" {
                    stream
                        .write_all(format!("{{\"return\":{{}},\"id\":{id}}}\r\n").as_bytes())
                        .unwrap();
                } else {
                    stream.write_all(b"{\"event\":\"STOP\"}\r\n").unwrap();
                    stream
                        .write_all(
                            format!("{{\"return\":{{\"status\":\"running\"}},\"id\":{id}}}\r\n")
                                .as_bytes(),
                        )
                        .unwrap();
                }
            }
        });
        let mut client = QmpClient::connect(&socket, Duration::from_secs(1)).unwrap();
        let result = client
            .execute("query-status", serde_json::Value::Null)
            .unwrap();
        assert_eq!(result["status"], "running");
        server.join().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn process_supervisor_uses_argv_and_reports_exit() {
        let root = temp_root("process");
        fs::create_dir_all(&root).unwrap();
        let stdout = root.join("stdout.log");
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s' \"$1\"".into(),
            "machineemu-test".into(),
            "argv-value".into(),
        ];
        let run_id = Id::new("run", "run01").unwrap();
        let mut process =
            ManagedProcess::spawn(run_id.clone(), &argv, Some(&stdout), None).unwrap();
        let exit = process.wait().unwrap();
        assert!(exit.success);
        assert_eq!(process.run_id, run_id);
        assert_eq!(fs::read_to_string(stdout).unwrap(), "argv-value");
        drop(process);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn run_reconciliation_rejects_a_dead_or_reused_process_identity() {
        let root = temp_root("runs");
        let workspace = Workspace::open(&root).unwrap();
        let image = manifest();
        workspace.register_image(&image).unwrap();
        let instance = workspace
            .create_instance(
                Id::new("instance", "lab01").unwrap(),
                image.image_id,
                Id::new("profile", "debian13-cloud").unwrap(),
            )
            .unwrap();
        let argv = vec!["/bin/sh".into(), "-c".into(), "sleep 2".into()];
        let mut process =
            ManagedProcess::spawn(Id::new("run", "run01").unwrap(), &argv, None, None).unwrap();
        let run = workspace
            .record_run(
                process.run_id.clone(),
                instance.instance_id,
                process.pid,
                process.process_start().unwrap(),
                root.join("qmp.sock"),
            )
            .unwrap();
        assert_eq!(
            workspace.reconcile_run(&run.run_id).unwrap().status,
            "running"
        );
        process.terminate().unwrap();
        process.wait().unwrap();
        assert_eq!(
            workspace.reconcile_run(&run.run_id).unwrap().status,
            "uncertain"
        );
        assert_eq!(workspace.reconcile_active_runs().unwrap().len(), 1);
        drop(workspace);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn start_orchestrates_operation_process_run_and_qmp() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;
        use std::thread;

        let root = temp_root("start");
        let workspace = Workspace::open(&root).unwrap();
        let image = manifest();
        workspace.register_image(&image).unwrap();
        let instance = workspace
            .create_instance(
                Id::new("instance", "lab01").unwrap(),
                image.image_id,
                Id::new("profile", "debian13-cloud").unwrap(),
            )
            .unwrap();
        let socket = root.join("qmp.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(b"{\"QMP\":{\"version\":{},\"capabilities\":[]}}\r\n")
                .unwrap();
            let reader_stream = stream.try_clone().unwrap();
            let mut reader = BufReader::new(reader_stream);
            for _ in 0..5 {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                stream
                    .write_all(
                        format!("{{\"return\":{{}},\"id\":{}}}\r\n", request["id"]).as_bytes(),
                    )
                    .unwrap();
            }
        });
        let argv = vec!["/bin/sh".into(), "-c".into(), "sleep 2".into()];
        let mut running = workspace
            .start_instance(
                Id::new("operation", "op01").unwrap(),
                Id::new("run", "run01").unwrap(),
                instance.instance_id.clone(),
                "start-01",
                r#"{"profile":"debian13-cloud"}"#,
                &argv,
                &socket,
                None,
                None,
                Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(
            workspace.instance(&instance.instance_id).unwrap().state,
            "running"
        );
        assert_eq!(workspace.run(&running.run_id).unwrap().status, "running");
        assert_eq!(
            workspace
                .pause_instance(&instance.instance_id, &mut running)
                .unwrap()
                .state,
            "paused"
        );
        assert_eq!(
            workspace
                .resume_instance(&instance.instance_id, &mut running)
                .unwrap()
                .state,
            "running"
        );
        workspace
            .reset_instance(&instance.instance_id, &mut running)
            .unwrap();
        assert_eq!(
            workspace
                .stop_instance(&instance.instance_id, &mut running)
                .unwrap()
                .state,
            "stopped"
        );
        assert_eq!(workspace.run(&running.run_id).unwrap().status, "exited");
        server.join().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stopped_snapshot_restores_hashed_components() {
        let root = temp_root("snapshot");
        let workspace = Workspace::open(&root).unwrap();
        let image = manifest();
        workspace.register_image(&image).unwrap();
        let instance = workspace
            .create_instance(
                Id::new("instance", "lab01").unwrap(),
                image.image_id,
                Id::new("profile", "debian13-cloud").unwrap(),
            )
            .unwrap();
        workspace
            .transition_instance(&instance.instance_id, "starting")
            .unwrap();
        assert!(matches!(
            workspace.create_snapshot(
                Id::new("snapshot", "snap-running").unwrap(),
                instance.instance_id.clone(),
                &[],
            ),
            Err(Error::SnapshotRequiresStopped)
        ));
        workspace
            .transition_instance(&instance.instance_id, "running")
            .unwrap();
        workspace
            .transition_instance(&instance.instance_id, "stopping")
            .unwrap();
        workspace
            .transition_instance(&instance.instance_id, "stopped")
            .unwrap();
        let source = root.join("disk.qcow2");
        fs::write(&source, b"before").unwrap();
        let snapshot = workspace
            .create_snapshot(
                Id::new("snapshot", "snap01").unwrap(),
                instance.instance_id,
                &[("disk.qcow2".into(), source.clone())],
            )
            .unwrap();
        assert_eq!(snapshot.files["disk.qcow2"], hex_digest(b"before"));
        fs::write(&source, b"after").unwrap();
        let restored = root.join("restored");
        workspace
            .restore_snapshot(&snapshot.snapshot_id, &restored)
            .unwrap();
        assert_eq!(fs::read(restored.join("disk.qcow2")).unwrap(), b"before");
        let clone_destination = root.join("clone-files");
        let clone = workspace
            .clone_snapshot(
                &snapshot.snapshot_id,
                Id::new("instance", "lab02").unwrap(),
                Id::new("profile", "debian13-cloud").unwrap(),
                &clone_destination,
            )
            .unwrap();
        assert_eq!(clone.state, "created");
        assert_eq!(
            fs::read(clone_destination.join("disk.qcow2")).unwrap(),
            b"before"
        );
        assert_ne!(clone.instance_id, snapshot.instance_id);
        let _ = fs::remove_dir_all(root);
    }
}
