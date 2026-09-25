use super::*;
use axum::extract::{
    Query,
    ws::{Message, WebSocket, WebSocketUpgrade},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::{
    collections::{BTreeSet, VecDeque},
    io::Read,
    os::fd::AsRawFd,
    os::unix::fs::FileTypeExt,
    os::unix::net::UnixStream as StdUnixStream,
    sync::atomic::{AtomicBool, Ordering},
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
    pub(super) takeover: bool,
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

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SpiceTicketRequest {
    #[serde(default)]
    microphone: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub(super) struct TicketRequest {
    #[serde(default)]
    control: bool,
    #[serde(default)]
    takeover: bool,
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

fn pending_serial_endpoint(root: &std::path::Path, id: &str) -> Endpoint {
    Endpoint::Unix(root.join("instances").join(id).join("sockets/serial.sock"))
}

async fn vnc_endpoint(
    state: &AppState,
    root: &std::path::Path,
    id: &str,
    run: &machineemu_core::domain::Run,
) -> Result<Endpoint, RuntimeError> {
    let gate = instance_lock(state, id)?;
    let _guard = gate.lock_owned().await;
    let mut qmp = supervisor::qmp(state, run).await?;
    let info = qmp.execute("query-vnc", Value::Null).await?;
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

async fn video_endpoint(
    state: &AppState,
    root: &std::path::Path,
    id: &str,
    run: &machineemu_core::domain::Run,
) -> Result<Endpoint, RuntimeError> {
    let lock = instance_lock(state, id)?;
    let _guard = lock.lock_owned().await;
    {
        let mut owners = state
            .supervisors
            .lock()
            .map_err(|_| RuntimeError::Process("supervisor lock poisoned".into()))?;
        let owner = owners
            .entry(id.into())
            .or_insert_with(|| supervisor::RunSupervisor::new(run.run_id.clone()));
        if owner.run_id != run.run_id {
            return Err(RuntimeError::Process(
                "display request belongs to a stale run".into(),
            ));
        }
        if let Some(process) = &mut owner.display {
            if process.try_wait()?.is_none() {
                return socket_endpoint(root, id, "video.sock");
            }
            owner.display = None;
        }
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
    let mut qmp = match supervisor::qmp(state, run).await {
        Ok(qmp) => qmp,
        Err(error) => {
            let _ = process.terminate();
            let _ = process.wait();
            return Err(error);
        }
    };
    if let Err(error) = qmp.attach_dbus_display(qemu_end.as_raw_fd()).await {
        let _ = process.terminate();
        let _ = process.wait();
        return Err(error);
    }
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
            tokio::time::sleep(Duration::from_millis(250)).await;
            if process.try_wait()?.is_some() {
                return Err(RuntimeError::Process(format!(
                    "display-stream could not attach to QEMU; inspect {}",
                    directory.join("video.log").display()
                )));
            }
            let mut owners = state
                .supervisors
                .lock()
                .map_err(|_| RuntimeError::Process("supervisor lock poisoned".into()))?;
            let owner = owners
                .get_mut(id)
                .filter(|owner| owner.run_id == run.run_id)
                .ok_or_else(|| RuntimeError::Process("display run was removed".into()))?;
            owner.display = Some(process);
            return Ok(endpoint);
        }
        if Instant::now() >= deadline {
            let _ = process.terminate();
            let _ = process.wait();
            return Err(RuntimeError::Process(
                "display-stream did not create video.sock".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
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
    let result = tokio::spawn({
        let state = state.clone();
        async move {
            let id_value = Id::new("instance", id.clone())?;
            if !matches!(
                kind.as_str(),
                "vnc" | "video" | "audio-dbus" | "usbredir" | "serial" | "lcm" | "frontpanel"
            ) {
                return Err(RuntimeError::Process("unknown stream kind".into()));
            }
            if kind == "usbredir" && !request.control {
                return Err(RuntimeError::Process(
                    "this stream requires control permission".into(),
                ));
            }
            if kind == "serial" && !request.control {
                return Err(RuntimeError::Process(
                    "serial streams require control permission".into(),
                ));
            }
            if request.takeover && (!request.control || !matches!(kind.as_str(), "vnc" | "video")) {
                return Err(RuntimeError::Process(
                    "takeover requires display control".into(),
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
            let run = workspace.live_run(&id_value)?;
            if kind != "serial" && run.is_none() {
                return Err(RuntimeError::Process("instance has no live run".into()));
            }
            if kind == "serial" {
                workspace.instance(&id_value)?;
            }
            let root = workspace.root().to_owned();
            let endpoint = match kind.as_str() {
                "vnc" => vnc_endpoint(&state, &root, &id, run.as_ref().unwrap()).await?,
                "video" | "audio-dbus" => {
                    video_endpoint(&state, &root, &id, run.as_ref().unwrap()).await?
                }
                "usbredir" => socket_endpoint(workspace.root(), &id, "usbredir.sock")?,
                "serial" => pending_serial_endpoint(workspace.root(), &id),
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
                    run_id: run
                        .as_ref()
                        .map(|run| run.run_id.as_str())
                        .unwrap_or("pending")
                        .into(),
                    kind: kind.clone(),
                    control: request.control,
                    takeover: request.takeover,
                    endpoint,
                    expires: Instant::now() + Duration::from_secs(30),
                    audio_group: None,
                },
            );
            Ok(serde_json::json!({"ticket": value, "expires_in_seconds": 30, "kind": kind}))
        }
    })
    .await
    .map_err(|error| RuntimeError::Process(format!("stream ticket task failed: {error}")))
    .and_then(|result| result);
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

async fn spice_endpoint(
    state: &AppState,
    root: &std::path::Path,
    id: &str,
    run: &machineemu_core::domain::Run,
) -> Result<Endpoint, RuntimeError> {
    let gate = instance_lock(state, id)?;
    let _guard = gate.lock_owned().await;
    let mut qmp = supervisor::qmp(state, run).await?;
    let info = qmp.execute("query-spice", Value::Null).await?;
    if info.get("enabled") != Some(&Value::Bool(true))
        || info.get("auth").and_then(Value::as_str) != Some("none")
    {
        return Err(RuntimeError::Process(
            "audio-only SPICE server is unavailable".into(),
        ));
    }
    let path = run.qmp_socket.with_file_name("spice.sock");
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
    let result = async move {
        let instance_id = Id::new("instance", id.clone())?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?
            .attach()?;
        let run = workspace
            .live_run(&instance_id)?
            .ok_or_else(|| RuntimeError::Process("instance has no live run".into()))?;
        let root = workspace.root().to_owned();
        let endpoint = spice_endpoint(&state, &root, &id, &run).await?;
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
                    takeover: false,
                    endpoint: endpoint.clone(),
                    expires: Instant::now() + Duration::from_secs(30),
                    audio_group: Some(group.clone()),
                },
            );
            tokens.insert(channel.into(), Value::String(token));
        }
        Ok(serde_json::json!({"client_token":group,"tickets":tokens,"expires_in_seconds":30}))
    }
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
    revoked: Arc<AtomicBool>,
    owners: Arc<Mutex<BTreeMap<String, Arc<AtomicBool>>>>,
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
        if let Ok(mut owners) = self.owners.lock()
            && owners
                .get(&self.key)
                .is_some_and(|current| Arc::ptr_eq(current, &self.revoked))
        {
            owners.remove(&self.key);
        }
    }
}

pub(super) fn revoke_display(state: &AppState, instance_id: &str, run_id: &str) {
    if let Ok(mut owners) = state.control_streams.lock()
        && let Some(flag) = owners.remove(&format!("{instance_id}:{run_id}:display"))
    {
        flag.store(true, Ordering::Release);
    }
}

fn claim_control(
    ticket: &StreamTicket,
    owners: Arc<Mutex<BTreeMap<String, Arc<AtomicBool>>>>,
) -> Result<Option<ControlLease>, StatusCode> {
    if !ticket.control {
        return Ok(None);
    }
    let group = if matches!(ticket.kind.as_str(), "vnc" | "video") {
        "display"
    } else {
        ticket.kind.as_str()
    };
    let key = if ticket.kind == "serial" {
        format!("{}:serial", ticket.instance_id)
    } else {
        format!("{}:{}:{group}", ticket.instance_id, ticket.run_id)
    };
    let revoked = Arc::new(AtomicBool::new(false));
    {
        let mut current = owners
            .lock()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if let Some(previous) = current.get(&key) {
            if !ticket.takeover {
                return Err(StatusCode::CONFLICT);
            }
            previous.store(true, Ordering::Release);
        }
        current.insert(key.clone(), revoked.clone());
    }
    Ok(Some(ControlLease {
        key,
        revoked,
        owners,
    }))
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
    let lease = {
        let state = state.clone();
        let id = id.clone();
        let ticket = ticket.clone();
        blocking(move || {
            let instance_id = Id::new("instance", id)?;
            let lock = instance_lock(&state, instance_id.as_str())?;
            let _guard = lock.blocking_lock();
            let live = {
                let workspace = state
                    .workspace
                    .lock()
                    .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
                workspace.live_run(&instance_id)?
            };
            if ticket.kind != "serial"
                && !matches!(live, Some(ref run) if run.run_id.as_str() == ticket.run_id)
            {
                return Err(RuntimeError::Process(
                    "stream ticket belongs to an inactive run".into(),
                ));
            }
            claim_control(&ticket, state.control_streams.clone()).map_err(|status| {
                RuntimeError::Process(format!("display input claim rejected: {status}"))
            })
        })
        .await
    };
    let lease = match lease {
        Ok(lease) => lease,
        Err(_) => return StatusCode::CONFLICT.into_response(),
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
            let revoked = lease.as_ref().map(|lease| lease.revoked.clone());
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
                let _ = relay(socket, ticket, revoked).await;
            }
        })
        .into_response()
}

trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}

async fn connect_pending_serial(path: &std::path::Path) -> std::io::Result<tokio::net::UnixStream> {
    loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

struct RfbInputGate {
    handshake: VecDeque<usize>,
    security_selected: bool,
    buffer: Vec<u8>,
}

impl Default for RfbInputGate {
    fn default() -> Self {
        Self {
            handshake: VecDeque::from([12, 1, 1]),
            security_selected: false,
            buffer: Vec::new(),
        }
    }
}

impl RfbInputGate {
    fn feed(&mut self, data: &[u8], allow_input: bool) -> std::io::Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut offset = 0;
        while let Some(needed) = self.handshake.front_mut() {
            if offset == data.len() {
                break;
            }
            let take = (*needed).min(data.len() - offset);
            output.extend_from_slice(&data[offset..offset + take]);
            offset += take;
            *needed -= take;
            if *needed == 0 {
                let security_selection = self.handshake.len() == 2 && !self.security_selected;
                self.handshake.pop_front();
                if security_selection {
                    self.security_selected = true;
                    match data[offset - 1] {
                        1 => {}
                        2 => self.handshake.push_front(16),
                        _ => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "unsupported view-only RFB security type",
                            ));
                        }
                    }
                }
            }
        }
        self.buffer.extend_from_slice(&data[offset..]);
        if !self.handshake.is_empty() {
            return Ok(output);
        }
        let mut consumed = 0;
        while consumed < self.buffer.len() {
            let kind = self.buffer[consumed];
            let Some(size) = Self::message_size(&self.buffer[consumed..])? else {
                break;
            };
            if size > self.buffer.len() - consumed {
                break;
            }
            if allow_input || !matches!(kind, 4 | 5 | 6 | 251 | 255) {
                output.extend_from_slice(&self.buffer[consumed..consumed + size]);
            }
            consumed += size;
        }
        self.buffer.drain(..consumed);
        if self.buffer.len() > MAX_MESSAGE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "RFB message too large",
            ));
        }
        Ok(output)
    }

    fn message_size(data: &[u8]) -> std::io::Result<Option<usize>> {
        let size = match data[0] {
            0 => 20,
            2 if data.len() >= 4 => 4 + 4 * u16::from_be_bytes([data[2], data[3]]) as usize,
            2 => return Ok(None),
            3 | 150 => 10,
            4 => 8,
            5 if data.len() >= 2 => {
                if data[1] & 0x80 != 0 {
                    7
                } else {
                    6
                }
            }
            5 => return Ok(None),
            6 if data.len() >= 8 => {
                8 + i32::from_be_bytes(data[4..8].try_into().unwrap()).unsigned_abs() as usize
            }
            6 => return Ok(None),
            248 if data.len() >= 9 => 9 + data[8] as usize,
            248 => return Ok(None),
            251 if data.len() >= 8 => 8 + 16 * data[6] as usize,
            251 => return Ok(None),
            255 => 12,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "unsupported RFB message",
                ));
            }
        };
        if size > MAX_MESSAGE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "RFB message too large",
            ));
        }
        Ok(Some(size))
    }
}

async fn relay(
    socket: WebSocket,
    ticket: StreamTicket,
    revoked: Option<Arc<AtomicBool>>,
) -> std::io::Result<()> {
    let stream: Box<dyn Transport> = match ticket.endpoint {
        Endpoint::Unix(path) if ticket.kind == "serial" => {
            // The ticket and WebSocket may predate VM start. Keep retrying the
            // daemon-local UART until QEMU publishes it.
            Box::new(connect_pending_serial(&path).await?)
        }
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
    let revoked_for_input = revoked.clone();
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
        let mut gate = RfbInputGate::default();
        while let Some(message) = receiver.next().await {
            if revoked_for_input
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "display input ownership revoked",
                ));
            }
            match message.map_err(std::io::Error::other)? {
                Message::Binary(bytes)
                    if !matches!(
                        ticket.kind.as_str(),
                        "video" | "audio-dbus" | "lcm" | "frontpanel"
                    ) && bytes.len() <= MAX_MESSAGE =>
                {
                    if ticket.kind == "vnc" {
                        if ticket.control {
                            writer.write_all(&bytes).await?;
                        } else {
                            let allowed = gate.feed(&bytes, false)?;
                            if !allowed.is_empty() {
                                writer.write_all(&allowed).await?;
                            }
                        }
                    } else {
                        writer.write_all(&bytes).await?;
                    }
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
    let revoked_wait = async {
        if let Some(flag) = revoked {
            while !flag.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        } else {
            std::future::pending::<()>().await;
        }
        Ok(())
    };
    tokio::select! { result = to_browser => result, result = from_browser => result, result = revoked_wait => result }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pending_serial_connects_when_socket_appears() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("serial.sock");
        let connecting = tokio::spawn({
            let path = path.clone();
            async move { connect_pending_serial(&path).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (client, accepted) = tokio::join!(connecting, listener.accept());
        client.unwrap().unwrap();
        accepted.unwrap();
    }

    #[test]
    fn view_only_rfb_filters_fragmented_keyboard_mouse_and_clipboard() {
        let mut gate = RfbInputGate::default();
        assert_eq!(
            gate.feed(b"RFB 003.008\n", false).unwrap(),
            b"RFB 003.008\n"
        );
        assert_eq!(gate.feed(&[1, 1], false).unwrap(), [1, 1]);
        let keyboard = [4, 1, 0, 0, 0, 0, 0, 0];
        assert!(gate.feed(&keyboard[..3], false).unwrap().is_empty());
        assert!(gate.feed(&keyboard[3..], false).unwrap().is_empty());
        assert!(gate.feed(&[5, 1, 0, 0, 0, 0], false).unwrap().is_empty());
        assert!(
            gate.feed(&[6, 0, 0, 0, 0, 0, 0, 1, b'x'], false)
                .unwrap()
                .is_empty()
        );
        let request = [3, 0, 0, 0, 0, 0, 0, 0, 1, 1];
        assert_eq!(gate.feed(&request, false).unwrap(), request);
    }

    #[test]
    fn controlling_rfb_passes_input_and_rejects_oversized_frames() {
        let mut gate = RfbInputGate::default();
        gate.feed(b"RFB 003.008\n\x01\x01", true).unwrap();
        let keyboard = [4, 1, 0, 0, 0, 0, 0, 0];
        assert_eq!(gate.feed(&keyboard, true).unwrap(), keyboard);
        assert!(
            gate.feed(&[6, 0, 0, 0, 0x7f, 0xff, 0xff, 0xff], true)
                .is_err()
        );
    }

    #[test]
    fn view_only_rfb_passes_vnc_auth_response_before_filtering_input() {
        let mut gate = RfbInputGate::default();
        let mut handshake = b"RFB 003.008\n\x02".to_vec();
        handshake.extend_from_slice(&[0x55; 16]);
        handshake.push(1);
        assert_eq!(gate.feed(&handshake, false).unwrap(), handshake);
        assert!(
            gate.feed(&[4, 1, 0, 0, 0, 0, 0, 0], false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn vnc_and_video_share_a_revocable_run_input_owner() {
        let owners = Arc::new(Mutex::new(BTreeMap::new()));
        let ticket = |kind: &str, control: bool, takeover: bool| StreamTicket {
            instance_id: "lab01".into(),
            run_id: "run01".into(),
            kind: kind.into(),
            control,
            takeover,
            endpoint: Endpoint::Unix(PathBuf::from("unused")),
            expires: Instant::now() + Duration::from_secs(30),
            audio_group: None,
        };
        let old = claim_control(&ticket("vnc", true, false), owners.clone())
            .unwrap()
            .unwrap();
        assert!(matches!(
            claim_control(&ticket("video", true, false), owners.clone()),
            Err(StatusCode::CONFLICT)
        ));
        assert!(
            claim_control(&ticket("video", false, false), owners.clone())
                .unwrap()
                .is_none()
        );
        let new = claim_control(&ticket("video", true, true), owners.clone())
            .unwrap()
            .unwrap();
        assert!(old.revoked.load(Ordering::Acquire));

        let serial = claim_control(&ticket("serial", true, false), owners.clone())
            .unwrap()
            .unwrap();
        assert!(matches!(
            claim_control(&ticket("serial", true, false), owners.clone()),
            Err(StatusCode::CONFLICT)
        ));
        drop(serial);
        let replacement = claim_control(&ticket("serial", true, false), owners.clone())
            .unwrap()
            .unwrap();
        drop(replacement);
        drop(old);
        assert!(owners.lock().unwrap().contains_key("lab01:run01:display"));
        drop(new);
        assert!(!owners.lock().unwrap().contains_key("lab01:run01:display"));
    }
}
