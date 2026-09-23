use super::ManagedProcess;
use super::process_identity_matches;
use crate::domain::{Instance, Run};
#[cfg(unix)]
use crate::protocols::qmp::QmpClient;
use crate::{Error, Result, domain::Id, storage::Workspace};
#[cfg(unix)]
use std::{path::Path, time::Duration};
use std::{thread, time::Instant};

#[cfg(unix)]
pub struct RunningInstance {
    process: Option<ManagedProcess>,
    pub qmp: QmpClient,
    pub run_id: Id,
    recovered: Option<(u32, u64)>,
}

#[cfg(unix)]
impl RunningInstance {
    pub fn is_recovered(&self) -> bool {
        self.recovered.is_some()
    }

    pub fn recover(run: &Run) -> Result<Self> {
        if !process_identity_matches(run.pid, run.process_start) {
            return Err(Error::Process("recorded process is no longer alive".into()));
        }
        let qmp = QmpClient::connect(&run.qmp_socket, Duration::from_secs(2))?;
        Ok(Self {
            process: None,
            qmp,
            run_id: run.run_id.clone(),
            recovered: Some((run.pid, run.process_start)),
        })
    }

    pub fn pause(&mut self) -> Result<()> {
        self.qmp.execute("stop", serde_json::Value::Null)?;
        Ok(())
    }

    pub fn resume(&mut self) -> Result<()> {
        self.qmp.execute("cont", serde_json::Value::Null)?;
        Ok(())
    }

    pub fn reset(&mut self) -> Result<()> {
        self.qmp.execute("system_reset", serde_json::Value::Null)?;
        Ok(())
    }
}

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
}

impl Workspace {
    #[cfg(unix)]
    pub fn recover_instance_run(&self, instance_id: &Id) -> Result<Option<RunningInstance>> {
        let Some(run) = self.active_run(instance_id)? else {
            return Ok(None);
        };
        let run = self.reconcile_run(&run.run_id)?;
        if run.status != "running" {
            self.finish_run(&run.run_id, "failed")?;
            let instance = self.instance(instance_id)?;
            if matches!(
                instance.state.as_str(),
                "starting" | "running" | "paused" | "stopping"
            ) {
                self.transition_instance(instance_id, "error")?;
            }
            return Ok(None);
        }
        RunningInstance::recover(&run).map(Some)
    }

    #[cfg(unix)]
    pub fn start_instance(&self, request: StartRequest<'_>) -> Result<RunningInstance> {
        let StartRequest {
            operation_id,
            run_id,
            instance_id,
            idempotency_key,
            input_json,
            argv,
            qmp_socket,
            stdout,
            stderr,
            qmp_timeout,
        } = request;
        if let Some(active) = self.active_run(&instance_id)? {
            let reconciled = self.reconcile_run(&active.run_id)?;
            if reconciled.status == "running" {
                return Err(Error::ActiveRun(instance_id.as_str().into()));
            }
            self.finish_run(&active.run_id, "failed")?;
            let instance = self.instance(&instance_id)?;
            if matches!(
                instance.state.as_str(),
                "starting" | "running" | "paused" | "stopping"
            ) {
                self.transition_instance(&instance_id, "error")?;
            }
        }
        let operation = self.begin_operation(
            operation_id,
            instance_id.clone(),
            "start",
            idempotency_key,
            input_json,
        )?;
        if operation.status != "accepted" {
            return Err(Error::Process(format!(
                "start operation {} is already {}",
                operation.operation_id.as_str(),
                operation.status
            )));
        }
        let cleanup_run_id = run_id.clone();
        let mut recorded = false;
        let started = (|| -> Result<RunningInstance> {
            self.transition_instance(&instance_id, "starting")?;
            let mut process = match ManagedProcess::spawn(run_id.clone(), argv, stdout, stderr) {
                Ok(process) => process,
                Err(error) => {
                    let _ = self.transition_instance(&instance_id, "error");
                    return Err(error);
                }
            };
            let process_start = match process.process_start() {
                Ok(value) => value,
                Err(error) => {
                    let _ = process.terminate();
                    let _ = process.wait();
                    let _ = self.transition_instance(&instance_id, "error");
                    return Err(error);
                }
            };
            self.record_run(
                run_id.clone(),
                instance_id.clone(),
                process.pid,
                process_start,
                qmp_socket.to_owned(),
            )?;
            recorded = true;
            let deadline = Instant::now() + qmp_timeout;
            let mut qmp = loop {
                match QmpClient::connect(qmp_socket, qmp_timeout.min(Duration::from_millis(250))) {
                    Ok(client) => break client,
                    Err(error) if Instant::now() < deadline => {
                        if process.try_wait()?.is_some() {
                            let _ = self.finish_run(&run_id, "failed");
                            let _ = self.transition_instance(&instance_id, "error");
                            return Err(error);
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => {
                        let _ = process.terminate();
                        let _ = process.wait();
                        let _ = self.finish_run(&run_id, "failed");
                        let _ = self.transition_instance(&instance_id, "error");
                        return Err(error);
                    }
                }
            };
            if let Err(error) = self.transition_instance(&instance_id, "running") {
                let _ = qmp.execute("quit", serde_json::Value::Null);
                let _ = process.terminate();
                let _ = process.wait();
                let _ = self.finish_run(&run_id, "failed");
                return Err(error);
            }
            self.complete_operation(&operation.operation_id, r#"{"state":"running"}"#)?;
            Ok(RunningInstance {
                process: Some(process),
                qmp,
                run_id,
                recovered: None,
            })
        })();
        if let Err(error) = &started {
            let _ = self.fail_operation(&operation.operation_id, &error.to_string());
            if recorded {
                let _ = self.finish_run(&cleanup_run_id, "failed");
            }
            if let Ok(instance) = self.instance(&instance_id)
                && matches!(instance.state.as_str(), "starting" | "running")
            {
                let _ = self.transition_instance(&instance_id, "error");
            }
        }
        started
    }

    #[cfg(unix)]
    pub fn pause_instance(
        &self,
        instance_id: &Id,
        running: &mut RunningInstance,
    ) -> Result<Instance> {
        let current = self.instance(instance_id)?;
        if current.state != "running" {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: "paused".into(),
            });
        }
        running.pause()?;
        self.transition_instance(instance_id, "paused")
    }

    #[cfg(unix)]
    pub fn resume_instance(
        &self,
        instance_id: &Id,
        running: &mut RunningInstance,
    ) -> Result<Instance> {
        let current = self.instance(instance_id)?;
        if current.state != "paused" {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: "running".into(),
            });
        }
        running.resume()?;
        self.transition_instance(instance_id, "running")
    }

    #[cfg(unix)]
    pub fn reset_instance(&self, instance_id: &Id, running: &mut RunningInstance) -> Result<()> {
        let current = self.instance(instance_id)?;
        if current.state != "running" && current.state != "paused" {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: "running".into(),
            });
        }
        running.reset()
    }

    #[cfg(unix)]
    pub fn stop_instance(
        &self,
        instance_id: &Id,
        running: &mut RunningInstance,
    ) -> Result<Instance> {
        let current = self.instance(instance_id)?;
        if current.state != "running" && current.state != "paused" && current.state != "stopping" {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: "stopped".into(),
            });
        }
        if current.state != "stopping" {
            self.transition_instance(instance_id, "stopping")?;
        }
        let result = running.qmp.execute("quit", serde_json::Value::Null);
        let status = if let Some(process) = running.process.as_mut() {
            if result.is_err() {
                let _ = process.terminate();
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            let exit = loop {
                if let Some(exit) = process.try_wait()? {
                    break exit;
                }
                if Instant::now() >= deadline {
                    let _ = process.terminate();
                    break process.wait()?;
                }
                thread::sleep(Duration::from_millis(20));
            };
            if exit.success { "exited" } else { "failed" }
        } else if let Some((pid, start)) = running.recovered {
            result?;
            let deadline = Instant::now() + Duration::from_secs(10);
            while process_identity_matches(pid, start) {
                if Instant::now() >= deadline {
                    return Err(Error::Process(
                        "recovered process did not exit after QMP quit".into(),
                    ));
                }
                thread::sleep(Duration::from_millis(20));
            }
            "exited"
        } else {
            return Err(Error::Process("run has no process owner".into()));
        };
        self.finish_run(&running.run_id, status)?;
        if status == "failed" {
            self.transition_instance(instance_id, "error")
        } else {
            self.transition_instance(instance_id, "stopped")
        }
    }
}
