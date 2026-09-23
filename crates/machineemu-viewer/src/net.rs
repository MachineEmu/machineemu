//! Daemon ticket request and the `video` WebSocket session.

use anyhow::{Context, Result, anyhow, bail};
use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;
use tracing::{debug, info, trace, warn};

use crate::media::{self, Audio, DecoderChoice, Video, VideoError};
use crate::protocol::{self, AudioConfig, Control, Cursor, RecordType, VideoConfig, message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Tcp(String),
    Unix(PathBuf),
}

impl Endpoint {
    /// Parse the CLI's endpoint syntax: `host:port` or `unix:/path`.
    pub fn parse(value: &str) -> Self {
        match value.strip_prefix("unix:") {
            Some(path) => Self::Unix(PathBuf::from(path)),
            None => Self::Tcp(value.to_owned()),
        }
    }

    /// The HTTP `Host` value. The daemon's WebSocket check requires `Origin`
    /// to be `http://` plus this value.
    fn host(&self) -> &str {
        match self {
            Self::Tcp(address) => address,
            Self::Unix(_) => "localhost",
        }
    }

    async fn connect(&self) -> Result<Box<dyn Io>> {
        match self {
            Self::Tcp(address) => Ok(Box::new(
                tokio::net::TcpStream::connect(address)
                    .await
                    .with_context(|| format!("cannot connect to daemon at {address}"))?,
            )),
            #[cfg(unix)]
            Self::Unix(path) => Ok(Box::new(
                tokio::net::UnixStream::connect(path)
                    .await
                    .with_context(|| format!("cannot connect to daemon at {}", path.display()))?,
            )),
            #[cfg(not(unix))]
            Self::Unix(_) => bail!("Unix daemon sockets are unavailable on this platform"),
        }
    }
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub struct Settings {
    pub endpoint: Endpoint,
    pub token: String,
    pub instance: String,
    pub control: bool,
    pub takeover: bool,
    pub audio: bool,
    pub decoder: DecoderChoice,
}

/// Events for the window.
pub enum Event {
    Config(VideoConfig),
    Cursor(Cursor),
    Decoder {
        name: String,
        hardware: bool,
    },
    Clipboard {
        available: bool,
        text: Option<String>,
    },
}

fn check_instance_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        bail!("invalid instance id {id:?}");
    }
    Ok(())
}

/// Create a single-use stream ticket with the daemon bearer token.
async fn issue_ticket(settings: &Settings) -> Result<String> {
    let body =
        serde_json::json!({"control": settings.control, "takeover": settings.takeover}).to_string();
    let request = format!(
        "POST /api/v2/instances/{}/streams/video/ticket HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        settings.instance,
        settings.endpoint.host(),
        settings.token,
        body.len()
    );
    let mut stream = settings.endpoint.connect().await?;
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .context("cannot read the ticket response")?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("malformed daemon response")?;
    let header = String::from_utf8_lossy(&response[..split]);
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(599);
    debug!(status, "ticket response");
    let head = header.to_ascii_lowercase();
    let mut payload = response[split + 4..].to_vec();
    if head.contains("transfer-encoding: chunked") {
        payload = dechunk(&payload).context("malformed chunked daemon response")?;
    }
    let value: serde_json::Value = serde_json::from_slice(&payload).unwrap_or_default();
    if !(200..300).contains(&status) {
        let reason = value["error"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| String::from_utf8_lossy(&payload).into_owned());
        bail!("ticket request failed with HTTP {status}: {reason}");
    }
    value["ticket"]
        .as_str()
        .map(str::to_owned)
        .context("ticket response has no ticket")
}

fn dechunk(mut input: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let line = input.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&input[..line]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        input = &input[line + 2..];
        if size == 0 {
            return Some(output);
        }
        output.extend_from_slice(input.get(..size)?);
        input = input.get(size + 2..)?;
    }
}

/// Counts for the periodic statistics line, reset each interval.
#[derive(Default)]
struct Stats {
    bytes: u64,
    keyframes: u64,
    deltas: u64,
    skipped: u64,
    audio_bytes: u64,
    sent: u64,
}

const STATS_INTERVAL: Duration = Duration::from_secs(5);

/// The message type of an outgoing control message, for logging. Payloads
/// are not logged: clipboard text can hold anything.
fn message_type(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| value["type"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "invalid".into())
}

/// Run one viewing session until the stream closes or fails. Returns the
/// reason the stream ended.
#[tracing::instrument(name = "session", skip_all, fields(instance = %settings.instance))]
pub async fn run(
    settings: Settings,
    video: Arc<Video>,
    mut video_errors: mpsc::UnboundedReceiver<VideoError>,
    events: impl Fn(Event),
    mut outbound: mpsc::UnboundedReceiver<String>,
) -> Result<String> {
    check_instance_id(&settings.instance)?;
    let started = Instant::now();
    let ticket = issue_ticket(&settings).await?;
    debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "stream ticket issued"
    );
    let host = settings.endpoint.host().to_owned();
    let mut request = format!(
        "ws://{host}/ws/v2/instances/{}/video?ticket={ticket}",
        settings.instance
    )
    .into_client_request()?;
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_str(&format!("http://{host}"))?);
    let stream = settings.endpoint.connect().await?;
    let (mut socket, response) = tokio_tungstenite::client_async(request, stream)
        .await
        .context("video WebSocket handshake failed")?;
    info!(
        status = response.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        control = settings.control,
        "video stream connected"
    );
    socket.send(Message::text(message::request_idr())).await?;
    debug!("requested keyframe");

    let (plan, _) = video.plan();
    events(Event::Decoder {
        name: plan.decoder.clone(),
        hardware: plan.hardware,
    });

    let mut audio = Audio::default();
    let mut buffer = BytesMut::new();
    let mut awaiting_keyframe = true;
    let mut first_keyframe = true;
    let mut stats = Stats::default();
    let mut stats_tick = tokio::time::interval(STATS_INTERVAL);
    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    stats_tick.tick().await;
    let (mut last_decoded, mut last_superseded) = (0, 0);
    loop {
        tokio::select! {
            incoming = socket.next() => {
                let Some(incoming) = incoming else {
                    info!("video stream ended without a close frame");
                    return Ok("the daemon closed the stream".into());
                };
                match incoming.context("video stream failed")? {
                    Message::Binary(data) => {
                        stats.bytes += data.len() as u64;
                        buffer.extend_from_slice(&data);
                        while let Some(record) = protocol::decode(&mut buffer)? {
                            trace!(kind = ?record.kind, bytes = record.payload.len(), pts_us = record.pts_us, "record");
                            match record.kind {
                                RecordType::Config | RecordType::Resize => {
                                    let config: VideoConfig = serde_json::from_slice(&record.payload)
                                        .context("invalid video configuration")?;
                                    debug!(kind = ?record.kind, ?config, "video configuration");
                                    events(Event::Config(config));
                                }
                                RecordType::Keyframe => {
                                    if first_keyframe || awaiting_keyframe {
                                        info!(
                                            skipped_deltas = stats.skipped,
                                            elapsed_ms = started.elapsed().as_millis() as u64,
                                            "keyframe received; decoding"
                                        );
                                        first_keyframe = false;
                                    }
                                    awaiting_keyframe = false;
                                    stats.keyframes += 1;
                                    video.push(record.payload, record.pts_us)?;
                                }
                                RecordType::Delta if !awaiting_keyframe => {
                                    stats.deltas += 1;
                                    video.push(record.payload, record.pts_us)?;
                                }
                                RecordType::Delta => stats.skipped += 1,
                                RecordType::Control => {
                                    let value: serde_json::Value = serde_json::from_slice(&record.payload)
                                        .context("invalid control record")?;
                                    if value["type"] == "cursor" {
                                        match serde_json::from_value::<Cursor>(value) {
                                            Ok(cursor) if cursor.valid() => events(Event::Cursor(cursor)),
                                            Ok(_) => warn!("invalid cursor geometry or image"),
                                            Err(error) => warn!(%error, "invalid cursor record"),
                                        }
                                        continue;
                                    }
                                    match serde_json::from_value::<Control>(value) {
                                        Ok(control) if control.kind == "clipboard" => {
                                            debug!(
                                                available = control.available,
                                                text_bytes = control.text.as_ref().map(String::len),
                                                "guest clipboard"
                                            );
                                            events(Event::Clipboard { available: control.available, text: control.text });
                                        }
                                        Ok(control) => debug!(kind = control.kind, "ignored control record"),
                                        Err(error) => warn!(%error, "invalid control record"),
                                    }
                                }
                                RecordType::AudioConfig if settings.audio => {
                                    let config: AudioConfig = serde_json::from_slice(&record.payload)
                                        .context("invalid audio configuration")?;
                                    if let Err(error) = audio.configure(&config) {
                                        warn!(error = format!("{error:#}"), "audio disabled");
                                    }
                                }
                                RecordType::AudioData if settings.audio => {
                                    stats.audio_bytes += record.payload.len() as u64;
                                    if let Err(error) = audio.push(record.payload) {
                                        warn!(error = format!("{error:#}"), "audio playback failed");
                                    }
                                }
                                RecordType::AudioConfig | RecordType::AudioData => {}
                            }
                        }
                    }
                    Message::Close(frame) => {
                        info!(?frame, "daemon closed the video stream");
                        return Ok(frame
                            .map(|frame| format!("the daemon closed the stream: {}", frame.reason))
                            .unwrap_or_else(|| "the daemon closed the stream".into()));
                    }
                    other => debug!(?other, "ignored WebSocket message"),
                }
            }
            error = video_errors.recv() => {
                let Some(error) = error else { continue };
                let (plan, generation) = video.plan();
                if error.generation != generation {
                    debug!(error.generation, generation, "ignored error from a replaced pipeline");
                    continue;
                }
                // Hardware decode is preferred, but a device that registers a
                // decoder can still fail on a stream; Auto then continues in
                // software from the next keyframe.
                let fallback = (settings.decoder == DecoderChoice::Auto && plan.hardware)
                    .then(media::software_fallback)
                    .flatten();
                let Some(fallback) = fallback else {
                    return Err(anyhow!("video decoding failed: {}", error.message));
                };
                warn!(
                    failed = plan.decoder,
                    error = error.message,
                    fallback = fallback.decoder,
                    "hardware decoder failed; falling back to software"
                );
                video.rebuild(&fallback)?;
                events(Event::Decoder { name: fallback.decoder.clone(), hardware: fallback.hardware });
                awaiting_keyframe = true;
                socket.send(Message::text(message::request_idr())).await?;
                debug!("requested keyframe");
            }
            outgoing = outbound.recv() => {
                let Some(text) = outgoing else {
                    info!("viewer closed; closing the stream");
                    let _ = socket.close(None).await;
                    return Ok("viewer closed".into());
                };
                let kind = message_type(&text);
                if kind == "mouse_abs" {
                    trace!(kind, "sending control message");
                } else {
                    debug!(kind, bytes = text.len(), "sending control message");
                }
                stats.sent += 1;
                socket.send(Message::text(text)).await?;
            }
            _ = stats_tick.tick() => {
                let counters = video.counters();
                let decoded = counters.decoded.load(Ordering::Relaxed);
                let superseded = counters.superseded.load(Ordering::Relaxed);
                let seconds = STATS_INTERVAL.as_secs_f64();
                debug!(
                    kbit_per_s = (stats.bytes as f64 * 8.0 / 1000.0 / seconds).round() as u64,
                    keyframes = stats.keyframes,
                    deltas = stats.deltas,
                    skipped_before_keyframe = stats.skipped,
                    decoded_fps = ((decoded - last_decoded) as f64 / seconds).round() as u64,
                    not_drawn = superseded - last_superseded,
                    audio_bytes = stats.audio_bytes,
                    sent = stats.sent,
                    queued_bytes = buffer.len(),
                    "stream statistics"
                );
                (last_decoded, last_superseded) = (decoded, superseded);
                stats = Stats::default();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoints() {
        assert_eq!(
            Endpoint::parse("127.0.0.1:8787"),
            Endpoint::Tcp("127.0.0.1:8787".into())
        );
        let unix = Endpoint::parse("unix:/run/me.sock");
        assert_eq!(unix, Endpoint::Unix("/run/me.sock".into()));
        assert_eq!(unix.host(), "localhost");
    }

    #[test]
    fn rejects_path_characters_in_instance_ids() {
        assert!(check_instance_id("win11-dev_1").is_ok());
        assert!(check_instance_id("../x").is_err());
        assert!(check_instance_id("").is_err());
    }

    #[test]
    fn decodes_chunked_bodies() {
        assert_eq!(
            dechunk(b"4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n").unwrap(),
            b"{\"a\":1}"
        );
    }

    #[tokio::test]
    async fn requests_a_ticket_with_the_bearer_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0u8; 4096];
            let count = socket.read(&mut bytes).await.unwrap();
            let request = String::from_utf8_lossy(&bytes[..count]).into_owned();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 42\r\n\r\n{\"ticket\":\"abc\",\"expires_in_seconds\":30}  ")
                .await
                .unwrap();
            request
        });
        let settings = Settings {
            endpoint: Endpoint::Tcp(address),
            token: "secret".into(),
            instance: "vm1".into(),
            control: true,
            takeover: false,
            audio: true,
            decoder: DecoderChoice::Auto,
        };
        assert_eq!(issue_ticket(&settings).await.unwrap(), "abc");
        let request = server.await.unwrap();
        assert!(
            request.starts_with("POST /api/v2/instances/vm1/streams/video/ticket HTTP/1.1\r\n")
        );
        assert!(request.contains("Authorization: Bearer secret\r\n"));
        assert!(request.ends_with(r#"{"control":true,"takeover":false}"#));
    }
}
