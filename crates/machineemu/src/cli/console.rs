use super::*;
use machineemu_core::engine::Error;
use std::io::IsTerminal;
use std::process::Stdio;

fn error(value: impl std::fmt::Display) -> Error {
    Error::Runtime(value.to_string())
}

pub(super) fn instance_dir(instance: &str, workspace: Option<&Path>) -> Result<PathBuf, Error> {
    Id::new("instance", instance.to_owned()).map_err(error)?;
    let root = if let Some(workspace) = workspace {
        workspace.to_owned()
    } else {
        let (config, config_path) = machineemu_core::config::load_config(None).map_err(error)?;
        let configured = config
            .client
            .and_then(|c| c.workspace)
            .or_else(|| config.server.and_then(|s| s.workspace));
        configured
            .map(|path| machineemu_core::config::resolve_config_path(config_path.as_deref(), path))
            .unwrap_or_else(|| PathBuf::from("machineemu-workspace"))
    };
    let path = root.join("instances").join(instance);
    if !path.is_dir() {
        return Err(error(format!(
            "no local instance {instance:?} at {}; use --workspace for its host workspace",
            path.display()
        )));
    }
    Ok(path)
}

#[cfg(test)]
fn log_path(directory: &Path, source: &str) -> Result<PathBuf, Error> {
    let names: &[&str] = match source {
        "serial" => &["serial.log"],
        "stderr" => &["qemu.stderr"],
        "stdout" => &["qemu.stdout"],
        _ => &["serial.log", "qemu.stderr", "qemu.stdout"],
    };
    names
        .iter()
        .map(|name| directory.join(name))
        .find(|path| path.metadata().is_ok_and(|m| m.is_file() && m.len() > 0))
        .or_else(|| {
            names
                .iter()
                .map(|name| directory.join(name))
                .find(|path| path.is_file())
        })
        .ok_or_else(|| {
            error(format!(
                "no {source} log exists yet in {}",
                directory.display()
            ))
        })
}

pub(super) async fn logs(
    instance: &str,
    daemon: &str,
    token: &str,
    follow: bool,
    lines: usize,
    source: &str,
) -> Result<(), Error> {
    let mut previous = String::new();
    loop {
        let path = format!("/api/v2/instances/{instance}/logs?source={source}&lines={lines}");
        let value = super::daemon_request(daemon, token, "GET", &path, None).await?;
        let content = value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let addition = content.strip_prefix(&previous).unwrap_or(content);
        print!("{addition}");
        std::io::Write::flush(&mut std::io::stdout()).map_err(error)?;
        if !follow {
            return Ok(());
        }
        previous = content.to_owned();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

pub(super) async fn serial(instance: &str, daemon: &str, token: &str) -> Result<(), Error> {
    use futures_util::{SinkExt, StreamExt};
    use std::io::{Read, Write};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

    Id::new("instance", instance.to_owned()).map_err(error)?;
    let ticket = super::daemon_request(
        daemon,
        token,
        "POST",
        &format!("/api/v2/instances/{instance}/streams/serial/ticket"),
        Some(serde_json::json!({"control":true,"takeover":false})),
    )
    .await?;
    let ticket = ticket
        .get("ticket")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| error("daemon returned no serial stream ticket"))?;
    let (endpoint, _) = super::effective_client(daemon, token)?;
    let stream = super::client::connect_daemon(&endpoint).await?;
    let url = format!("ws://machineemu/ws/v2/instances/{instance}/serial?ticket={ticket}");
    let mut request = url.into_client_request().map_err(error)?;
    request
        .headers_mut()
        .insert("origin", "http://machineemu".parse().unwrap());
    let (websocket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(error)?;
    let (mut sender, mut receiver) = websocket.split();
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    tokio::task::spawn_blocking(move || {
        let mut input = std::io::stdin().lock();
        let mut bytes = [0; 4096];
        while let Ok(count) = input.read(&mut bytes) {
            if count == 0 {
                break;
            }
            let end = bytes[..count]
                .iter()
                .position(|byte| *byte == 0x1d)
                .unwrap_or(count);
            if end > 0 && input_tx.blocking_send(bytes[..end].to_vec()).is_err() {
                break;
            }
            if end != count {
                break;
            }
        }
    });
    let _terminal = Terminal::raw()?;
    eprintln!("Attached to {instance}. Ctrl-] detaches; Ctrl-C is sent to the guest.");
    loop {
        tokio::select! {
            input = input_rx.recv() => match input {
                Some(bytes) => sender.send(Message::Binary(bytes.into())).await.map_err(error)?,
                None => { let _ = sender.send(Message::Close(None)).await; break; }
            },
            message = receiver.next() => match message {
                Some(Ok(Message::Binary(bytes))) => {
                    let mut output = std::io::stdout().lock();
                    output.write_all(&bytes).and_then(|_| output.flush()).map_err(error)?;
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {},
                Some(Err(error_value)) => return Err(error(error_value)),
            },
            _ = tokio::signal::ctrl_c() => { let _ = sender.send(Message::Close(None)).await; break; }
        }
    }
    Ok(())
}

struct Terminal(Option<String>);
impl Terminal {
    fn raw() -> Result<Self, Error> {
        if !std::io::stdin().is_terminal() {
            return Ok(Self(None));
        }
        let saved = ProcessCommand::new("stty")
            .arg("-g")
            .stdin(Stdio::inherit())
            .output()
            .map_err(error)?;
        if !saved.status.success() {
            return Err(error("could not read terminal settings"));
        }
        let terminal = Self(Some(String::from_utf8_lossy(&saved.stdout).trim().into()));
        if !ProcessCommand::new("stty")
            .args(["raw", "-echo"])
            .stdin(Stdio::inherit())
            .status()
            .map_err(error)?
            .success()
        {
            return Err(error("could not put terminal into raw mode"));
        }
        Ok(terminal)
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            let _ = ProcessCommand::new("stty")
                .arg(saved)
                .stdin(Stdio::inherit())
                .status();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_boot_logs_fall_back_to_qemu_stderr() {
        let root = std::env::temp_dir().join(format!("machineemu-console-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("serial.log"), b"").unwrap();
        fs::write(root.join("qemu.stderr"), b"bridge helper failed").unwrap();
        assert_eq!(log_path(&root, "auto").unwrap(), root.join("qemu.stderr"));
        fs::write(root.join("serial.log"), b"login: ").unwrap();
        assert_eq!(log_path(&root, "auto").unwrap(), root.join("serial.log"));
        assert_eq!(log_path(&root, "stderr").unwrap(), root.join("qemu.stderr"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn instance_paths_reject_traversal() {
        assert!(instance_dir("../outside", Some(Path::new("/tmp"))).is_err());
    }
}
