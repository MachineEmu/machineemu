use super::*;
use axum::{Json, response::Response};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    os::unix::fs::FileTypeExt,
    path::Path as FsPath,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};
use utoipa::ToSchema;

const MAX_REPLY_BYTES: u64 = 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 512 * 1024;
const COMMANDS: &[(&str, &str)] = &[
    ("guest-get-host-name", "hostname"),
    ("guest-get-osinfo", "os"),
    ("guest-network-get-interfaces", "interfaces"),
    ("guest-get-fsinfo", "filesystems"),
    ("guest-get-users", "users"),
    ("guest-get-timezone", "timezone"),
    ("guest-get-vcpus", "vcpus"),
];

#[derive(Serialize, ToSchema)]
pub(super) struct GuestAgentInformation {
    available: bool,
    instance_id: String,
    run_id: Option<String>,
    reason: Option<&'static str>,
    version: Option<String>,
    hostname: Option<String>,
    os: Option<Value>,
    interfaces: Option<Value>,
    filesystems: Option<Value>,
    users: Option<Value>,
    timezone: Option<Value>,
    vcpus: Option<Value>,
}

impl GuestAgentInformation {
    fn unavailable(instance_id: String, run_id: Option<String>, reason: &'static str) -> Self {
        Self {
            available: false,
            instance_id,
            run_id,
            reason: Some(reason),
            version: None,
            hostname: None,
            os: None,
            interfaces: None,
            filesystems: None,
            users: None,
            timezone: None,
            vcpus: None,
        }
    }
}

pub(super) async fn get_guest_agent_information(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let Ok(instance_id) = Id::new("instance", id.clone()) else {
        return error(StatusCode::BAD_REQUEST, "invalid_instance_id");
    };
    let initial_state = state.clone();
    let initial_id = instance_id.clone();
    let result = blocking(move || {
        let (run, socket) = {
            let lock = instance_lock(&initial_state, initial_id.as_str())?;
            let _guard = lock.blocking_lock();
            let workspace = initial_state
                .workspace
                .lock()
                .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
            workspace.instance(&initial_id)?;
            let run = workspace.live_run(&initial_id)?;
            let socket = workspace
                .root()
                .join("instances")
                .join(initial_id.as_str())
                .join("qga.sock");
            (run, socket)
        };
        Ok::<_, RuntimeError>((run, socket))
    })
    .await;
    let (run, socket) = match result {
        Ok(value) => value,
        Err(RuntimeError::NotFound { .. }) => {
            return error(StatusCode::NOT_FOUND, "instance_not_found");
        }
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "guest_agent_unavailable"),
    };
    let Some(run) = run else {
        return Json(GuestAgentInformation::unavailable(id, None, "not_running")).into_response();
    };
    let run_id = Some(run.run_id.as_str().to_owned());
    if !tokio::fs::symlink_metadata(&socket)
        .await
        .is_ok_and(|metadata| metadata.file_type().is_socket())
    {
        return Json(GuestAgentInformation::unavailable(
            id,
            run_id,
            "not_configured",
        ))
        .into_response();
    }
    let info = read_information(&socket, id.clone(), run_id.clone())
        .await
        .unwrap_or_else(|_| {
            GuestAgentInformation::unavailable(id.clone(), run_id.clone(), "not_responding")
        });
    let current_state = state.clone();
    let current_id = instance_id.clone();
    let current = blocking(move || {
        let lock = instance_lock(&current_state, current_id.as_str())?;
        let _guard = lock.blocking_lock();
        let workspace = current_state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.live_run(&current_id)
    })
    .await;
    match current {
        Ok(current) if current.as_ref().map(|run| run.run_id.as_str()) == run_id.as_deref() => {
            Json(info).into_response()
        }
        Ok(current) => Json(GuestAgentInformation::unavailable(
            id,
            current.map(|run| run.run_id.as_str().to_owned()),
            "run_changed",
        ))
        .into_response(),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "guest_agent_unavailable"),
    }
}

fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(ErrorBody { error: code.into() })).into_response()
}

pub(super) struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    pub(super) async fn connect(socket: &FsPath) -> std::io::Result<Self> {
        let stream = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(socket))
            .await
            .map_err(std::io::Error::other)??;
        let (reader, writer) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(reader),
            writer,
        };
        let sync_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;
        client.write(&[0xff]).await?;
        client
            .write(
                format!(
                    "{}\n",
                    json!({"execute":"guest-sync", "arguments":{"id":sync_id}})
                )
                .as_bytes(),
            )
            .await?;
        for _ in 0..4 {
            let response = client.reply().await?;
            if response.get("return").and_then(Value::as_i64) == Some(sync_id) {
                return Ok(client);
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "guest agent did not synchronize",
        ))
    }

    async fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        tokio::time::timeout(Duration::from_secs(2), self.writer.write_all(bytes))
            .await
            .map_err(std::io::Error::other)??;
        Ok(())
    }

    async fn reply(&mut self) -> std::io::Result<Value> {
        let mut line = Vec::new();
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let chunk = self.reader.fill_buf().await?;
                if chunk.is_empty() {
                    break;
                }
                let size = chunk
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(chunk.len(), |index| index + 1);
                if line.len() + size > MAX_REPLY_BYTES as usize {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "guest agent reply is too large",
                    ));
                }
                line.extend_from_slice(&chunk[..size]);
                self.reader.consume(size);
                if line.ends_with(b"\n") {
                    break;
                }
            }
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(std::io::Error::other)?;
        result?;
        if !line.ends_with(b"\n") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "guest agent reply is missing",
            ));
        }
        let line = line.strip_prefix(&[0xff]).unwrap_or(&line);
        serde_json::from_slice(line).map_err(std::io::Error::other)
    }

    pub(super) async fn command(&mut self, name: &str) -> std::io::Result<Option<Value>> {
        self.command_args(name, Value::Null).await
    }

    pub(super) async fn command_args(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> std::io::Result<Option<Value>> {
        let mut request = json!({"execute":name});
        if !arguments.is_null() {
            request["arguments"] = arguments;
        }
        self.write(format!("{request}\n").as_bytes()).await?;
        let response = self.reply().await?;
        Ok(response.get("return").cloned())
    }
}

async fn read_information(
    socket: &FsPath,
    instance_id: String,
    run_id: Option<String>,
) -> std::io::Result<GuestAgentInformation> {
    let mut client = Client::connect(socket).await?;
    let info = client
        .command("guest-info")
        .await?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "guest-info failed"))?;
    let version = info
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let supported: BTreeSet<&str> = info
        .get("supported_commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|command| command.get("enabled").and_then(Value::as_bool) == Some(true))
        .filter_map(|command| command.get("name").and_then(Value::as_str))
        .collect();
    let mut response = GuestAgentInformation {
        available: true,
        instance_id,
        run_id,
        reason: None,
        version,
        hostname: None,
        os: None,
        interfaces: None,
        filesystems: None,
        users: None,
        timezone: None,
        vcpus: None,
    };
    let mut total = 0;
    for &(command, field) in COMMANDS {
        if !supported.contains(command) {
            continue;
        }
        let value = match client.command(command).await {
            Ok(Some(value)) => value,
            Ok(None) => continue,
            Err(_) => break,
        };
        let size = serde_json::to_vec(&value)
            .map_err(std::io::Error::other)?
            .len();
        if total + size > MAX_TOTAL_BYTES {
            break;
        }
        total += size;
        match field {
            "hostname" => {
                response.hostname = value
                    .get("host-name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }
            "os" => response.os = value.is_object().then_some(value),
            "interfaces" => response.interfaces = value.is_array().then_some(value),
            "filesystems" => response.filesystems = value.is_array().then_some(value),
            "users" => response.users = value.is_array().then_some(value),
            "timezone" => response.timezone = value.is_object().then_some(value),
            "vcpus" => response.vcpus = value.is_array().then_some(value),
            _ => unreachable!(),
        }
    }
    Ok(response)
}

pub(super) async fn guest_ipv4(socket: &FsPath) -> Option<String> {
    let mut client = Client::connect(socket).await.ok()?;
    let value = client
        .command("guest-network-get-interfaces")
        .await
        .ok()??;
    let interfaces = value.as_array()?;
    for interface in interfaces {
        let Some(addresses) = interface.get("ip-addresses").and_then(Value::as_array) else {
            continue;
        };
        for address in addresses {
            if address.get("ip-address-type").and_then(Value::as_str) != Some("ipv4") {
                continue;
            }
            let Some(ip) = address.get("ip-address").and_then(Value::as_str) else {
                continue;
            };
            if !ip.starts_with("127.") && !ip.starts_with("169.254.") {
                return Some(ip.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
        thread,
    };

    #[tokio::test]
    async fn reads_supported_guest_information_and_ignores_unsupported_commands() {
        let root = std::env::temp_dir().join(format!("machineemu-qga-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let socket = root.join("qga.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).unwrap();
            let sync: Value = serde_json::from_slice(line.strip_prefix(&[0xff]).unwrap()).unwrap();
            writer
                .write_all(format!("{}\n", json!({"return":sync["arguments"]["id"]})).as_bytes())
                .unwrap();
            let commands = [
                (
                    "guest-info",
                    json!({"version":"9.2", "supported_commands":[
                        {"name":"guest-get-host-name","enabled":true},
                        {"name":"guest-get-osinfo","enabled":true},
                        {"name":"guest-network-get-interfaces","enabled":true},
                        {"name":"guest-get-fsinfo","enabled":false}
                    ]}),
                ),
                ("guest-get-host-name", json!({"host-name":"lab-guest"})),
                ("guest-get-osinfo", json!({"pretty-name":"Test OS"})),
                (
                    "guest-network-get-interfaces",
                    json!([{"name":"eth0","ip-addresses":[{"ip-address":"10.0.0.2","ip-address-type":"ipv4","prefix":24}]}]),
                ),
            ];
            for (expected, value) in commands {
                line.clear();
                reader.read_until(b'\n', &mut line).unwrap();
                let request: Value = serde_json::from_slice(&line).unwrap();
                assert_eq!(request["execute"], expected);
                writer
                    .write_all(format!("{}\n", json!({"return":value})).as_bytes())
                    .unwrap();
            }
        });
        let info = read_information(&socket, "lab01".into(), Some("run01".into()))
            .await
            .unwrap();
        assert!(info.available);
        assert_eq!(info.version.as_deref(), Some("9.2"));
        assert_eq!(info.hostname.as_deref(), Some("lab-guest"));
        assert_eq!(info.os.as_ref().unwrap()["pretty-name"], "Test OS");
        assert_eq!(info.interfaces.as_ref().unwrap()[0]["name"], "eth0");
        assert!(info.filesystems.is_none());
        server.join().unwrap();
        std::fs::remove_file(&socket).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }
}
