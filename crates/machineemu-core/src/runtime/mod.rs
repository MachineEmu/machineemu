//! Owned child processes and instance lifecycle orchestration.
#[cfg(unix)]
mod devices;
mod lifecycle;
#[cfg(unix)]
mod lifecycle_async;
mod process;
#[cfg(unix)]
pub use lifecycle::StartRequest;
#[cfg(unix)]
pub use lifecycle_async::AsyncRunningInstance;
pub use process::{ManagedProcess, ProcessExit};
pub(crate) use process::{process_identity_matches, process_start_identity};
