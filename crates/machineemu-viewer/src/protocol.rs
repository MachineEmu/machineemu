//! The display-stream record format carried in the `video` WebSocket.
//!
//! Each record has a 16-byte header: type (u8), flags (u8), a reserved u16
//! that must be zero, the big-endian payload length (u32) and the capture
//! time in microseconds (u64), followed by the payload. The authoritative
//! encoder lives in the sibling QEMU project's `display-stream` crate.

use bytes::{Buf, Bytes, BytesMut};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;

pub const HEADER_LEN: usize = 16;
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
/// Largest clipboard text either side of the stream accepts.
pub const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordType {
    /// JSON [`VideoConfig`], sent before every keyframe.
    Config,
    /// Annex-B access unit containing an IDR and its SPS/PPS.
    Keyframe,
    /// Annex-B access unit that depends on earlier frames.
    Delta,
    /// JSON [`VideoConfig`] after a scanout size change.
    Resize,
    /// JSON [`Control`] message, currently clipboard state.
    Control,
    /// JSON [`AudioConfig`].
    AudioConfig,
    /// Raw PCM in the most recent audio configuration's format.
    AudioData,
}

impl TryFrom<u8> for RecordType {
    type Error = ProtocolError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Config,
            1 => Self::Keyframe,
            2 => Self::Delta,
            3 => Self::Resize,
            4 => Self::Control,
            5 => Self::AudioConfig,
            6 => Self::AudioData,
            other => return Err(ProtocolError::UnknownType(other)),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub kind: RecordType,
    pub pts_us: u64,
    pub payload: Bytes,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("unknown display record type {0}")]
    UnknownType(u8),
    #[error("display record has a non-zero reserved field")]
    Reserved,
    #[error("display record payload is too large")]
    PayloadTooLarge,
}

/// Take one complete record from the front of `input`, or return `None` when
/// more bytes are needed. Records may span WebSocket messages.
pub fn decode(input: &mut BytesMut) -> Result<Option<Record>, ProtocolError> {
    if input.len() < HEADER_LEN {
        return Ok(None);
    }
    let mut header = &input[..HEADER_LEN];
    let kind = RecordType::try_from(header.get_u8())?;
    let _flags = header.get_u8();
    if header.get_u16() != 0 {
        return Err(ProtocolError::Reserved);
    }
    let length = header.get_u32() as usize;
    let pts_us = header.get_u64();
    if length > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge);
    }
    if input.len() < HEADER_LEN + length {
        return Ok(None);
    }
    input.advance(HEADER_LEN);
    Ok(Some(Record {
        kind,
        pts_us,
        payload: input.split_to(length).freeze(),
    }))
}

/// Video configuration, named for the browser's WebCodecs `VideoDecoderConfig`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct VideoConfig {
    pub codec: String,
    #[serde(rename = "codedWidth")]
    pub coded_width: u32,
    #[serde(rename = "codedHeight")]
    pub coded_height: u32,
    #[serde(default)]
    pub capture: String,
    #[serde(default)]
    pub encoder: String,
    #[serde(default)]
    pub hardware: bool,
}

/// Guest audio format as reported by QEMU's D-Bus audio interface.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioConfig {
    pub id: u64,
    pub bits: u8,
    pub signed: bool,
    pub float: bool,
    pub frequency: u32,
    pub channels: u8,
    pub bytes_per_frame: u32,
    pub big_endian: bool,
    pub enabled: bool,
    pub muted: bool,
    #[serde(default)]
    pub volume: Vec<u8>,
}

impl AudioConfig {
    /// GStreamer raw audio caps for this format, or `None` if unsupported.
    pub fn caps(&self) -> Option<String> {
        let channels = u32::from(self.channels);
        if channels == 0 || self.frequency == 0 {
            return None;
        }
        let base = match (self.float, self.signed, self.bits) {
            (true, _, 32) => "F32",
            (true, _, 64) => "F64",
            (false, true, 8) => "S8",
            (false, false, 8) => "U8",
            (false, true, 16) => "S16",
            (false, false, 16) => "U16",
            // QEMU packs 24-bit samples either in three bytes or in the low
            // bits of a 32-bit word; the frame size says which.
            (false, signed, 24) => {
                let packed = self.bytes_per_frame == 3 * channels;
                match (signed, packed) {
                    (true, true) => "S24",
                    (false, true) => "U24",
                    (true, false) => "S24_32",
                    (false, false) => "U24_32",
                }
            }
            (false, true, 32) => "S32",
            (false, false, 32) => "U32",
            _ => return None,
        };
        let format = if self.bits == 8 {
            base.to_owned()
        } else {
            format!("{base}{}", if self.big_endian { "BE" } else { "LE" })
        };
        // Beyond stereo the stream carries no channel positions.
        let mask = if channels > 2 {
            ",channel-mask=(bitmask)0x0"
        } else {
            ""
        };
        Some(format!(
            "audio/x-raw,format={format},rate={},channels={channels},layout=interleaved{mask}",
            self.frequency
        ))
    }

    /// Linear playback gain from QEMU's per-channel 0-255 volume and mute.
    pub fn gain(&self) -> f64 {
        if self.muted {
            return 0.0;
        }
        if self.volume.is_empty() {
            return 1.0;
        }
        let sum: u32 = self.volume.iter().map(|value| u32::from(*value)).sum();
        f64::from(sum) / (255.0 * self.volume.len() as f64)
    }
}

/// A control record. `text` is present when the guest clipboard changed and
/// `null` when the guest released it.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Control {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub text: Option<String>,
}

/// Messages the viewer sends to the stream as JSON text frames.
pub mod message {
    use super::*;

    pub fn request_idr() -> String {
        json!({"type": "request_idr"}).to_string()
    }
    pub fn key(keycode: u32, down: bool) -> String {
        let kind = if down { "key_down" } else { "key_up" };
        json!({"type": kind, "keycode": keycode}).to_string()
    }
    pub fn mouse_abs(x: u32, y: u32) -> String {
        json!({"type": "mouse_abs", "x": x, "y": y}).to_string()
    }
    pub fn mouse_button(button: u32, down: bool) -> String {
        let kind = if down { "mouse_down" } else { "mouse_up" };
        json!({"type": kind, "button": button}).to_string()
    }
    /// Negative steps scroll up, positive steps scroll down.
    pub fn mouse_wheel(steps: i64) -> String {
        json!({"type": "mouse_wheel", "steps": steps.clamp(-10, 10)}).to_string()
    }
    /// Guest resolution request; `None` when outside what QEMU accepts.
    pub fn resize(width: u32, height: u32) -> Option<String> {
        ((320..=7680).contains(&width) && (200..=4320).contains(&height))
            .then(|| json!({"type": "resize", "width": width, "height": height}).to_string())
    }
    pub fn clipboard_set(text: &str) -> Option<String> {
        (text.len() <= MAX_CLIPBOARD_BYTES)
            .then(|| json!({"type": "clipboard_set", "text": text}).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BufMut;

    fn encode(kind: u8, pts: u64, payload: &[u8]) -> BytesMut {
        let mut out = BytesMut::new();
        out.put_u8(kind);
        out.put_u8(0);
        out.put_u16(0);
        out.put_u32(payload.len() as u32);
        out.put_u64(pts);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn decodes_records_split_across_messages() {
        let mut bytes = encode(1, 42, b"\x00\x00\x00\x01\x65");
        bytes.extend_from_slice(&encode(2, 43, b"\x00\x00\x01\x41"));
        let tail = bytes.split_off(7);
        let mut input = bytes;
        assert_eq!(decode(&mut input).unwrap(), None);
        input.extend_from_slice(&tail);
        let first = decode(&mut input).unwrap().unwrap();
        assert_eq!(
            (first.kind, first.pts_us, &first.payload[..]),
            (RecordType::Keyframe, 42, &b"\x00\x00\x00\x01\x65"[..])
        );
        assert_eq!(decode(&mut input).unwrap().unwrap().kind, RecordType::Delta);
        assert!(input.is_empty());
    }

    #[test]
    fn rejects_bad_headers() {
        let mut unknown = encode(9, 0, b"");
        assert_eq!(decode(&mut unknown), Err(ProtocolError::UnknownType(9)));
        let mut reserved = encode(0, 0, b"");
        reserved[3] = 1;
        assert_eq!(decode(&mut reserved), Err(ProtocolError::Reserved));
        let mut large = encode(0, 0, b"");
        large[4..8].copy_from_slice(&(MAX_PAYLOAD as u32 + 1).to_be_bytes());
        assert_eq!(decode(&mut large), Err(ProtocolError::PayloadTooLarge));
    }

    #[test]
    fn parses_server_video_config() {
        let config: VideoConfig = serde_json::from_str(
            r#"{"codec":"avc1.640028","codedWidth":1920,"codedHeight":1080,"description":[1,2],"capture":"dmabuf","encoder":"vaapi","hardware":true}"#,
        )
        .unwrap();
        assert_eq!((config.coded_width, config.coded_height), (1920, 1080));
        assert!(config.hardware);
    }

    #[test]
    fn maps_audio_formats_to_caps() {
        let mut config: AudioConfig = serde_json::from_str(
            r#"{"id":1,"bits":16,"signed":true,"float":false,"frequency":48000,"channels":2,"bytesPerFrame":4,"bigEndian":false,"enabled":true,"muted":false,"volume":[255,127]}"#,
        )
        .unwrap();
        assert_eq!(
            config.caps().unwrap(),
            "audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved"
        );
        assert!((config.gain() - 382.0 / 510.0).abs() < 1e-9);
        config.bits = 24;
        config.bytes_per_frame = 8;
        assert!(config.caps().unwrap().contains("format=S24_32LE"));
        config.muted = true;
        assert_eq!(config.gain(), 0.0);
        config.bits = 12;
        assert_eq!(config.caps(), None);
    }

    #[test]
    fn bounds_outgoing_messages() {
        assert_eq!(message::resize(100, 100), None);
        assert!(message::resize(1280, 800).is_some());
        assert_eq!(
            message::clipboard_set(&"x".repeat(MAX_CLIPBOARD_BYTES + 1)),
            None
        );
        assert_eq!(
            message::mouse_wheel(-40),
            r#"{"steps":-10,"type":"mouse_wheel"}"#
        );
    }
}
