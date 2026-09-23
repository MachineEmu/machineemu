use crate::{Error, Result};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, IoSlice, Write},
    os::fd::{AsRawFd, RawFd},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
pub struct QmpClient {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
    events: VecDeque<serde_json::Value>,
    timeout: Duration,
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
            events: VecDeque::new(),
            timeout,
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
        self.read_reply(id)
    }

    /// Hand a peer-to-peer D-Bus socket to QEMU's display backend. QEMU must
    /// have been started with `-display dbus,p2p=on`.
    pub fn attach_dbus_display(&mut self, fd: RawFd) -> Result<()> {
        use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
        let id = self.next_id;
        self.next_id += 1;
        let fdname = format!("me-display-{id}");
        let mut bytes = serde_json::to_vec(&serde_json::json!({
            "execute":"getfd", "arguments":{"fdname":fdname}, "id":id
        }))?;
        bytes.extend_from_slice(b"\r\n");
        let sent = sendmsg::<()>(
            self.writer.as_raw_fd(),
            &[IoSlice::new(&bytes)],
            &[ControlMessage::ScmRights(&[fd])],
            MsgFlags::MSG_NOSIGNAL,
            None,
        )
        .map_err(|error| Error::Qmp(format!("passing display socket to QEMU failed: {error}")))?;
        if sent < bytes.len() {
            self.writer
                .write_all(&bytes[sent..])
                .map_err(|source| Error::Io {
                    path: PathBuf::from("QMP socket"),
                    source,
                })?;
        }
        self.read_reply(id)?;
        self.execute(
            "add_client",
            serde_json::json!({
                "protocol":"@dbus-display", "fdname":fdname
            }),
        )?;
        Ok(())
    }

    fn read_reply(&mut self, id: u64) -> Result<serde_json::Value> {
        loop {
            let message = self.read_message()?;
            if message.get("event").is_some() {
                if self.events.len() == 256 {
                    self.events.pop_front();
                }
                self.events.push_back(message);
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

    /// Wait for an asynchronous device-unplug result on this QMP connection.
    pub fn wait_device_deleted(&mut self, id: &str, timeout: Duration) -> Result<bool> {
        let result = self.wait_device_deleted_inner(id, timeout);
        let _ = self.reader.get_ref().set_read_timeout(Some(self.timeout));
        result
    }

    fn wait_device_deleted_inner(&mut self, id: &str, timeout: Duration) -> Result<bool> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            while let Some(event) = self.events.pop_front() {
                if event
                    .get("data")
                    .and_then(|data| data.get("device"))
                    .and_then(|value| value.as_str())
                    == Some(id)
                {
                    match event.get("event").and_then(|value| value.as_str()) {
                        Some("DEVICE_DELETED") => return Ok(true),
                        Some("DEVICE_UNPLUG_GUEST_ERROR") => {
                            return Err(Error::Qmp(format!("guest rejected removal of {id}")));
                        }
                        _ => {}
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            self.reader
                .get_ref()
                .set_read_timeout(Some(remaining.min(Duration::from_millis(250))))
                .map_err(|source| Error::Io {
                    path: PathBuf::from("QMP socket"),
                    source,
                })?;
            match self.read_message() {
                Ok(event) if event.get("event").is_some() => self.events.push_back(event),
                Ok(_) => {}
                Err(Error::Io { source, .. })
                    if matches!(
                        source.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => return Err(error),
            }
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
