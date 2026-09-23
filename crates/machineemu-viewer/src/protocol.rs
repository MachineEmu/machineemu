//! Shared display wire format and viewer protocol checks.

pub use display_stream_protocol::{
    AudioConfig, Control, Cursor, MAX_CLIPBOARD_BYTES, RecordType, VideoConfig, decode, message,
};

/// Playback conversion for the native GStreamer viewer.
pub trait AudioConfigExt {
    fn caps(&self) -> Option<String>;
    fn gain(&self) -> f64;
}

impl AudioConfigExt for AudioConfig {
    /// GStreamer raw audio caps for this format, or `None` if unsupported.
    fn caps(&self) -> Option<String> {
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
    fn gain(&self) -> f64 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BufMut;
    use bytes::BytesMut;
    use display_stream_protocol::{MAX_PAYLOAD, ProtocolError};

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
