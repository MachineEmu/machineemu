//! Shared machine definitions, launch planning, storage and process control.
//! HTTP and command-line adapters live in the machineemu application crate.
pub mod config;
pub mod domain;
pub mod engine;
mod error;
pub mod launch;
pub mod protocols;
pub mod resolution;
pub mod runtime;
pub mod storage;
pub use error::{Error, Result};
