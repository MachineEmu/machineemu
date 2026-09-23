use super::*;
use axum::extract::{
    Query,
    ws::{Message, WebSocket, WebSocketUpgrade},
};
use futures_util::{SinkExt, StreamExt};
use machineemu_core::protocols::qmp::QmpClient;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::Read,
    os::fd::AsRawFd,
    os::unix::fs::FileTypeExt,
    os::unix::net::UnixStream as StdUnixStream,
    time::{Duration, Instant},
};
use tokio::io::AsyncBufReadExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_MESSAGE: usize = 1_048_576;
const MAX_VIDEO_RECORD: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub(super) enum Endpoint {
    Unix(PathBuf),
    Tcp(SocketAddr),
}

#[derive(Clone)]
pub(super) struct StreamTicket {
    pub(super) instance_id: String,
    pub(super) run_id: String,
    pub(super) kind: String,
    pub(super) control: bool,
    pub(super) endpoint: Endpoint,
    pub(super) expires: Instant,
    pub(super) audio_group: Option<String>,
}

pub(super) struct AudioSession {
    pub instance_id: String,
    pub(super) run_id: String,
    pub(super) connection_id: Option<u32>,
    pub(super) active: BTreeSet<String>,
    pub(super) microphone: bool,
    pub(super) expires: Instant,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SpiceTicketRequest {
    #[serde(default)]
    microphone: bool,
}

#[derive(Deserialize)]
pub(super) struct TicketRequest {
    #[serde(default)]
    control: bool,
}

#[derive(Deserialize)]
pub(super) struct TicketQuery {
    ticket: String,
}

fn socket_endpoint(root: &std::path::Path, id: &str, name: &str) -> Result<Endpoint, RuntimeError> {
    let path = root.join("instances").join(id).join(name);
    let metadata = std::fs::symlink_metadata(&path).map_err(|source| RuntimeError::Io {
        path: path.clone(),
        source,
    })?;
    if !metadata.file_type().is_socket() {
        return Err(RuntimeError::Process(format!(
            "{} is not a Unix socket",
            path.display()
        )));
    }
    Ok(Endpoint::Unix(path))
}

fn vnc_endpoint(
    root: &std::path::Path,
    id: &str,
    qmp_socket: &std::path::Path,
) -> Result<Endpoint, RuntimeError> {
    let mut qmp = QmpClient::connect(qmp_socket, Duration::from_secs(2))?;
    let info = qmp.execute("query-vnc", Value::Null)?;
    if info.get("enabled") != Some(&Value::Bool(true)) {
        return Err(RuntimeError::Process("VNC is disabled for this run".into()));
    }
    match info.get("family").and_then(Value::as_str) {
        Some("unix") => {
            let path = info
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| RuntimeError::Process("VNC has no Unix socket path".into()))?;
            let path = PathBuf::from(path);
            let instance_dir =
                root.join("instances")
                    .join(id)
                    .canonicalize()
                    .map_err(|source| RuntimeError::Io {
                        path: root.join("instances").join(id),
                        source,
                    })?;
            let canonical = path.canonicalize().map_err(|source| RuntimeError::Io {
                path: path.clone(),
                source,
            })?;
            if !canonical.starts_with(instance_dir) {
                return Err(RuntimeError::Process(
                    "VNC socket is outside the instance directory".into(),
                ));
            }
            let metadata = std::fs::symlink_metadata(&path).map_err(|source| RuntimeError::Io {
                path: path.clone(),
                source,
            })?;
            if !metadata.file_type().is_socket() {
                return Err(RuntimeError::Process(
                    "VNC endpoint is not a Unix socket".into(),
                ));
            }
            Ok(Endpoint::Unix(path))
        }
        Some(family @ ("ipv4" | "ipv6")) => {
            let port = info
                .get("service")
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<u16>().ok())
                .filter(|port| *port != 0)
                .ok_or_else(|| RuntimeError::Process("VNC has no numeric TCP port".into()))?;
            let address = if family == "ipv6" {
                SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port))
            } else {
                SocketAddr::from(([127, 0, 0, 1], port))
            };
            Ok(Endpoint::Tcp(address))
        }
        _ => Err(RuntimeError::Process("unsupported VNC endpoint".into())),
    }
}

fn video_endpoint(
    state: &AppState,
    root: &std::path::Path,
    id: &str,
    run: &machineemu_core::domain::Run,
) -> Result<Endpoint, RuntimeError> {
    let lock = instance_lock(state, id)?;
    let _guard = lock
        .lock()
        .map_err(|_| RuntimeError::Process("instance lock poisoned".into()))?;
    let mut processes = state
        .display_streams
        .lock()
        .map_err(|_| RuntimeError::Process("display stream map lock poisoned".into()))?;
    if let Some(process) = processes.get_mut(id) {
        if process.run_id == run.run_id && process.try_wait()?.is_none() {
            return socket_endpoint(root, id, "video.sock");
        }
        processes.remove(id);
    }
    let directory = root.join("instances").join(id);
    std::fs::create_dir_all(&directory).map_err(|source| RuntimeError::Io {
        path: directory.clone(),
        source,
    })?;
    let output = directory.join("video.sock");
    if let Ok(metadata) = std::fs::symlink_metadata(&output) {
        if !metadata.file_type().is_socket() {
            return Err(RuntimeError::Process(
                "video output path is not a socket".into(),
            ));
        }
        std::fs::remove_file(&output).map_err(|source| RuntimeError::Io {
            path: output.clone(),
            source,
        })?;
    }
    let (client, qemu_end) = StdUnixStream::pair()
        .map_err(|error| RuntimeError::Process(format!("display socket pair failed: {error}")))?;
    let argv = vec![
        state.display_stream.to_string_lossy().into_owned(),
        "--bus-fd".into(),
        "0".into(),
        "--output".into(),
        output.to_string_lossy().into_owned(),
        "--record".into(),
        directory.join("screen.mp4").to_string_lossy().into_owned(),
    ];
    let mut process = ManagedProcess::spawn_with_stdin(
        run.run_id.clone(),
        &argv,
        None,
        Some(&directory.join("video.log")),
        client,
    )?;
    let mut qmp = QmpClient::connect(&run.qmp_socket, Duration::from_secs(2))?;
    qmp.attach_dbus_display(qemu_end.as_raw_fd())?;
    drop(qemu_end);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if process.try_wait()?.is_some() {
            return Err(RuntimeError::Process(format!(
                "display-stream exited; inspect {}",
                directory.join("video.log").display()
            )));
        }
        if let Ok(endpoint) = socket_endpoint(root, id, "video.sock") {
            // The streamer binds before registering its QEMU D-Bus listener.
            // Give an immediate registration failure time to reach the child.
            std::thread::sleep(Duration::from_millis(250));
            if process.try_wait()?.is_some() {
                return Err(RuntimeError::Process(format!(
                    "display-stream could not attach to QEMU; inspect {}",
                    directory.join("video.log").display()
                )));
            }
            processes.insert(id.to_owned(), process);
            return Ok(endpoint);
        }
        if Instant::now() >= deadline {
            return Err(RuntimeError::Process(
                "display-stream did not create video.sock".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn random_ticket() -> Result<String, RuntimeError> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| RuntimeError::Process(format!("cannot create stream ticket: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(super) async fn issue_stream_ticket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, kind)): Path<(String, String)>,
    axum::Json(request): axum::Json<TicketRequest>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking({
        let state = state.clone();
        move || {
            let id_value = Id::new("instance", id.clone())?;
            if !matches!(
                kind.as_str(),
                "vnc" | "video" | "audio-dbus" | "usbredir" | "lcm" | "frontpanel"
            ) {
                return Err(RuntimeError::Process("unknown stream kind".into()));
            }
            if matches!(kind.as_str(), "vnc" | "usbredir") && !request.control {
                return Err(RuntimeError::Process(
                    "this stream requires control permission".into(),
                ));
            }
            if matches!(kind.as_str(), "audio-dbus" | "lcm" | "frontpanel") && request.control {
                return Err(RuntimeError::Process("this stream is read only".into()));
            }
            let workspace = {
                let owner = state
                    .workspace
                    .lock()
                    .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
                owner.attach()?
            };
            let run = workspace
                .live_run(&id_value)?
                .ok_or_else(|| RuntimeError::Process("instance has no live run".into()))?;
            let endpoint = match kind.as_str() {
                "vnc" => vnc_endpoint(workspace.root(), &id, &run.qmp_socket)?,
                "video" | "audio-dbus" => video_endpoint(&state, workspace.root(), &id, &run)?,
                "usbredir" => socket_endpoint(workspace.root(), &id, "usbredir.sock")?,
                "lcm" => socket_endpoint(workspace.root(), &id, "display.sock")?,
                "frontpanel" => socket_endpoint(workspace.root(), &id, "frontpanel.sock")?,
                _ => unreachable!(),
            };
            let value = random_ticket()?;
            let mut tickets = state
                .stream_tickets
                .lock()
                .map_err(|_| RuntimeError::Process("stream ticket lock poisoned".into()))?;
            tickets.retain(|_, ticket| ticket.expires > Instant::now());
            if tickets.len() >= 128 {
                return Err(RuntimeError::Process(
                    "too many outstanding stream tickets".into(),
                ));
            }
            tickets.insert(
                value.clone(),
                StreamTicket {
                    instance_id: id,
                    run_id: run.run_id.as_str().into(),
                    kind: kind.clone(),
                    control: request.control,
                    endpoint,
                    expires: Instant::now() + Duration::from_secs(30),
                    audio_group: None,
                },
            );
            Ok(serde_json::json!({"ticket": value, "expires_in_seconds": 30, "kind": kind}))
        }
    })
    .await;
    match result {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

fn spice_endpoint(
    root: &std::path::Path,
    id: &str,
    qmp_socket: &std::path::Path,
) -> Result<Endpoint, RuntimeError> {
    let mut qmp = QmpClient::connect(qmp_socket, Duration::from_secs(2))?;
    let info = qmp.execute("query-spice", Value::Null)?;
    if info.get("enabled") != Some(&Value::Bool(true))
        || info.get("auth").and_then(Value::as_str) != Some("none")
    {
        return Err(RuntimeError::Process(
            "audio-only SPICE server is unavailable".into(),
        ));
    }
    let path = qmp_socket.with_file_name("spice.sock");
    if info.get("host").and_then(Value::as_str) != path.to_str() {
        return Err(RuntimeError::Process(
            "SPICE is not bound to the instance audio socket".into(),
        ));
    }
    let instance = root
        .join("instances")
        .join(id)
        .canonicalize()
        .map_err(|source| RuntimeError::Io {
            path: root.join("instances").join(id),
            source,
        })?;
    let canonical = path.canonicalize().map_err(|source| RuntimeError::Io {
        path: path.clone(),
        source,
    })?;
    if !canonical.starts_with(instance) {
        return Err(RuntimeError::Process(
            "SPICE socket is outside the instance directory".into(),
        ));
    }
    let metadata = std::fs::symlink_metadata(&path).map_err(|source| RuntimeError::Io {
        path: path.clone(),
        source,
    })?;
    if !metadata.file_type().is_socket() {
        return Err(RuntimeError::Process(
            "SPICE endpoint is not a Unix socket".into(),
        ));
    }
    Ok(Endpoint::Unix(path))
}

pub(super) async fn issue_spice_tickets(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(request): axum::Json<SpiceTicketRequest>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || {
        let instance_id = Id::new("instance", id.clone())?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?
            .attach()?;
        let run = workspace
            .live_run(&instance_id)?
            .ok_or_else(|| RuntimeError::Process("instance has no live run".into()))?;
        let endpoint = spice_endpoint(workspace.root(), &id, &run.qmp_socket)?;
        let group = random_ticket()?;
        let channels = if request.microphone {
            vec!["main", "playback", "record"]
        } else {
            vec!["main", "playback"]
        };
        let mut tokens = serde_json::Map::new();
        let mut tickets = state
            .stream_tickets
            .lock()
            .map_err(|_| RuntimeError::Process("stream ticket lock poisoned".into()))?;
        tickets.retain(|_, ticket| ticket.expires > Instant::now());
        if tickets.len() + channels.len() > 128 {
            return Err(RuntimeError::Process(
                "too many outstanding stream tickets".into(),
            ));
        }
        let mut sessions = state
            .audio_sessions
            .lock()
            .map_err(|_| RuntimeError::Process("audio session map lock poisoned".into()))?;
        sessions
            .retain(|_, session| session.expires > Instant::now() || !session.active.is_empty());
        if sessions.len() >= 128 {
            return Err(RuntimeError::Process("too many audio sessions".into()));
        }
        sessions.insert(
            group.clone(),
            AudioSession {
                instance_id: id.clone(),
                run_id: run.run_id.as_str().into(),
                connection_id: None,
                active: BTreeSet::new(),
                microphone: request.microphone,
                expires: Instant::now() + Duration::from_secs(120),
            },
        );
        for channel in channels {
            let token = random_ticket()?;
            let kind = format!("spice-{channel}");
            tickets.insert(
                token.clone(),
                StreamTicket {
                    instance_id: id.clone(),
                    run_id: run.run_id.as_str().into(),
                    kind,
                    control: channel == "record",
                    endpoint: endpoint.clone(),
                    expires: Instant::now() + Duration::from_secs(30),
                    audio_group: Some(group.clone()),
                },
            );
            tokens.insert(channel.into(), Value::String(token));
        }
        Ok(serde_json::json!({"client_token":group,"tickets":tokens,"expires_in_seconds":30}))
    })
    .await;
    match result {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

fn same_origin(headers: &HeaderMap) -> bool {
    let host = headers.get("host").and_then(|value| value.to_str().ok());
    let origin = headers.get("origin").and_then(|value| value.to_str().ok());
    matches!((host, origin), (Some(host), Some(origin))
        if origin == format!("http://{host}") || origin == format!("https://{host}"))
}

struct ControlLease {
    key: String,
    owners: Arc<Mutex<BTreeSet<String>>>,
}

struct AudioLease {
    group: String,
    channel: String,
    sessions: Arc<Mutex<BTreeMap<String, AudioSession>>>,
}
impl Drop for AudioLease {
    fn drop(&mut self) {
        if let Ok(mut sessions) = self.sessions.lock() {
            if self.channel == "main" {
                sessions.remove(&self.group);
            } else if let Some(session) = sessions.get_mut(&self.group) {
                session.active.remove(&self.channel);
            }
        }
    }
}
impl Drop for ControlLease {
    fn drop(&mut self) {
        if let Ok(mut owners) = self.owners.lock() {
            owners.remove(&self.key);
        }
    }
}

pub(super) async fn connect_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, kind)): Path<(String, String)>,
    Query(query): Query<TicketQuery>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    if !same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let ticket = match state.stream_tickets.lock() {
        Ok(mut tickets) => tickets.remove(&query.ticket),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let Some(ticket) = ticket else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if ticket.expires <= Instant::now() || ticket.instance_id != id || ticket.kind != kind {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let live = {
        let state = state.clone();
        let id = id.clone();
        blocking(move || {
            let instance_id = Id::new("instance", id)?;
            let workspace = state
                .workspace
                .lock()
                .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
            workspace.live_run(&instance_id)
        })
        .await
    };
    if !matches!(live, Ok(Some(ref run)) if run.run_id.as_str() == ticket.run_id) {
        return StatusCode::CONFLICT.into_response();
    }
    let lease = if ticket.control {
        let key = format!("{}:{}:{}", ticket.instance_id, ticket.run_id, ticket.kind);
        let mut owners = match state.control_streams.lock() {
            Ok(owners) => owners,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        if !owners.insert(key.clone()) {
            return StatusCode::CONFLICT.into_response();
        }
        Some(ControlLease {
            key,
            owners: state.control_streams.clone(),
        })
    } else {
        None
    };
    let audio = if let Some(group) = ticket.audio_group.as_ref() {
        let channel = ticket.kind.strip_prefix("spice-").unwrap_or_default();
        let mut sessions = match state.audio_sessions.lock() {
            Ok(sessions) => sessions,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        let Some(session) = sessions.get_mut(group) else {
            return StatusCode::CONFLICT.into_response();
        };
        if session.instance_id != id
            || session.run_id != ticket.run_id
            || session.expires <= Instant::now()
            || session.active.contains(channel)
            || (channel == "record" && !session.microphone)
        {
            return StatusCode::CONFLICT.into_response();
        }
        let expected_id = if channel == "main" {
            if !session.active.is_empty() || session.connection_id.is_some() {
                return StatusCode::CONFLICT.into_response();
            }
            0
        } else {
            if !session.active.contains("main") {
                return StatusCode::CONFLICT.into_response();
            }
            let Some(id) = session.connection_id else {
                return StatusCode::CONFLICT.into_response();
            };
            id
        };
        session.active.insert(channel.into());
        Some((
            AudioLease {
                group: group.clone(),
                channel: channel.into(),
                sessions: state.audio_sessions.clone(),
            },
            expected_id,
        ))
    } else {
        None
    };
    upgrade
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            let _lease = lease;
            if let Some((audio_lease, expected_id)) = audio {
                let _audio_lease = audio_lease;
                let _ = super::spice_audio::relay(
                    socket,
                    ticket,
                    expected_id,
                    state.audio_sessions.clone(),
                )
                .await;
            } else {
                let _ = relay(socket, ticket).await;
            }
        })
        .into_response()
}

trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}

async fn relay(socket: WebSocket, ticket: StreamTicket) -> std::io::Result<()> {
    let stream: Box<dyn Transport> = match ticket.endpoint {
        Endpoint::Unix(path) => Box::new(
            tokio::time::timeout(
                Duration::from_secs(3),
                tokio::net::UnixStream::connect(path),
            )
            .await??,
        ),
        Endpoint::Tcp(addr) => Box::new(
            tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(addr))
                .await??,
        ),
    };
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (mut sender, mut receiver) = socket.split();
    let to_browser = async {
        if matches!(ticket.kind.as_str(), "video" | "audio-dbus") {
            loop {
                let mut header = [0u8; 16];
                reader.read_exact(&mut header).await?;
                let length = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
                if header[2..4] != [0, 0] || length > MAX_VIDEO_RECORD || header[0] > 6 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid video record",
                    ));
                }
                let mut record = Vec::with_capacity(16 + length);
                record.extend_from_slice(&header);
                record.resize(16 + length, 0);
                reader.read_exact(&mut record[16..]).await?;
                if ticket.kind == "video" || matches!(header[0], 5 | 6) {
                    sender
                        .send(Message::Binary(record))
                        .await
                        .map_err(std::io::Error::other)?;
                }
            }
        } else if matches!(ticket.kind.as_str(), "lcm" | "frontpanel") {
            let mut reader = tokio::io::BufReader::new(reader);
            let mut line = Vec::new();
            loop {
                line.clear();
                let size = reader.read_until(b'\n', &mut line).await?;
                if size == 0 {
                    return Ok(());
                }
                if size > 64 * 1024 || !line.ends_with(b"\n") {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid panel record",
                    ));
                }
                let value: Value = serde_json::from_slice(&line).map_err(std::io::Error::other)?;
                let expected = if ticket.kind == "lcm" {
                    "unifi.lcm.v1"
                } else {
                    "unifi.frontpanel.v1"
                };
                if value.get("schema").and_then(Value::as_str) != Some(expected) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "unexpected panel record schema",
                    ));
                }
                sender
                    .send(Message::Text(
                        String::from_utf8(line.clone()).map_err(std::io::Error::other)?,
                    ))
                    .await
                    .map_err(std::io::Error::other)?;
            }
        } else {
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count = reader.read(&mut buffer).await?;
                if count == 0 {
                    return Ok(());
                }
                sender
                    .send(Message::Binary(buffer[..count].to_vec()))
                    .await
                    .map_err(std::io::Error::other)?;
            }
        }
    };
    let from_browser = async {
        while let Some(message) = receiver.next().await {
            match message.map_err(std::io::Error::other)? {
                Message::Binary(bytes)
                    if !matches!(
                        ticket.kind.as_str(),
                        "video" | "audio-dbus" | "lcm" | "frontpanel"
                    ) && bytes.len() <= MAX_MESSAGE =>
                {
                    writer.write_all(&bytes).await?
                }
                Message::Text(text) if ticket.kind == "video" && text.len() <= 64 * 1024 => {
                    let value: Value =
                        serde_json::from_str(&text).map_err(std::io::Error::other)?;
                    let command = value
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let allowed = command == "request_idr"
                        || (ticket.control
                            && matches!(
                                command,
                                "key_down"
                                    | "key_up"
                                    | "mouse_move"
                                    | "mouse_abs"
                                    | "mouse_down"
                                    | "mouse_up"
                                    | "mouse_wheel"
                                    | "resize"
                                    | "clipboard_set"
                                    | "clipboard_request"
                            ));
                    if !allowed {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "video control is not allowed",
                        ));
                    }
                    writer.write_all(text.as_bytes()).await?;
                    writer.write_all(b"\n").await?;
                }
                Message::Close(_) => return Ok(()),
                Message::Ping(_) | Message::Pong(_) => {}
                _ => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid stream message",
                    ));
                }
            }
        }
        Ok::<(), std::io::Error>(())
    };
    tokio::select! { result = to_browser => result, result = from_browser => result }
}
