use super::*;
use axum::{
    Json,
    http::header,
    response::{
        Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::stream;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;
use utoipa::ToSchema;

const MAX_JOBS: usize = 32;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const CHUNK_BYTES: usize = 12 * 1024;
static NEXT_EXECUTION: AtomicU64 = AtomicU64::new(1);

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct StartExecution {
    command: String,
    #[serde(default = "default_shell")]
    shell: String,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
}

fn default_shell() -> String {
    "auto".into()
}
fn default_timeout() -> u64 {
    120
}

#[derive(Serialize, ToSchema)]
pub(super) struct ExecutionAccepted {
    execution_id: String,
    events_url: String,
}

#[derive(Clone)]
struct JobState {
    sequence: u64,
    phase: &'static str,
    guest_pid: Option<i64>,
    exit_code: Option<i64>,
    signal: Option<i64>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
    error_code: Option<&'static str>,
    finished_at: Option<Instant>,
}

impl JobState {
    fn queued() -> Self {
        Self {
            sequence: 1,
            phase: "queued",
            guest_pid: None,
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            error_code: None,
            finished_at: None,
        }
    }
    fn terminal(&self) -> bool {
        self.finished_at.is_some()
    }
}

pub(super) struct ExecutionJob {
    execution_id: String,
    instance_id: String,
    run_id: String,
    sender: watch::Sender<JobState>,
}

fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(ErrorBody { error: code.into() })).into_response()
}

pub(super) async fn start_execution(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<StartExecution>,
) -> Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let Ok(instance_id) = Id::new("instance", id.clone()) else {
        return error(StatusCode::BAD_REQUEST, "invalid_instance_id");
    };
    if request.command.is_empty()
        || request.command.len() > 4096
        || request.command.contains('\0')
        || !matches!(request.shell.as_str(), "auto" | "sh" | "powershell")
        || !(1..=600).contains(&request.timeout_seconds)
    {
        return error(StatusCode::BAD_REQUEST, "invalid_execution_request");
    }
    let lookup_state = state.clone();
    let lookup_id = instance_id.clone();
    let run = blocking(move || {
        let lock = instance_lock(&lookup_state, lookup_id.as_str())?;
        let _guard = lock.blocking_lock();
        let workspace = lookup_state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.instance(&lookup_id)?;
        workspace.live_run(&lookup_id)
    })
    .await;
    let run = match run {
        Ok(Some(run)) => run,
        Ok(None) => return error(StatusCode::CONFLICT, "instance_not_running"),
        Err(RuntimeError::NotFound { .. }) => {
            return error(StatusCode::NOT_FOUND, "instance_not_found");
        }
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "execution_unavailable"),
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let execution_id = format!(
        "ge-{now:x}-{:x}",
        NEXT_EXECUTION.fetch_add(1, Ordering::Relaxed)
    );
    let (sender, _) = watch::channel(JobState::queued());
    let job = Arc::new(ExecutionJob {
        execution_id: execution_id.clone(),
        instance_id: id.clone(),
        run_id: run.run_id.as_str().into(),
        sender,
    });
    {
        let Ok(mut jobs) = state.guest_executions.lock() else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "execution_unavailable");
        };
        jobs.retain(|_, job| {
            job.sender
                .borrow()
                .finished_at
                .is_none_or(|finished| finished.elapsed() <= Duration::from_secs(600))
        });
        if jobs.len() >= MAX_JOBS {
            return error(StatusCode::TOO_MANY_REQUESTS, "too_many_executions");
        }
        jobs.insert(execution_id.clone(), job.clone());
    }
    let worker_state = state.clone();
    tokio::spawn(async move {
        run_execution(worker_state, instance_id, request, job).await;
    });
    (
        StatusCode::ACCEPTED,
        Json(ExecutionAccepted {
            events_url: format!("/api/v2/guest-executions/{execution_id}/events"),
            execution_id,
        }),
    )
        .into_response()
}

async fn run_execution(
    state: AppState,
    instance_id: Id,
    request: StartExecution,
    job: Arc<ExecutionJob>,
) {
    let result = run_execution_inner(&state, &instance_id, &request, &job).await;
    if let Err(code) = result {
        job.sender.send_modify(|current| {
            current.sequence += 1;
            current.phase = "failed";
            current.error_code = Some(code);
            current.finished_at = Some(Instant::now());
        });
    }
}

async fn run_execution_inner(
    state: &AppState,
    instance_id: &Id,
    request: &StartExecution,
    job: &ExecutionJob,
) -> Result<(), &'static str> {
    let gate = instance_lock(state, instance_id.as_str())
        .map_err(|_| "execution_unavailable")?
        .lock_owned()
        .await;
    let lookup_state = state.clone();
    let lookup_id = instance_id.clone();
    let (run, socket) = blocking(move || {
        let workspace = lookup_state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let run = workspace.live_run(&lookup_id)?;
        let socket = workspace
            .root()
            .join("instances")
            .join(lookup_id.as_str())
            .join("qga.sock");
        Ok((run, socket))
    })
    .await
    .map_err(|_| "execution_unavailable")?;
    if run.as_ref().map(|run| run.run_id.as_str()) != Some(job.run_id.as_str()) {
        return Err("run_changed");
    }
    let mut client = guest_agent::Client::connect(&socket)
        .await
        .map_err(|_| "agent_unavailable")?;
    let shell = if request.shell == "auto" {
        match client.command("guest-get-osinfo").await.ok().flatten() {
            Some(os) if os.get("id").and_then(Value::as_str) == Some("mswindows") => "powershell",
            _ => "sh",
        }
    } else {
        request.shell.as_str()
    };
    let (path, args) = if shell == "powershell" {
        (
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            json!(["-NoProfile", "-NonInteractive", "-Command", request.command]),
        )
    } else {
        ("/bin/sh", json!(["-c", request.command]))
    };
    let launched = client
        .command_args(
            "guest-exec",
            json!({"path":path,"arg":args,"capture-output":true}),
        )
        .await
        .map_err(|_| "agent_unavailable")?
        .ok_or("guest_exec_failed")?;
    let pid = launched
        .get("pid")
        .and_then(Value::as_i64)
        .ok_or("invalid_agent_response")?;
    drop(client);
    drop(gate);
    job.sender.send_modify(|current| {
        current.sequence += 1;
        current.phase = "running";
        current.guest_pid = Some(pid);
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(request.timeout_seconds);
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("execution_timeout");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        let status = {
            let _gate = instance_lock(state, instance_id.as_str())
                .map_err(|_| "execution_unavailable")?
                .lock_owned()
                .await;
            let check_state = state.clone();
            let check_id = instance_id.clone();
            let active = blocking(move || {
                let workspace = check_state
                    .workspace
                    .lock()
                    .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
                workspace.live_run(&check_id)
            })
            .await
            .map_err(|_| "execution_unavailable")?;
            if active.as_ref().map(|run| run.run_id.as_str()) != Some(job.run_id.as_str()) {
                return Err("run_changed");
            }
            let mut client = guest_agent::Client::connect(&socket)
                .await
                .map_err(|_| "agent_unavailable")?;
            client
                .command_args("guest-exec-status", json!({"pid":pid}))
                .await
                .map_err(|_| "agent_unavailable")?
                .ok_or("guest_exec_status_failed")?
        };
        if status.get("exited").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let stdout = decode_output(&status, "out-data")?;
        let stderr = decode_output(&status, "err-data")?;
        job.sender.send_modify(|current| {
            current.sequence += 1;
            current.phase = "completed";
            current.exit_code = status.get("exitcode").and_then(Value::as_i64);
            current.signal = status.get("signal").and_then(Value::as_i64);
            current.stdout_truncated =
                stdout.1 || status.get("out-truncated").and_then(Value::as_bool) == Some(true);
            current.stderr_truncated =
                stderr.1 || status.get("err-truncated").and_then(Value::as_bool) == Some(true);
            current.stdout = stdout.0;
            current.stderr = stderr.0;
            current.finished_at = Some(Instant::now());
        });
        return Ok(());
    }
}

fn decode_output(status: &Value, field: &str) -> Result<(Vec<u8>, bool), &'static str> {
    let Some(encoded) = status.get(field).and_then(Value::as_str) else {
        return Ok((Vec::new(), false));
    };
    let mut decoded = STANDARD
        .decode(encoded)
        .map_err(|_| "invalid_agent_response")?;
    let truncated = decoded.len() > MAX_OUTPUT_BYTES;
    decoded.truncate(MAX_OUTPUT_BYTES);
    Ok((decoded, truncated))
}

pub(super) async fn stream_execution(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let job = match state.guest_executions.lock() {
        Ok(jobs) => jobs.get(&id).cloned(),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "execution_unavailable"),
    };
    let Some(job) = job else {
        return error(StatusCode::NOT_FOUND, "execution_not_found");
    };
    let receiver = job.sender.subscribe();
    let output = stream::unfold(
        (receiver, job, VecDeque::new(), false, false),
        |(mut receiver, job, mut pending, mut started, mut finished)| async move {
            loop {
                if let Some(event) = pending.pop_front() {
                    return Some((
                        Ok::<Event, std::convert::Infallible>(event),
                        (receiver, job, pending, started, finished),
                    ));
                }
                if finished {
                    return None;
                }
                let state = if started {
                    receiver.changed().await.ok()?;
                    receiver.borrow_and_update().clone()
                } else {
                    started = true;
                    receiver.borrow_and_update().clone()
                };
                pending = frames(&job, &state);
                finished = state.terminal();
            }
        },
    );
    let mut response = Sse::new(output)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-cache, no-transform".parse().unwrap(),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().unwrap());
    response
}

fn frames(job: &ExecutionJob, state: &JobState) -> VecDeque<Event> {
    let mut result = VecDeque::new();
    let status = json!({"execution_id":job.execution_id,"instance_id":job.instance_id,"run_id":job.run_id,"state":state.phase,"guest_pid":state.guest_pid,"error_code":state.error_code});
    result.push_back(
        Event::default()
            .event("status")
            .id(format!("{}:{}", job.execution_id, state.sequence))
            .data(status.to_string()),
    );
    if state.terminal() {
        for (channel, bytes) in [("stdout", &state.stdout), ("stderr", &state.stderr)] {
            for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
                result.push_back(Event::default().event("output").data(json!({"execution_id":job.execution_id,"channel":channel,"chunk_index":index,"encoding":"base64","data":STANDARD.encode(chunk)}).to_string()));
            }
        }
        result.push_back(Event::default().event("complete").data(json!({"execution_id":job.execution_id,"state":state.phase,"exit_code":state.exit_code,"signal":state.signal,"stdout_truncated":state.stdout_truncated,"stderr_truncated":state.stderr_truncated,"error_code":state.error_code}).to_string()));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt},
        net::UnixListener,
    };

    #[test]
    fn output_frames_are_bounded_and_terminal_state_is_explicit() {
        let (sender, _) = watch::channel(JobState::queued());
        let job = ExecutionJob {
            execution_id: "ge-test".into(),
            instance_id: "lab01".into(),
            run_id: "run01".into(),
            sender,
        };
        let mut state = JobState::queued();
        state.phase = "completed";
        state.stdout = vec![b'a'; CHUNK_BYTES + 1];
        state.exit_code = Some(7);
        state.finished_at = Some(Instant::now());
        let frames = frames(&job, &state);
        assert_eq!(frames.len(), 4);
        assert!(
            decode_output(&json!({"out-data":STANDARD.encode(b"hello")}), "out-data")
                .unwrap()
                .0
                == b"hello"
        );
    }

    #[tokio::test]
    async fn guest_exec_and_status_use_the_agent_protocol() {
        let root =
            std::env::temp_dir().join(format!("machineemu-guest-exec-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("qga.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            for expected in ["guest-exec", "guest-exec-status"] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = tokio::io::BufReader::new(stream);
                let mut line = Vec::new();
                stream.read_until(b'\n', &mut line).await.unwrap();
                let sync: Value =
                    serde_json::from_slice(line.strip_prefix(&[0xff]).unwrap()).unwrap();
                stream
                    .get_mut()
                    .write_all(
                        format!("{}\n", json!({"return":sync["arguments"]["id"]})).as_bytes(),
                    )
                    .await
                    .unwrap();
                line.clear();
                stream.read_until(b'\n', &mut line).await.unwrap();
                let command: Value = serde_json::from_slice(&line).unwrap();
                assert_eq!(command["execute"], expected);
                let reply = if expected == "guest-exec" {
                    assert_eq!(command["arguments"]["path"], "/bin/sh");
                    assert_eq!(command["arguments"]["capture-output"], true);
                    json!({"pid":42})
                } else {
                    assert_eq!(command["arguments"]["pid"], 42);
                    json!({"exited":true,"exitcode":0,"out-data":STANDARD.encode(b"done\n")})
                };
                stream
                    .get_mut()
                    .write_all(format!("{}\n", json!({"return":reply})).as_bytes())
                    .await
                    .unwrap();
            }
        });
        let mut launch = guest_agent::Client::connect(&socket).await.unwrap();
        let result = launch
            .command_args(
                "guest-exec",
                json!({"path":"/bin/sh","arg":["-c","echo done"],"capture-output":true}),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result["pid"], 42);
        drop(launch);
        let mut poll = guest_agent::Client::connect(&socket).await.unwrap();
        let result = poll
            .command_args("guest-exec-status", json!({"pid":42}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decode_output(&result, "out-data").unwrap().0, b"done\n");
        server.await.unwrap();
        std::fs::remove_file(&socket).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }
}
