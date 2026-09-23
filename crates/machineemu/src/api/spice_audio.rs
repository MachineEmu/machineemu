//! Restrict the SPICE socket to the three audio channels used by the browser.
use super::streams::{AudioSession, Endpoint, StreamTicket};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LINK_HEADER: usize = 16;
const LINK_MESS: usize = 18;
const LINK_REPLY: usize = 178;
const MINI_HEADER: usize = 6;
const MAX_LINK: usize = 4096;
const MAX_CLIENT: usize = 64 * 1024;
const MAX_SERVER: usize = 512 * 1024;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn u16le(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}
fn u32le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn cap(words: &[u8], bit: u32) -> bool {
    let offset = (bit / 32) as usize * 4;
    offset + 4 <= words.len() && (u32le(words, offset) & (1 << (bit % 32))) != 0
}
fn capabilities(body: &[u8], offset: usize, common: usize, channel: usize) -> io::Result<&[u8]> {
    if common + channel > MAX_LINK / 4 || offset < LINK_MESS {
        return Err(invalid("invalid SPICE capabilities"));
    }
    let common_end = offset
        .checked_add(common * 4)
        .ok_or_else(|| invalid("invalid SPICE capabilities"))?;
    let end = common_end
        .checked_add(channel * 4)
        .ok_or_else(|| invalid("invalid SPICE capabilities"))?;
    if end > body.len() {
        return Err(invalid("SPICE capabilities exceed the link message"));
    }
    Ok(&body[offset..common_end])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Header,
    Link,
    Auth,
    Ticket,
    Messages,
}

struct ClientGate {
    channel: &'static str,
    connection_id: u32,
    phase: Phase,
    link_size: usize,
    buffer: Vec<u8>,
}
impl ClientGate {
    fn new(channel: &'static str, connection_id: u32) -> Self {
        Self {
            channel,
            connection_id,
            phase: Phase::Header,
            link_size: 0,
            buffer: Vec::new(),
        }
    }
    fn feed(&mut self, input: &[u8]) -> io::Result<Vec<u8>> {
        self.buffer.extend_from_slice(input);
        if self.buffer.len() > MAX_LINK + MAX_CLIENT + MINI_HEADER {
            return Err(invalid("SPICE client buffer is too large"));
        }
        let mut output = Vec::new();
        loop {
            let length = match self.phase {
                Phase::Header => {
                    if self.buffer.len() < LINK_HEADER {
                        break;
                    }
                    if &self.buffer[..4] != b"REDQ" || u32le(&self.buffer, 4) != 2 {
                        return Err(invalid("invalid SPICE link header"));
                    }
                    self.link_size = u32le(&self.buffer, 12) as usize;
                    if !(LINK_MESS..=MAX_LINK).contains(&self.link_size) {
                        return Err(invalid("invalid SPICE link size"));
                    }
                    self.phase = Phase::Link;
                    LINK_HEADER
                }
                Phase::Link => {
                    if self.buffer.len() < self.link_size {
                        break;
                    }
                    let body = &self.buffer[..self.link_size];
                    let expected = match self.channel {
                        "main" => 1,
                        "playback" => 5,
                        "record" => 6,
                        _ => unreachable!(),
                    };
                    if u32le(body, 0) != self.connection_id || body[4] != expected || body[5] != 0 {
                        return Err(invalid(
                            "SPICE audio channel or connection ID does not match",
                        ));
                    }
                    let common = capabilities(
                        body,
                        u32le(body, 14) as usize,
                        u32le(body, 6) as usize,
                        u32le(body, 10) as usize,
                    )?;
                    if !cap(common, 1) || !cap(common, 3) || cap(common, 2) {
                        return Err(invalid("SPICE audio requires ticket auth and mini headers"));
                    }
                    self.phase = Phase::Auth;
                    self.link_size
                }
                Phase::Auth => {
                    if self.buffer.len() < 4 {
                        break;
                    }
                    if u32le(&self.buffer, 0) != 1 {
                        return Err(invalid("SPICE SASL authentication is not allowed"));
                    }
                    self.phase = Phase::Ticket;
                    4
                }
                Phase::Ticket => {
                    if self.buffer.len() < 128 {
                        break;
                    }
                    self.phase = Phase::Messages;
                    128
                }
                Phase::Messages => {
                    if self.buffer.len() < MINI_HEADER {
                        break;
                    }
                    let kind = u16le(&self.buffer, 0);
                    let size = u32le(&self.buffer, 2) as usize;
                    let allowed = matches!(kind, 1 | 2 | 3 | 6)
                        || match self.channel {
                            "main" => matches!(kind, 101 | 104),
                            "record" => matches!(kind, 101..=103),
                            _ => false,
                        };
                    if !allowed || size > MAX_CLIENT {
                        return Err(invalid(
                            "SPICE message is not allowed on this audio channel",
                        ));
                    }
                    if self.buffer.len() < MINI_HEADER + size {
                        break;
                    }
                    MINI_HEADER + size
                }
            };
            output.extend_from_slice(&self.buffer[..length]);
            self.buffer.drain(..length);
        }
        Ok(output)
    }
}

#[derive(Clone, Copy)]
enum ServerPhase {
    Header,
    Reply,
    Result,
    Messages,
    Done,
}
struct ServerGate {
    channel: &'static str,
    phase: ServerPhase,
    reply_size: usize,
    buffer: Vec<u8>,
    connection_id: Option<u32>,
}
impl ServerGate {
    fn new(channel: &'static str) -> Self {
        Self {
            channel,
            phase: ServerPhase::Header,
            reply_size: 0,
            buffer: Vec::new(),
            connection_id: None,
        }
    }
    fn feed(&mut self, input: &[u8]) -> io::Result<Option<u32>> {
        self.buffer.extend_from_slice(input);
        if self.buffer.len() > MAX_LINK + MAX_SERVER + MINI_HEADER {
            return Err(invalid("SPICE server buffer is too large"));
        }
        loop {
            let length = match self.phase {
                ServerPhase::Header => {
                    if self.buffer.len() < LINK_HEADER {
                        break;
                    }
                    if &self.buffer[..4] != b"REDQ" || u32le(&self.buffer, 4) != 2 {
                        return Err(invalid("invalid SPICE server header"));
                    }
                    self.reply_size = u32le(&self.buffer, 12) as usize;
                    if !(LINK_REPLY..=MAX_LINK).contains(&self.reply_size) {
                        return Err(invalid("invalid SPICE reply size"));
                    }
                    self.phase = ServerPhase::Reply;
                    LINK_HEADER
                }
                ServerPhase::Reply => {
                    if self.buffer.len() < self.reply_size {
                        break;
                    }
                    let body = &self.buffer[..self.reply_size];
                    if u32le(body, 0) != 0 {
                        self.phase = ServerPhase::Done;
                    } else {
                        let common = capabilities(
                            body,
                            u32le(body, 174) as usize,
                            u32le(body, 166) as usize,
                            u32le(body, 170) as usize,
                        )?;
                        if !cap(common, 3) {
                            return Err(invalid("SPICE server declined mini headers"));
                        }
                        self.phase = ServerPhase::Result;
                    }
                    self.reply_size
                }
                ServerPhase::Result => {
                    if self.buffer.len() < 4 {
                        break;
                    }
                    self.phase = if u32le(&self.buffer, 0) == 0 {
                        ServerPhase::Messages
                    } else {
                        ServerPhase::Done
                    };
                    4
                }
                ServerPhase::Messages => {
                    if self.buffer.len() < MINI_HEADER {
                        break;
                    }
                    let kind = u16le(&self.buffer, 0);
                    let size = u32le(&self.buffer, 2) as usize;
                    if size > MAX_SERVER {
                        return Err(invalid("SPICE server message is too large"));
                    }
                    if self.buffer.len() < MINI_HEADER + size {
                        break;
                    }
                    if self.channel == "main" && kind == 103 && self.connection_id.is_none() {
                        if size < 32 {
                            return Err(invalid("SPICE main init is too short"));
                        }
                        self.connection_id = Some(u32le(&self.buffer, MINI_HEADER));
                    }
                    MINI_HEADER + size
                }
                ServerPhase::Done => {
                    self.buffer.clear();
                    break;
                }
            };
            self.buffer.drain(..length);
        }
        Ok(self.connection_id)
    }
}

pub(super) async fn relay(
    socket: WebSocket,
    ticket: StreamTicket,
    expected_id: u32,
    sessions: Arc<Mutex<BTreeMap<String, AudioSession>>>,
) -> io::Result<()> {
    let channel: &'static str = match ticket.kind.as_str() {
        "spice-main" => "main",
        "spice-playback" => "playback",
        "spice-record" => "record",
        _ => return Err(invalid("unknown SPICE audio channel")),
    };
    let group = ticket
        .audio_group
        .ok_or_else(|| invalid("audio ticket has no client binding"))?;
    let Endpoint::Unix(path) = ticket.endpoint else {
        return Err(invalid("SPICE audio must use a Unix socket"));
    };
    let (mut sender, mut receiver) = socket.split();
    let mut gate = ClientGate::new(channel, expected_id);
    let mut prelude = Vec::new();
    while matches!(gate.phase, Phase::Header | Phase::Link) {
        let message = tokio::time::timeout(Duration::from_secs(10), receiver.next())
            .await
            .map_err(|_| invalid("SPICE link timed out"))?
            .ok_or_else(|| invalid("SPICE link closed"))?
            .map_err(io::Error::other)?;
        let Message::Binary(bytes) = message else {
            return Err(invalid("SPICE audio needs binary frames"));
        };
        if bytes.len() > MAX_CLIENT {
            return Err(invalid("SPICE frame is too large"));
        }
        prelude.extend(gate.feed(&bytes)?);
    }
    let stream = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::UnixStream::connect(path),
    )
    .await??;
    let (mut reader, mut writer) = stream.into_split();
    writer.write_all(&prelude).await?;
    let mut observer = ServerGate::new(channel);
    let to_browser = async {
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer).await?;
            if count == 0 {
                return Ok::<(), io::Error>(());
            }
            if let Some(id) = observer.feed(&buffer[..count])?
                && channel == "main"
            {
                let mut sessions = sessions
                    .lock()
                    .map_err(|_| invalid("audio session lock poisoned"))?;
                if let Some(session) = sessions.get_mut(&group) {
                    session.connection_id = Some(id);
                }
            }
            sender
                .send(Message::Binary(buffer[..count].to_vec()))
                .await
                .map_err(io::Error::other)?;
        }
    };
    let from_browser = async {
        while let Some(message) = receiver.next().await {
            match message.map_err(io::Error::other)? {
                Message::Binary(bytes) if bytes.len() <= MAX_CLIENT => {
                    let forwarded = gate.feed(&bytes)?;
                    if !forwarded.is_empty() {
                        writer.write_all(&forwarded).await?;
                    }
                }
                Message::Close(_) => return Ok::<(), io::Error>(()),
                Message::Ping(_) | Message::Pong(_) => {}
                _ => return Err(invalid("invalid SPICE audio frame")),
            }
        }
        Ok(())
    };
    let watchdog = async {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let live = sessions
                .lock()
                .map_err(|_| invalid("audio session lock poisoned"))?
                .get(&group)
                .is_some_and(|session| session.active.contains("main"));
            if !live {
                return Err(invalid("audio main channel closed"));
            }
        }
    };
    tokio::select! { result = to_browser => result, result = from_browser => result, result = watchdog => result }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(channel: u8, connection_id: u32) -> Vec<u8> {
        let mut body = vec![0u8; 22];
        body[0..4].copy_from_slice(&connection_id.to_le_bytes());
        body[4] = channel;
        body[6..10].copy_from_slice(&1u32.to_le_bytes());
        body[14..18].copy_from_slice(&18u32.to_le_bytes());
        body[18..22].copy_from_slice(&0b1010u32.to_le_bytes());
        let mut bytes = b"REDQ".to_vec();
        bytes.extend(2u32.to_le_bytes());
        bytes.extend(2u32.to_le_bytes());
        bytes.extend((body.len() as u32).to_le_bytes());
        bytes.extend(body);
        bytes
    }

    #[test]
    fn rejects_other_spice_channels_and_connection_ids() {
        let mut gate = ClientGate::new("playback", 42);
        assert!(gate.feed(&link(2, 42)).is_err());
        let mut gate = ClientGate::new("playback", 42);
        assert!(gate.feed(&link(5, 7)).is_err());
    }

    #[test]
    fn fragments_link_and_rejects_non_audio_message() {
        let mut gate = ClientGate::new("playback", 42);
        let link = link(5, 42);
        assert!(gate.feed(&link[..7]).unwrap().is_empty());
        assert_eq!(gate.feed(&link[7..]).unwrap(), link);
        assert_eq!(gate.phase, Phase::Auth);
        assert_eq!(gate.feed(&1u32.to_le_bytes()).unwrap(), 1u32.to_le_bytes());
        assert_eq!(gate.feed(&[0; 128]).unwrap().len(), 128);
        let mut message = 101u16.to_le_bytes().to_vec();
        message.extend(0u32.to_le_bytes());
        assert!(gate.feed(&message).is_err());
    }

    #[test]
    fn server_main_init_binds_child_connection_id() {
        let mut body = vec![0u8; 182];
        body[166..170].copy_from_slice(&1u32.to_le_bytes());
        body[174..178].copy_from_slice(&178u32.to_le_bytes());
        body[178..182].copy_from_slice(&0b1000u32.to_le_bytes());
        let mut reply = b"REDQ".to_vec();
        reply.extend(2u32.to_le_bytes());
        reply.extend(2u32.to_le_bytes());
        reply.extend((body.len() as u32).to_le_bytes());
        reply.extend(body);
        reply.extend(0u32.to_le_bytes());
        reply.extend(103u16.to_le_bytes());
        reply.extend(32u32.to_le_bytes());
        reply.extend(42u32.to_le_bytes());
        reply.extend([0u8; 28]);
        let mut gate = ServerGate::new("main");
        assert_eq!(gate.feed(&reply[..19]).unwrap(), None);
        assert_eq!(gate.feed(&reply[19..]).unwrap(), Some(42));
    }

    #[test]
    fn server_rejects_missing_mini_header_capability() {
        let mut body = vec![0u8; 182];
        body[166..170].copy_from_slice(&1u32.to_le_bytes());
        body[174..178].copy_from_slice(&178u32.to_le_bytes());
        let mut reply = b"REDQ".to_vec();
        reply.extend(2u32.to_le_bytes());
        reply.extend(2u32.to_le_bytes());
        reply.extend((body.len() as u32).to_le_bytes());
        reply.extend(body);
        assert!(ServerGate::new("main").feed(&reply).is_err());
    }
}
