//! Transport-independent framing and control messages for QEMU display streams.
//!
//! The encoder and QEMU D-Bus listener are separate from this
//! crate. This keeps the wire contract testable on machines without a
//! guest, VA-API, or a graphical session.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

pub const HEADER_LEN: usize = 16;
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
pub const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;
pub const MAX_CURSOR_DIMENSION: u32 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordType {
    Config = 0,
    Keyframe = 1,
    Delta = 2,
    Resize = 3,
    Control = 4,
    AudioConfig = 5,
    AudioData = 6,
}

impl TryFrom<u8> for RecordType {
    type Error = VideoError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Config),
            1 => Ok(Self::Keyframe),
            2 => Ok(Self::Delta),
            3 => Ok(Self::Resize),
            4 => Ok(Self::Control),
            5 => Ok(Self::AudioConfig),
            6 => Ok(Self::AudioData),
            _ => Err(VideoError::UnknownType(value)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub kind: RecordType,
    pub flags: u8,
    pub pts_us: u64,
    pub payload: Bytes,
}

impl Record {
    pub fn encode(&self, output: &mut BytesMut) -> Result<(), VideoError> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(VideoError::PayloadTooLarge);
        }
        output.reserve(HEADER_LEN + self.payload.len());
        output.put_u8(self.kind as u8);
        output.put_u8(self.flags);
        output.put_u16(0);
        output.put_u32(self.payload.len() as u32);
        output.put_u64(self.pts_us);
        output.extend_from_slice(&self.payload);
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum VideoError {
    #[error("unknown display record type {0}")]
    UnknownType(u8),
    #[error("display record payload is too large")]
    PayloadTooLarge,
    #[error("display record has a non-zero reserved field")]
    Reserved,
    #[error("display record is truncated")]
    Truncated,
}

pub fn decode(input: &mut BytesMut) -> Result<Option<Record>, VideoError> {
    if input.len() < HEADER_LEN {
        return Ok(None);
    }
    let mut header = &input[..HEADER_LEN];
    let kind = RecordType::try_from(header.get_u8())?;
    let flags = header.get_u8();
    if header.get_u16() != 0 {
        return Err(VideoError::Reserved);
    }
    let length = header.get_u32() as usize;
    let pts_us = header.get_u64();
    if length > MAX_PAYLOAD {
        return Err(VideoError::PayloadTooLarge);
    }
    if input.len() < HEADER_LEN + length {
        return Ok(None);
    }
    input.advance(HEADER_LEN);
    Ok(Some(Record {
        kind,
        flags,
        pts_us,
        payload: input.split_to(length).freeze(),
    }))
}

/// Shared protocol error name for decoder clients.
pub type ProtocolError = VideoError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoConfig {
    pub codec: String,
    #[serde(rename = "codedWidth")]
    pub coded_width: u32,
    #[serde(rename = "codedHeight")]
    pub coded_height: u32,
    /// AVCDecoderConfigurationRecord bytes for WebCodecs.
    pub description: Vec<u8>,
    /// Server-side source path, such as `dmabuf` or `cpu-readback`.
    #[serde(default)]
    pub capture: String,
    /// Server-side H.264 encoder implementation.
    #[serde(default)]
    pub encoder: String,
    /// Whether the server-side encoder is hardware accelerated.
    #[serde(default)]
    pub hardware: bool,
}

/// Guest audio format as reported by QEMU's D-Bus audio interface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

/// QEMU's separate cursor plane, carried as a control record. `data` is
/// little-endian ARGB pixels, with one 32-bit pixel per entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cursor {
    #[serde(rename = "type")]
    pub kind: String,
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub width: u32,
    pub height: u32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub data: Vec<u8>,
}

impl Cursor {
    pub fn valid(&self) -> bool {
        self.kind == "cursor"
            && self.width <= MAX_CURSOR_DIMENSION
            && self.height <= MAX_CURSOR_DIMENSION
            && self.hot_x >= 0
            && self.hot_y >= 0
            && (self.width == 0 || self.hot_x < self.width as i32)
            && (self.height == 0 || self.hot_y < self.height as i32)
            && self.data.len() == self.width as usize * self.height as usize * 4
    }
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

    #[test]
    fn cursor_control_record_round_trips_and_bounds_shape() {
        let cursor = Cursor {
            kind: "cursor".into(),
            x: 12,
            y: 8,
            visible: true,
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            data: vec![0, 0, 255, 255],
        };
        assert!(cursor.valid());
        let mut wire = BytesMut::new();
        Record {
            kind: RecordType::Control,
            flags: 0,
            pts_us: 42,
            payload: Bytes::from(serde_json::to_vec(&cursor).unwrap()),
        }
        .encode(&mut wire)
        .unwrap();
        let record = decode(&mut wire).unwrap().unwrap();
        assert_eq!(record.kind, RecordType::Control);
        assert_eq!(
            serde_json::from_slice::<Cursor>(&record.payload).unwrap(),
            cursor
        );
        let mut invalid = cursor;
        invalid.data.pop();
        assert!(!invalid.valid());
    }

    #[test]
    fn records_round_trip_with_partial_input() {
        let record = Record {
            kind: RecordType::Keyframe,
            flags: 1,
            pts_us: 42,
            payload: Bytes::from_static(b"\x00\x00\x00\x01\x65"),
        };
        let mut encoded = BytesMut::new();
        record.encode(&mut encoded).unwrap();
        let split = encoded.split_off(7);
        let mut input = encoded;
        assert!(decode(&mut input).unwrap().is_none());
        input.extend_from_slice(&split);
        assert_eq!(decode(&mut input).unwrap(), Some(record));
        assert!(input.is_empty());
    }

    #[test]
    fn config_uses_browser_names() {
        let config = VideoConfig {
            codec: "avc1.640028".into(),
            coded_width: 1920,
            coded_height: 1080,
            description: Vec::new(),
            capture: "dmabuf".into(),
            encoder: "vaapi".into(),
            hardware: true,
        };
        assert_eq!(
            serde_json::to_string(&config).unwrap(),
            r#"{"codec":"avc1.640028","codedWidth":1920,"codedHeight":1080,"description":[],"capture":"dmabuf","encoder":"vaapi","hardware":true}"#
        );
    }

    #[test]
    fn audio_records_round_trip() {
        for kind in [RecordType::AudioConfig, RecordType::AudioData] {
            let record = Record {
                kind,
                flags: 0,
                pts_us: 1_234,
                payload: Bytes::from_static(b"audio"),
            };
            let mut encoded = BytesMut::new();
            record.encode(&mut encoded).unwrap();
            assert_eq!(decode(&mut encoded).unwrap(), Some(record));
            assert!(encoded.is_empty());
        }
    }
}
