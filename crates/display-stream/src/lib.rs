//! QEMU D-Bus display capture and H.264 streaming.

pub mod frame;
pub mod gpu_bridge;
pub mod listener;

pub use display_stream_protocol::{Record, RecordType, VideoConfig, decode};
