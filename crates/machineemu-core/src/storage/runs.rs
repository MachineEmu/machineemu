use super::Workspace;
use crate::{Error, Result};
use crate::{
    domain::{Id, Run},
    runtime::process_identity_matches,
};
use rusqlite::{OptionalExtension, params};
use std::path::PathBuf;

impl Workspace {
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
