use super::{ManagedProcess, StartRequest, process_identity_matches};
use crate::{
    Error, Result,
    domain::{Id, Instance, Run},
    protocols::async_qmp::AsyncQmp,
    storage::Workspace,
};
use serde_json::Value;
use std::time::{Duration, Instant};

/// A live VM controlled through an asynchronous QMP connection.
pub struct AsyncRunningInstance {
    process: Option<ManagedProcess>,
    pub qmp: AsyncQmp,
    pub run_id: Id,
    recovered: Option<(u32, u64)>,
}

impl AsyncRunningInstance {
    pub fn abort_owned_child(&mut self) -> Result<()> {
        let process = self.process.as_mut().ok_or_else(|| {
            Error::Process("cannot abort an adopted process without verified signaling".into())
        })?;
        if process.try_wait()?.is_none() {
            process.terminate()?;
            process.wait()?;
        }
        Ok(())
    }

    pub fn poll_exit(&mut self) -> Result<Option<super::ProcessExit>> {
        if let Some(process) = self.process.as_mut() {
            return process.try_wait();
        }
        if let Some((pid, start)) = self.recovered
            && !process_identity_matches(pid, start)
        {
            return Ok(Some(super::ProcessExit {
                code: None,
                success: false,
            }));
        }
        Ok(None)
    }

    pub fn is_recovered(&self) -> bool {
        self.recovered.is_some()
    }

    pub async fn recover(run: &Run) -> Result<Self> {
        if !process_identity_matches(run.pid, run.process_start) {
            return Err(Error::Process("recorded process is no longer alive".into()));
        }
        Ok(Self {
            process: None,
            qmp: AsyncQmp::connect(&run.qmp_socket).await?,
            run_id: run.run_id.clone(),
            recovered: Some((run.pid, run.process_start)),
        })
    }

    pub async fn pause(&mut self) -> Result<()> {
        self.qmp.execute("stop", Value::Null).await.map(|_| ())
    }
    pub async fn resume(&mut self) -> Result<()> {
        self.qmp.execute("cont", Value::Null).await.map(|_| ())
    }
    pub async fn reset(&mut self) -> Result<()> {
        self.qmp
            .execute("system_reset", Value::Null)
            .await
            .map(|_| ())
    }
}

impl Workspace {
    pub async fn recover_instance_run_async(
        &mut self,
        instance_id: &Id,
    ) -> Result<Option<AsyncRunningInstance>> {
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
        AsyncRunningInstance::recover(&run).await.map(Some)
    }

    pub async fn start_instance_async(
        &mut self,
        request: StartRequest<'_>,
    ) -> Result<AsyncRunningInstance> {
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
            on_operation,
            on_state,
            complete_operation,
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
        let existing_operation = self.operation(&operation_id).is_ok();
        let operation = self.begin_operation(
            operation_id,
            instance_id.clone(),
            "start",
            idempotency_key,
            input_json,
        )?;
        if !existing_operation && let Some(notify) = on_operation {
            notify(&operation);
        }
        if operation.status != "accepted" {
            return Err(Error::Process(format!(
                "start operation {} is already {}",
                operation.operation_id.as_str(),
                operation.status
            )));
        }
        let cleanup_operation_id = operation.operation_id.clone();
        let cleanup_run_id = run_id.clone();
        let cleanup_instance_id = instance_id.clone();
        let mut recorded = false;
        let recorded_ref = &mut recorded;
        let workspace = &mut *self;
        let started = async move {
            let starting = workspace.transition_instance(&instance_id, "starting")?;
            if let Some(notify) = on_state {
                notify(&starting);
            }
            let mut process = match ManagedProcess::spawn(run_id.clone(), argv, stdout, stderr) {
                Ok(process) => process,
                Err(error) => {
                    let _ = workspace.transition_instance(&instance_id, "error");
                    return Err(error);
                }
            };
            let process_start = match process.process_start() {
                Ok(value) => value,
                Err(error) => {
                    let _ = process.terminate();
                    let _ = process.wait();
                    let _ = workspace.transition_instance(&instance_id, "error");
                    return Err(error);
                }
            };
            workspace.record_run(
                run_id.clone(),
                instance_id.clone(),
                process.pid,
                process_start,
                qmp_socket.to_owned(),
            )?;
            *recorded_ref = true;
            let deadline = Instant::now() + qmp_timeout;
            let mut qmp = loop {
                match AsyncQmp::connect(qmp_socket).await {
                    Ok(client) => break client,
                    Err(error) if Instant::now() < deadline => {
                        if process.try_wait()?.is_some() {
                            return Err(error);
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => {
                        let _ = process.terminate();
                        let _ = process.wait();
                        return Err(error);
                    }
                }
            };
            let observed = qmp
                .execute("query-status", Value::Null)
                .await
                .and_then(|value| {
                    value
                        .get("status")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| Error::Qmp("query-status response has no status".into()))
                });
            let target = match observed {
                Ok(value) if value == "running" => "running",
                Ok(value) if matches!(value.as_str(), "paused" | "prelaunch") => "paused",
                Ok(value) => {
                    let _ = process.terminate();
                    let _ = process.wait();
                    return Err(Error::Qmp(format!(
                        "unsupported initial QEMU status {value}"
                    )));
                }
                Err(error) => {
                    let _ = process.terminate();
                    let _ = process.wait();
                    return Err(error);
                }
            };
            if process.try_wait()?.is_some() {
                return Err(Error::Process("QEMU exited during QMP negotiation".into()));
            }
            let final_state = match workspace.transition_instance(&instance_id, target) {
                Ok(instance) => instance,
                Err(error) => {
                    let _ = qmp.execute("quit", Value::Null).await;
                    let _ = process.terminate();
                    let _ = process.wait();
                    return Err(error);
                }
            };
            if let Some(notify) = on_state {
                notify(&final_state);
            }
            if complete_operation {
                let completed = workspace.complete_operation(
                    &operation.operation_id,
                    &serde_json::json!({"state":target}).to_string(),
                )?;
                if let Some(notify) = on_operation {
                    notify(&completed);
                }
            }
            Ok(AsyncRunningInstance {
                process: Some(process),
                qmp,
                run_id: run_id.clone(),
                recovered: None,
            })
        }
        .await;
        if let Err(error) = &started {
            if let Ok(failed) = self.fail_operation(&cleanup_operation_id, &error.to_string())
                && let Some(notify) = on_operation
            {
                notify(&failed);
            }
            if recorded {
                let _ = self.finish_run(&cleanup_run_id, "failed");
            }
            if let Ok(instance) = self.instance(&cleanup_instance_id) {
                if matches!(instance.state.as_str(), "starting" | "running") {
                    if let Ok(failed_state) =
                        self.transition_instance(&cleanup_instance_id, "error")
                        && let Some(notify) = on_state
                    {
                        notify(&failed_state);
                    }
                } else if instance.state == "error"
                    && let Some(notify) = on_state
                {
                    notify(&instance);
                }
            }
        }
        started
    }

    pub async fn pause_instance_async(
        &mut self,
        instance_id: &Id,
        running: &mut AsyncRunningInstance,
    ) -> Result<Instance> {
        let current = self.instance(instance_id)?;
        if current.state != "running" {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: "paused".into(),
            });
        }
        running.pause().await?;
        self.transition_instance(instance_id, "paused")
    }

    pub async fn resume_instance_async(
        &mut self,
        instance_id: &Id,
        running: &mut AsyncRunningInstance,
    ) -> Result<Instance> {
        let current = self.instance(instance_id)?;
        if current.state != "paused" {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: "running".into(),
            });
        }
        running.resume().await?;
        self.transition_instance(instance_id, "running")
    }

    pub async fn reset_instance_async(
        &mut self,
        instance_id: &Id,
        running: &mut AsyncRunningInstance,
    ) -> Result<()> {
        let current = self.instance(instance_id)?;
        if current.state != "running" && current.state != "paused" {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: "running".into(),
            });
        }
        running.reset().await
    }

    pub async fn stop_instance_async(
        &mut self,
        instance_id: &Id,
        running: &mut AsyncRunningInstance,
    ) -> Result<Instance> {
        let current = self.instance(instance_id)?;
        if !matches!(current.state.as_str(), "running" | "paused" | "stopping") {
            return Err(Error::InvalidTransition {
                from: current.state,
                to: "stopped".into(),
            });
        }
        if current.state != "stopping" {
            self.transition_instance(instance_id, "stopping")?;
        }
        let result = running.qmp.execute("quit", Value::Null).await;
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
                tokio::time::sleep(Duration::from_millis(20)).await;
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
                tokio::time::sleep(Duration::from_millis(20)).await;
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
