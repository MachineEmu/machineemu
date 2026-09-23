//! Owned child processes and instance lifecycle orchestration.
#[cfg(unix)]
mod devices;
mod lifecycle;
mod process;
#[cfg(unix)]
pub use lifecycle::{RunningInstance, StartRequest};
pub use process::{ManagedProcess, ProcessExit};
pub(crate) use process::{process_identity_matches, process_start_identity};
