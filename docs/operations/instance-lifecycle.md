# VM instance lifecycle

An instance keeps its disk, firmware variables, TPM state, guest identity, and
resolved launch plan across runs. `create` validates and saves that plan and
prepares the writable state in staging before publishing the instance, without
launching QEMU. An interrupted staging directory is cleared on retry; a
marked directory left between filesystem publication and database commit is
replaced on retry. `start` uses the saved
plan, creates a new run ID and operation ID, and preserves writable state.

```sh
machineemu create PROFILE INSTANCE --image IMAGE
machineemu start INSTANCE
machineemu stop INSTANCE
machineemu restart INSTANCE
machineemu rm INSTANCE
```

`machineemu run PROFILE INSTANCE` combines create and start and requires a new
instance. Use `start INSTANCE` for normal restarts. `create --file instance.yaml`
and `run --file instance.yaml` accept complete configurations without a template.
`--fresh` on `run` explicitly deletes and recreates an existing instance.

`machineemu run --rm PROFILE INSTANCE` requires a new instance. The instance
document records `auto_remove` with its saved plan. The daemon deletes it after an
operator stop, guest shutdown, or QEMU exit once QEMU and helper cleanup has
completed. A failed start also removes the disposable instance when no run
remains active. If cleanup fails, the instance is retained for investigation.
Disposable instances cannot take snapshots or use `restart`. The immutable
image and other instances' state are never removed. A successful removal
leaves a small tombstone at `GET /api/v2/instances/{id}/tombstone` containing
the last run ID, removal reason, and timestamp.

The API equivalents are `POST /api/v2/instances` with `instance_id`,
`image_id`, and `launch_plan`, optionally `profile_id` (default `custom`),
`auto_remove:true`, and the resolved `profile` JSON;
`POST /api/v2/instances/{id}/start` with `{}`; and the `stop`, `restart`, and
`DELETE /api/v2/instances/{id}` routes. Start requires a launch plan in the
instance's JSON/YAML document. Inline start plans and daemon profile launch maps
are no longer accepted. Callers can still supply run and operation IDs. Start
and restart responses include the generated
`run_id` and `operation_id` alongside the instance fields. A local VNC port is
checked again at each start. For an automatically selected port, Start chooses
a currently free port and returns it as `vnc_port`.

The restart route holds the instance lock across stop and start. If the start
phase fails, the instance remains stopped with its writable state preserved. Run
history and operation records are removed with an auto-removed instance; the
tombstone is the durable removal record.

An extra digest for every source file is not required for normal restart.
Imported disk, firmware, and TPM image components already carry SHA-256
verification metadata; the resolved launch plan and profile are saved with the
instance. A future provenance feature could fingerprint mutable host
executables or helper files when exact replay and audit are required.

A daemon restart preserves successfully started VM and helper processes. QEMU
and run-owned helpers are launched through per-process `systemd-run --scope`
units when systemd is available, so stopping the daemon service does not
implicitly kill the VM scope. The new daemon verifies recorded PID/start
identity and queries QMP to restore `running` or `paused`, including when the
stored lifecycle was `error` or `starting`. An unavailable QMP socket is retried
without killing a live VM. Helpers started by this version have persisted
identities and are cleaned up with their recovered VM. Stop a VM explicitly
through its lifecycle endpoint.

Instance removal commits metadata deletion and a durable filesystem cleanup
record in one transaction. If cleanup is interrupted, retry or workspace reopen
finishes it before the instance ID can be reused.
