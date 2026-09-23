use crate::Error as RuntimeError;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::IoSlice,
    os::fd::{AsRawFd, RawFd},
    path::Path,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

const MAX_FRAME: usize = 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

pub struct AsyncQmp {
    socket: BufReader<UnixStream>,
    next_id: u64,
    events: VecDeque<Value>,
}

impl AsyncQmp {
    pub async fn connect(path: &Path) -> Result<Self, RuntimeError> {
        let stream = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(path))
            .await
            .map_err(|_| RuntimeError::Qmp("QMP connection timed out".into()))?
            .map_err(|error| RuntimeError::Qmp(format!("QMP connection failed: {error}")))?;
        let mut client = Self {
            socket: BufReader::new(stream),
            next_id: 1,
            events: VecDeque::new(),
        };
        let greeting = tokio::time::timeout(Duration::from_secs(2), client.read_frame())
            .await
            .map_err(|_| RuntimeError::Qmp("QMP greeting timed out".into()))??;
        if greeting.get("QMP").is_none() {
            return Err(RuntimeError::Qmp("QMP greeting is missing".into()));
        }
        client.execute("qmp_capabilities", Value::Null).await?;
        Ok(client)
    }

    pub async fn execute(&mut self, name: &str, arguments: Value) -> Result<Value, RuntimeError> {
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(RuntimeError::Qmp("QMP command must be a token".into()));
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({"execute":name,"id":id});
        if !arguments.is_null() {
            request["arguments"] = arguments;
        }
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.extend_from_slice(b"\r\n");
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            self.socket
                .get_mut()
                .write_all(&bytes)
                .await
                .map_err(qmp_io)?;
            self.read_reply(id).await
        })
        .await
        .map_err(|_| RuntimeError::Qmp(format!("QMP {name} timed out")))?
    }

    pub async fn attach_dbus_display(&mut self, fd: RawFd) -> Result<(), RuntimeError> {
        use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
        let id = self.next_id;
        self.next_id += 1;
        let fdname = format!("me-display-{id}");
        let mut bytes =
            serde_json::to_vec(&json!({"execute":"getfd","arguments":{"fdname":fdname},"id":id}))?;
        bytes.extend_from_slice(b"\r\n");
        let sent = loop {
            self.socket.get_ref().writable().await.map_err(qmp_io)?;
            match self
                .socket
                .get_ref()
                .try_io(tokio::io::Interest::WRITABLE, || {
                    sendmsg::<()>(
                        self.socket.get_ref().as_raw_fd(),
                        &[IoSlice::new(&bytes)],
                        &[ControlMessage::ScmRights(&[fd])],
                        MsgFlags::MSG_NOSIGNAL,
                        None,
                    )
                    .map_err(std::io::Error::from)
                }) {
                Ok(sent) => break sent,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => {
                    return Err(RuntimeError::Qmp(format!(
                        "passing display socket to QEMU failed: {error}"
                    )));
                }
            }
        };
        if sent < bytes.len() {
            self.socket
                .get_mut()
                .write_all(&bytes[sent..])
                .await
                .map_err(qmp_io)?;
        }
        tokio::time::timeout(COMMAND_TIMEOUT, self.read_reply(id))
            .await
            .map_err(|_| RuntimeError::Qmp("QMP getfd timed out".into()))??;
        self.execute(
            "add_client",
            json!({"protocol":"@dbus-display","fdname":fdname}),
        )
        .await?;
        Ok(())
    }

    async fn read_reply(&mut self, id: u64) -> Result<Value, RuntimeError> {
        loop {
            let message = self.read_frame().await?;
            if message.get("event").is_some() {
                if self.events.len() == 256 {
                    self.events.pop_front();
                }
                self.events.push_back(message);
                continue;
            }
            if message.get("id") != Some(&Value::from(id)) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(RuntimeError::Qmp(error.to_string()));
            }
            return Ok(message.get("return").cloned().unwrap_or(Value::Null));
        }
    }

    pub async fn wait_device_deleted(
        &mut self,
        device_id: &str,
        timeout: Duration,
    ) -> Result<bool, RuntimeError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            while let Some(event) = self.events.pop_front() {
                if event
                    .get("data")
                    .and_then(|data| data.get("device"))
                    .and_then(Value::as_str)
                    != Some(device_id)
                {
                    continue;
                }
                match event.get("event").and_then(Value::as_str) {
                    Some("DEVICE_DELETED") => return Ok(true),
                    Some("DEVICE_UNPLUG_GUEST_ERROR") => {
                        return Err(RuntimeError::Qmp("guest rejected device removal".into()));
                    }
                    _ => {}
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            match tokio::time::timeout(remaining, self.read_frame()).await {
                Ok(Ok(event)) if event.get("event").is_some() => self.events.push_back(event),
                Ok(Ok(_)) => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => return Ok(false),
            }
        }
    }

    async fn read_frame(&mut self) -> Result<Value, RuntimeError> {
        let mut bytes = Vec::new();
        loop {
            let byte = self.socket.read_u8().await.map_err(qmp_io)?;
            if byte == b'\n' {
                break;
            }
            if bytes.len() >= MAX_FRAME {
                return Err(RuntimeError::Qmp("QMP frame exceeds size limit".into()));
            }
            bytes.push(byte);
        }
        serde_json::from_slice(&bytes).map_err(RuntimeError::from)
    }
}

fn qmp_io(error: std::io::Error) -> RuntimeError {
    RuntimeError::Qmp(format!("QMP I/O failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt},
        net::UnixListener,
    };

    #[tokio::test]
    async fn keeps_device_deleted_event_received_before_command_reply() {
        let root =
            std::env::temp_dir().join(format!("machineemu-async-qmp-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("qmp.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = tokio::io::BufReader::new(stream);
            stream
                .get_mut()
                .write_all(b"{\"QMP\":{}}\r\n")
                .await
                .unwrap();
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let capabilities: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(capabilities["execute"], "qmp_capabilities");
            stream
                .get_mut()
                .write_all(
                    format!("{}\r\n", json!({"return":{},"id":capabilities["id"]})).as_bytes(),
                )
                .await
                .unwrap();
            line.clear();
            stream.read_line(&mut line).await.unwrap();
            let remove: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(remove["execute"], "device_del");
            stream
                .get_mut()
                .write_all(
                    b"{\"event\":\"DEVICE_DELETED\",\"data\":{\"device\":\"me-usbi-1\"}}\r\n",
                )
                .await
                .unwrap();
            stream
                .get_mut()
                .write_all(format!("{}\r\n", json!({"return":{},"id":remove["id"]})).as_bytes())
                .await
                .unwrap();
        });
        let mut client = AsyncQmp::connect(&socket).await.unwrap();
        client
            .execute("device_del", json!({"id":"me-usbi-1"}))
            .await
            .unwrap();
        assert!(
            client
                .wait_device_deleted("me-usbi-1", Duration::from_millis(100))
                .await
                .unwrap()
        );
        server.await.unwrap();
        std::fs::remove_file(&socket).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    #[tokio::test]
    async fn passes_display_fd_before_add_client() {
        use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
        use std::io::{BufReader, IoSliceMut};

        let root =
            std::env::temp_dir().join(format!("machineemu-async-qmp-fd-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("qmp.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = StdUnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"{\"QMP\":{}}\r\n").unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let capabilities: Value = serde_json::from_str(&line).unwrap();
            stream
                .write_all(
                    format!("{}\r\n", json!({"return":{},"id":capabilities["id"]})).as_bytes(),
                )
                .unwrap();
            let mut bytes = [0u8; 512];
            let (count, got_fd) = {
                let mut iov = [IoSliceMut::new(&mut bytes)];
                let mut cmsg = nix::cmsg_space!([RawFd; 1]);
                let message = recvmsg::<()>(
                    stream.as_raw_fd(),
                    &mut iov,
                    Some(&mut cmsg),
                    MsgFlags::empty(),
                )
                .unwrap();
                let fd = message
                    .cmsgs()
                    .unwrap()
                    .find_map(|message| match message {
                        ControlMessageOwned::ScmRights(fds) => fds.first().copied(),
                        _ => None,
                    })
                    .unwrap();
                (message.bytes, fd)
            };
            let getfd: Value = serde_json::from_slice(&bytes[..count]).unwrap();
            assert_eq!(getfd["execute"], "getfd");
            stream
                .write_all(format!("{}\r\n", json!({"return":{},"id":getfd["id"]})).as_bytes())
                .unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            let add_client: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(add_client["execute"], "add_client");
            assert_eq!(
                add_client["arguments"]["fdname"],
                getfd["arguments"]["fdname"]
            );
            stream
                .write_all(format!("{}\r\n", json!({"return":{},"id":add_client["id"]})).as_bytes())
                .unwrap();
            nix::unistd::close(got_fd).unwrap();
        });
        let (_client, qemu_end) = StdUnixStream::pair().unwrap();
        let mut qmp = AsyncQmp::connect(&socket).await.unwrap();
        qmp.attach_dbus_display(qemu_end.as_raw_fd()).await.unwrap();
        server.join().unwrap();
        std::fs::remove_file(socket).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
