use crate::domain::{Id, Instance, Operation};
use std::{path::Path, time::Duration};

/// Validated launch inputs supplied by an instance owner.
#[cfg(unix)]
pub struct StartRequest<'a> {
    pub operation_id: Id,
    pub run_id: Id,
    pub instance_id: Id,
    pub idempotency_key: &'a str,
    pub input_json: &'a str,
    pub argv: &'a [String],
    pub qmp_socket: &'a Path,
    pub stdout: Option<&'a Path>,
    pub stderr: Option<&'a Path>,
    pub qmp_timeout: Duration,
    pub on_operation: Option<&'a (dyn Fn(&Operation) + Send + Sync)>,
    pub on_state: Option<&'a (dyn Fn(&Instance) + Send + Sync)>,
    pub complete_operation: bool,
}
