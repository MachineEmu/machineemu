use super::Workspace;
use crate::{Error, Result};
use crate::{
    domain::{Id, Run},
    runtime::process_identity_matches,
};
use rusqlite::{OptionalExtension, params};
use std::path::PathBuf;

impl Workspace {
    pub fn save_run_helpers(
        &self,
        run_id: &Id,
        helpers: &[crate::runtime::ManagedProcess],
    ) -> Result<()> {
        let transaction = self.db.unchecked_transaction()?;
        transaction.execute(
            "DELETE FROM run_helpers WHERE run_id = ?1",
            params![run_id.as_str()],
        )?;
        for (ordinal, helper) in helpers.iter().enumerate() {
            transaction.execute("INSERT INTO run_helpers(run_id, ordinal, helper_id, pid, process_start) VALUES (?1, ?2, ?3, ?4, ?5)", params![run_id.as_str(), ordinal as i64, helper.run_id.as_str(), helper.pid, helper.process_start()?])?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn recover_run_helpers(&self, run_id: &Id) -> Result<Vec<crate::runtime::ManagedProcess>> {
        let mut statement = self.db.prepare("SELECT helper_id, pid, process_start FROM run_helpers WHERE run_id = ?1 ORDER BY ordinal")?;
        let rows = statement.query_map(params![run_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, u64>(2)?,
            ))
        })?;
        let mut helpers = Vec::new();
        for row in rows {
            let (id, pid, start) = row?;
            if process_identity_matches(pid, start) {
                helpers.push(crate::runtime::ManagedProcess::adopt(
                    Id::new("helper", id)?,
                    pid,
                    start,
                )?);
            }
        }
        Ok(helpers)
    }

    pub fn record_run(
        &self,
        run_id: Id,
        instance_id: Id,
        pid: u32,
        process_start: u64,
        qmp_socket: PathBuf,
    ) -> Result<Run> {
        self.instance(&instance_id)?;
        self.db
            .execute(
                "INSERT INTO runs(run_id, instance_id, pid, process_start, qmp_socket, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'running')",
                params![
                    run_id.as_str(),
                    instance_id.as_str(),
                    pid,
                    process_start,
                    qmp_socket.to_string_lossy().as_ref()
                ],
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(_, _) => Error::RunConflict(run_id.as_str().into()),
                other => Error::Sqlite(other),
            })?;
        self.run(&run_id)
    }

    pub fn run(&self, run_id: &Id) -> Result<Run> {
        self.db
            .query_row(
                "SELECT run_id, instance_id, pid, process_start, qmp_socket, status
                 FROM runs WHERE run_id = ?1",
                params![run_id.as_str()],
                |row| {
                    Ok(Run {
                        run_id: Id::from_stored(row.get(0)?),
                        instance_id: Id::from_stored(row.get(1)?),
                        pid: row.get(2)?,
                        process_start: row.get(3)?,
                        qmp_socket: PathBuf::from(row.get::<_, String>(4)?),
                        status: row.get(5)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| Error::NotFound {
                kind: "run",
                id: run_id.as_str().into(),
            })
    }

    pub fn reconcile_run(&self, run_id: &Id) -> Result<Run> {
        let run = self.run(run_id)?;
        let status = if process_identity_matches(run.pid, run.process_start) {
            "running"
        } else {
            "uncertain"
        };
        self.db.execute(
            "UPDATE runs SET status = ?1 WHERE run_id = ?2",
            params![status, run_id.as_str()],
        )?;
        self.run(run_id)
    }

    /// Reconcile observed QEMU state separately from requested lifecycle transitions.
    /// The active run and process identity must still match after the QMP query.
    pub fn observe_run_status(&self, run: &Run, status: &str) -> Result<crate::domain::Instance> {
        let target = crate::domain::InstanceState::from_qmp(status)?;
        if !crate::runtime::process_identity_matches(run.pid, run.process_start)
            || !self.active_run(&run.instance_id)?.is_some_and(|active| {
                active.run_id == run.run_id
                    && active.pid == run.pid
                    && active.process_start == run.process_start
            })
        {
            return Err(Error::Process(
                "QMP observation belongs to a stale or dead run".into(),
            ));
        }
        self.db.execute("UPDATE instances SET lifecycle = ?1, revision = revision + 1 WHERE instance_id = ?2 AND lifecycle != ?1", params![target.as_str(), run.instance_id.as_str()])?;
        self.instance(&run.instance_id)
    }

    pub fn reconcile_active_runs(&self) -> Result<Vec<Run>> {
        let ids = {
            let mut statement = self.db.prepare(
                "SELECT run_id FROM runs WHERE status IN ('running', 'starting', 'uncertain')",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let runs: Vec<Run> = ids
            .into_iter()
            .map(|id| self.reconcile_run(&Id::from_stored(id)))
            .collect::<Result<_>>()?;
        for run in &runs {
            if run.status == "uncertain" {
                let instance = self.instance(&run.instance_id)?;
                if matches!(
                    instance.state.as_str(),
                    "starting" | "running" | "paused" | "stopping"
                ) {
                    self.transition_instance(&run.instance_id, "error")?;
                }
            }
        }
        Ok(runs)
    }

    pub fn active_run(&self, instance_id: &Id) -> Result<Option<Run>> {
        self.db
            .query_row(
                "SELECT run_id, instance_id, pid, process_start, qmp_socket, status
                 FROM runs
                 WHERE instance_id = ?1 AND status IN ('running', 'starting', 'uncertain')
                 ORDER BY created_at DESC LIMIT 1",
                params![instance_id.as_str()],
                |row| {
                    Ok(Run {
                        run_id: Id::from_stored(row.get(0)?),
                        instance_id: Id::from_stored(row.get(1)?),
                        pid: row.get(2)?,
                        process_start: row.get(3)?,
                        qmp_socket: PathBuf::from(row.get::<_, String>(4)?),
                        status: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Error::from)
    }

    /// Return the current run only when its recorded process identity is alive.
    pub fn live_run(&self, instance_id: &Id) -> Result<Option<Run>> {
        let Some(run) = self.active_run(instance_id)? else {
            return Ok(None);
        };
        let run = self.reconcile_run(&run.run_id)?;
        Ok((run.status == "running").then_some(run))
    }

    pub fn finish_run(&self, run_id: &Id, status: &str) -> Result<Run> {
        if !matches!(status, "exited" | "failed" | "uncertain") {
            return Err(Error::Process(format!(
                "invalid terminal run status {status:?}"
            )));
        }
        self.db.execute(
            "UPDATE runs SET status = ?1 WHERE run_id = ?2",
            params![status, run_id.as_str()],
        )?;
        self.run(run_id)
    }
}
