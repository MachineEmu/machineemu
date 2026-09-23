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

pub(super) fn logs(
    instance: &str,
    workspace: Option<&Path>,
    follow: bool,
    lines: usize,
    source: &str,
) -> Result<(), Error> {
    let directory = instance_dir(instance, workspace)?;
    let path = log_path(&directory, source)?;
    let mut command = ProcessCommand::new("tail");
    command.args(["-n", &lines.to_string()]);
    if follow {
        command.arg("-F");
    }
    let status = command.arg("--").arg(&path).status().map_err(error)?;
    if !status.success() {
        return Err(error(format!(
            "tail {} exited with {status}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn serial(instance: &str, workspace: Option<&Path>) -> Result<(), Error> {
    let directory = instance_dir(instance, workspace)?;
    let path = directory.join("serial.sock");
    if !path.exists() {
        if directory.join("serial.log").is_file() {
            eprintln!(
                "{instance}: this run has a file-only console; following output read-only (Ctrl-C exits). Restart with console.uart=true or devices.serial=socket for interactive input."
            );
            return logs(instance, workspace, true, 100, "serial");
        }
        return Err(error(format!(
            "{instance}: no UART socket in this run; console.uart=true takes effect on the next launch. Restart the instance, or inspect `machineemu logs {instance}` for startup errors"
        )));
    }
    let mut socket = UnixStream::connect(&path).map_err(|e| error(format!("cannot attach to {instance}: {e}; the instance may be stopped. Use `machineemu logs {instance}` for saved output")))?;
    let mut input_socket = socket.try_clone().map_err(error)?;
    let signal_socket = socket.try_clone().map_err(error)?;
    // Signal handlers close the stream so the terminal guard is dropped even
    // when another process sends SIGINT/SIGTERM. Raw-mode Ctrl-C goes to UART.
    let runtime = tokio::runtime::Runtime::new().map_err(error)?;
    let signal_task = runtime.spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut hangup = signal(SignalKind::hangup())?;
        tokio::select! {
            _ = term.recv() => {},
            _ = interrupt.recv() => {},
            _ = hangup.recv() => {},
        }
        signal_socket.shutdown(std::net::Shutdown::Both)
    });
    let _terminal = Terminal::raw()?;
    eprintln!("Attached to {instance}. Ctrl-] detaches; Ctrl-C is sent to the guest.");
    thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        let mut bytes = [0; 4096];
        loop {
            let count = match input.read(&mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let end = bytes[..count]
                .iter()
                .position(|b| *b == 0x1d)
                .unwrap_or(count);
            if input_socket.write_all(&bytes[..end]).is_err() || end != count {
                break;
            }
        }
        let _ = input_socket.shutdown(std::net::Shutdown::Both);
    });
    let result = (|| {
        let mut output = std::io::stdout().lock();
        let mut bytes = [0; 4096];
        loop {
            let count = socket.read(&mut bytes).map_err(error)?;
            if count == 0 {
                break;
            }
            output.write_all(&bytes[..count]).map_err(error)?;
            output.flush().map_err(error)?;
        }
        Ok(())
    })();
    signal_task.abort();
    result
}

#[cfg(not(unix))]
pub(super) fn serial(_instance: &str, _workspace: Option<&Path>) -> Result<(), Error> {
    Err(error("interactive serial requires Unix sockets"))
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
