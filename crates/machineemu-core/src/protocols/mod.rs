//! External guest and engine protocols.
#[cfg(unix)]
pub mod async_qmp;
#[cfg(unix)]
pub mod guest_agent;
#[cfg(unix)]
pub mod qmp;
