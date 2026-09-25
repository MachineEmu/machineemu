use super::{Workspace, digests::hex_digest};
use crate::domain::{Id, Operation};
use crate::{Error, Result};
use rusqlite::{OptionalExtension, params};

impl Workspace {
    /// Resolve operations interrupted by a daemon restart after run and snapshot
    /// records have been reconciled. This is safe to repeat.
    pub fn reconcile_operations(&self) -> Result<Vec<Operation>> {
        let mut statement = self.db.prepare(
            "SELECT operation_id FROM operations WHERE status = 'accepted' ORDER BY created_at, operation_id",
        )?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut changed = Vec::new();
        for id in ids {
            let operation = self.operation(&Id::from_stored(id))?;
            let completed = match operation.kind.as_str() {
                "start" => {
                    let instance = self.instance(&operation.instance_id)?;
                    if matches!(instance.state.as_str(), "running" | "paused")
                        && self
                            .active_run(&operation.instance_id)?
                            .is_some_and(|run| run.status == "running")
                    {
                        Some(serde_json::json!({"state": instance.state}).to_string())
                    } else {
                        None
                    }
                }
                "snapshot" => operation
                    .idempotency_key
                    .strip_prefix("snapshot:")
                    .and_then(|id| Id::new("snapshot", id).ok())
                    .filter(|id| self.snapshot(id).is_ok())
                    .map(|id| serde_json::json!({"snapshot_id": id.as_str()}).to_string()),
                _ => None,
            };
            changed.push(match completed {
                Some(result) => self.complete_operation(&operation.operation_id, &result)?,
                None => self.fail_operation(&operation.operation_id, "interrupted_by_recovery")?,
            });
        }
        Ok(changed)
    }

    pub fn active_operations(&self, instance_id: &Id) -> Result<Vec<Operation>> {
        let mut statement = self.db.prepare(
            "SELECT operation_id FROM operations WHERE instance_id = ?1 AND status IN ('accepted', 'queued', 'running') ORDER BY operation_id LIMIT 64",
        )?;
        let ids = statement
            .query_map(params![instance_id.as_str()], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| self.operation(&Id::from_stored(id)))
            .collect()
    }

    pub fn begin_operation(
        &self,
        operation_id: Id,
        instance_id: Id,
        kind: &str,
        idempotency_key: &str,
        input_json: &str,
    ) -> Result<Operation> {
        self.instance(&instance_id)?;
        let input_sha256 = hex_digest(input_json.as_bytes());
        let existing: Option<(String, String, String, String, Option<String>)> = self
            .db
            .query_row(
                "SELECT operation_id, kind, input_sha256, status, result_json
             FROM operations WHERE instance_id = ?1 AND idempotency_key = ?2",
                params![instance_id.as_str(), idempotency_key],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        if let Some((existing_id, existing_kind, existing_hash, status, result_json)) = existing {
            if existing_kind != kind || existing_hash != input_sha256 {
                return Err(Error::OperationConflict {
                    key: idempotency_key.to_owned(),
                });
            }
            return Ok(Operation {
                operation_id: Id::from_stored(existing_id),
                instance_id,
                kind: existing_kind,
                idempotency_key: idempotency_key.to_owned(),
                status,
                result_json,
            });
        }
        self.db.execute(
            "INSERT INTO operations(operation_id, instance_id, kind, idempotency_key, input_sha256, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'accepted')",
            params![operation_id.as_str(), instance_id.as_str(), kind, idempotency_key, input_sha256],
        )?;
        Ok(Operation {
            operation_id,
            instance_id,
            kind: kind.to_owned(),
            idempotency_key: idempotency_key.to_owned(),
            status: "accepted".into(),
            result_json: None,
        })
    }

    pub fn complete_operation(&self, operation_id: &Id, result_json: &str) -> Result<Operation> {
        let changed = self.db.execute(
            "UPDATE operations SET status = 'completed', result_json = ?1 WHERE operation_id = ?2",
            params![result_json, operation_id.as_str()],
        )?;
        if changed == 0 {
            return Err(Error::NotFound {
                kind: "operation",
                id: operation_id.as_str().to_owned(),
            });
        }
        self.operation(operation_id)
    }

    pub fn fail_operation(&self, operation_id: &Id, reason: &str) -> Result<Operation> {
        let result_json = serde_json::json!({"error": reason}).to_string();
        let changed = self.db.execute(
            "UPDATE operations SET status = 'failed', result_json = ?1 WHERE operation_id = ?2 AND status = 'accepted'",
            params![result_json, operation_id.as_str()],
        )?;
        if changed == 0 {
            return Err(Error::Process(format!(
                "operation {} is not accepted",
                operation_id.as_str()
            )));
        }
        self.operation(operation_id)
    }

    pub fn operation(&self, operation_id: &Id) -> Result<Operation> {
        self.db
            .query_row(
                "SELECT operation_id, instance_id, kind, idempotency_key, status, result_json
             FROM operations WHERE operation_id = ?1",
                params![operation_id.as_str()],
                |row| {
                    Ok(Operation {
                        operation_id: Id::from_stored(row.get(0)?),
                        instance_id: Id::from_stored(row.get(1)?),
                        kind: row.get(2)?,
                        idempotency_key: row.get(3)?,
                        status: row.get(4)?,
                        result_json: row.get(5)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| Error::NotFound {
                kind: "operation",
                id: operation_id.as_str().to_owned(),
            })
    }
}
