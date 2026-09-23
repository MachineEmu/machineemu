use super::*;
pub(super) fn ensure_daemon(
    endpoint: &str,
    token: &str,
    workspace: &Path,
) -> Result<(), machineemu_core::engine::Error> {
    if daemon_request(endpoint, token, "GET", "/api/v2/health", None).is_ok() {
        return Ok(());
    }
    let launch_plans = workspace.join("staging/launch-plans.json");
    fs::write(&launch_plans, "{}")
        .map_err(|error| machineemu_core::engine::Error::Invalid(error.to_string()))?;
    let executable = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join("machineemu-daemon")))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("machineemu-daemon"));
    #[cfg(unix)]
    let mut command = {
        let mut command = ProcessCommand::new("setsid");
        command.arg("-f").arg(&executable);
        command
    };
    #[cfg(not(unix))]
    let mut command = ProcessCommand::new(&executable);
    let mut args = vec![
        "--workspace".to_owned(),
        workspace.to_string_lossy().into_owned(),
    ];
    if let Some(socket) = endpoint.strip_prefix("unix:") {
        args.extend(["--unix-socket".into(), socket.into()]);
    } else {
        args.extend(["--listen".into(), endpoint.into()]);
        args.extend(["--bearer-token".into(), token.into()]);
    }
    args.extend([
        "--launch-plans".into(),
        launch_plans.to_string_lossy().into_owned(),
    ]);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| {
            machineemu_core::engine::Error::Invalid(format!("cannot start daemon: {error}"))
        })?;
    for _ in 0..50 {
        thread::sleep(Duration::from_millis(100));
        if daemon_request(endpoint, token, "GET", "/api/v2/health", None).is_ok() {
            return Ok(());
        }
    }
    Err(machineemu_core::engine::Error::Invalid(
        "daemon did not become ready".into(),
    ))
}

pub(super) fn effective_client(
    endpoint: &str,
    token: &str,
) -> Result<(String, String), machineemu_core::engine::Error> {
    if endpoint != "127.0.0.1:8787" || token != "machineemu-dev-token" {
        return Ok((endpoint.to_owned(), token.to_owned()));
    }
    let (config, config_path) = machineemu_core::config::load_config(None)
        .map_err(|error| machineemu_core::engine::Error::Runtime(error.to_string()))?;
    let Some(client) = config.client else {
        return Ok((endpoint.to_owned(), token.to_owned()));
    };
    let endpoint = client
        .unix_socket
        .map(|path| {
            format!(
                "unix:{}",
                machineemu_core::config::resolve_config_path(config_path.as_deref(), path)
                    .display()
            )
        })
        .or(client.endpoint)
        .unwrap_or_else(|| endpoint.to_owned());
    Ok((endpoint, client.token.unwrap_or_else(|| token.to_owned())))
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

fn connect_daemon(endpoint: &str) -> Result<Box<dyn ReadWrite>, machineemu_core::engine::Error> {
    if let Some(path) = endpoint.strip_prefix("unix:") {
        #[cfg(unix)]
        return UnixStream::connect(path)
            .map(|stream| Box::new(stream) as Box<dyn ReadWrite>)
            .map_err(|error| {
                machineemu_core::engine::Error::Invalid(format!(
                    "cannot connect to daemon: {error}"
                ))
            });
        #[cfg(not(unix))]
        return Err(machineemu_core::engine::Error::Invalid(
            "Unix daemon sockets are unavailable on this platform".into(),
        ));
    }
    TcpStream::connect(endpoint)
        .map(|stream| Box::new(stream) as Box<dyn ReadWrite>)
        .map_err(|error| {
            machineemu_core::engine::Error::Invalid(format!("cannot connect to daemon: {error}"))
        })
}

pub(super) fn daemon_request(
    endpoint: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, machineemu_core::engine::Error> {
    let (endpoint, token) = effective_client(endpoint, token)?;
    let mut stream = connect_daemon(&endpoint)?;
    let bytes = body
        .map(|value| serde_json::to_vec(&value).expect("JSON value serializes"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {endpoint}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        bytes.len()
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.write_all(&bytes))
        .map_err(|error| {
            machineemu_core::engine::Error::Invalid(format!("cannot write daemon request: {error}"))
        })?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|error| {
        machineemu_core::engine::Error::Invalid(format!("cannot read daemon response: {error}"))
    })?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| {
            machineemu_core::engine::Error::Invalid("malformed daemon response".into())
        })?;
    let header = String::from_utf8_lossy(&response[..split]);
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(599);
    let payload = &response[split + 4..];
    let value = if payload.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(payload)
            .unwrap_or_else(|_| serde_json::json!({"raw": String::from_utf8_lossy(payload)}))
    };
    if !(200..300).contains(&status) {
        return Err(machineemu_core::engine::Error::Invalid(format!(
            "daemon returned HTTP {status}: {value}"
        )));
    }
    Ok(value)
}
