use super::*;
use axum::Json;
use serde_json::{Value, json};
use std::{
    io::{BufRead, Read, Write},
    os::unix::{
        fs::FileTypeExt,
        net::{UnixDatagram, UnixStream},
    },
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

struct ReplySocket(PathBuf);
impl Drop for ReplySocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(super) fn request(
    state: &AppState,
    id: &str,
    kind: &str,
    payload: Value,
) -> Result<Value, RuntimeError> {
    let instance = Id::new("instance", id.to_owned())?;
    let workspace = state
        .workspace
        .lock()
        .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?
        .attach()?;
    if workspace.live_run(&instance)?.is_none() {
        return Err(RuntimeError::Process("instance has no live run".into()));
    }
    let name = match kind {
        "bluetooth" => "bluetooth-control.sock",
        "wifi" => "wifi-control.sock",
        "lcm" => "display-input.sock",
        _ => return Err(RuntimeError::Process("unknown helper".into())),
    };
    let path = workspace.root().join("instances").join(id).join(name);
    let metadata = fs::symlink_metadata(&path).map_err(|source| RuntimeError::Io {
        path: path.clone(),
        source,
    })?;
    if !metadata.file_type().is_socket() {
        return Err(RuntimeError::Process(
            "helper endpoint is not a Unix socket".into(),
        ));
    }
    let bytes =
        serde_json::to_vec(&payload).map_err(|error| RuntimeError::Process(error.to_string()))?;
    if bytes.len() > 16 * 1024 {
        return Err(RuntimeError::Process(
            "helper request exceeds 16 KiB".into(),
        ));
    }
    let mut response = vec![0u8; 65_536];
    let size = if kind == "lcm" {
        let mut socket = UnixStream::connect(&path).map_err(|source| RuntimeError::Io {
            path: path.clone(),
            source,
        })?;
        socket
            .set_read_timeout(Some(Duration::from_secs(6)))
            .map_err(|error| RuntimeError::Process(error.to_string()))?;
        socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|error| RuntimeError::Process(error.to_string()))?;
        socket
            .write_all(&bytes)
            .and_then(|_| socket.write_all(b"\n"))
            .map_err(|error| RuntimeError::Process(error.to_string()))?;
        let mut line = Vec::new();
        let mut reader = std::io::BufReader::new(socket.take(65_537));
        reader
            .read_until(b'\n', &mut line)
            .map_err(|error| RuntimeError::Process(error.to_string()))?;
        if line.len() > 65_536 || !line.ends_with(b"\n") {
            return Err(RuntimeError::Process("invalid LCM response".into()));
        }
        return serde_json::from_slice(&line)
            .map_err(|error| RuntimeError::Process(error.to_string()));
    } else {
        let reply_path = std::env::temp_dir().join(format!(
            "machineemu-ctl-{}-{}.sock",
            std::process::id(),
            REQUEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let socket = UnixDatagram::bind(&reply_path).map_err(|source| RuntimeError::Io {
            path: reply_path.clone(),
            source,
        })?;
        let _cleanup = ReplySocket(reply_path.clone());
        #[cfg(unix)]
        fs::set_permissions(
            &reply_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .map_err(|source| RuntimeError::Io {
            path: reply_path,
            source,
        })?;
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|error| RuntimeError::Process(error.to_string()))?;
        socket
            .send_to(&bytes, &path)
            .map_err(|error| RuntimeError::Process(error.to_string()))?;
        socket
            .recv(&mut response)
            .map_err(|error| RuntimeError::Process(error.to_string()))?
    };
    serde_json::from_slice(&response[..size])
        .map_err(|error| RuntimeError::Process(error.to_string()))
}

pub(super) async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, kind)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    if !matches!(kind.as_str(), "bluetooth" | "wifi") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let result =
        blocking(move || request(&state, &id, &kind, json!({"version":1,"type":"stats"}))).await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn action(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, kind)): Path<(String, String)>,
    Json(mut payload): Json<Value>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let lcm_action = payload.as_object().is_some_and(|object| {
        object.len() == 1
            && match object.iter().next().unwrap() {
                (key, Value::String(value)) if key == "screen" => {
                    !value.is_empty() && value.len() <= 64
                }
                (key, Value::String(value)) if key == "dismiss" => {
                    !value.is_empty()
                        && value.len() <= 64
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_'))
                }
                (key, Value::Bool(_)) if key == "screensaver" => true,
                (key, Value::Number(value)) if key == "port" => {
                    value.as_u64().is_some_and(|port| (1..=26).contains(&port))
                }
                _ => false,
            }
    });
    let allowed = match (kind.as_str(), payload.get("type").and_then(Value::as_str)) {
        ("bluetooth", Some("configure" | "advertise")) | ("wifi", Some("configure")) => true,
        ("lcm", _) => lcm_action,
        _ => false,
    };
    if !allowed || !payload.is_object() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if kind != "lcm" {
        payload["version"] = json!(1);
        payload.as_object_mut().unwrap().remove("instance");
    }
    let result = blocking(move || request(&state, &id, &kind, payload)).await;
    match result {
        Ok(value) if value["type"] == "error" || value["ok"] == false => {
            (StatusCode::CONFLICT, Json(value)).into_response()
        }
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}
