# Instance lifecycle CLI

Implementation status: the create/start/stop/restart/run/rm commands, saved
launch plans, daemon-generated run IDs, `run --rm`, cleanup tombstones,
snapshot restriction, staged creation, and VNC port checks at start are
implemented. The saved configuration stores the resolved launch plan and
instance profile. Additional source digests are optional provenance work.

## Decision

Use `create` for a durable VM and reserve `init` for initializing a workspace or
project. A VM instance owns its writable disk, firmware variables, TPM state,
identity, and resolved launch configuration across multiple runs.

| Command | Meaning |
| --- | --- |
| `machineemu create PROFILE INSTANCE [options]` | Validate and save the effective VM configuration, allocate guest identity, and prepare persistent state without launching QEMU. |
| `machineemu start INSTANCE` | Start a created or stopped instance from its saved configuration. The daemon creates a new run and operation ID. |
| `machineemu stop INSTANCE` | Stop the current run and retain the instance and its writable state. |
| `machineemu restart INSTANCE` | Serialize stop then start with the same saved configuration and a new run ID. |
| `machineemu rm INSTANCE` | Delete a stopped instance and its owned writable state, subject to snapshot and cleanup checks. |
| `machineemu run PROFILE INSTANCE [options]` | Convenience command for create then start. A new instance is required; an existing instance should be started by name. |
| `machineemu run --rm PROFILE INSTANCE [options]` | Create a disposable instance, start it, and request automatic removal after its terminal run and writer cleanup. |

## Remaining hardening

The saved launch plan lets a bare `machineemu start INSTANCE` use the original
configuration after a daemon restart or profile edit. Creation stages owned
files before publication and retries recover marked interrupted publication.
The existing image assets have SHA-256 identities, so extra source digests are
not needed for ordinary restart. Fingerprints for mutable host executables and
helper files could support exact replay audits. Durability across abrupt power
loss still needs explicit directory sync and crash-injection verification.

## Required behavior

1. Create resolves the profile, image, engine, helper paths, disks, firmware,
   networking, and guest identity once. Validate the result, save the effective
   configuration and existing image asset digests, then prepare instance-owned state.
   Keep an incomplete creation recoverable rather than exposing a partially
   usable instance.
2. Start reads the saved configuration. It may resolve host availability such
   as a free VNC port, but it must not silently adopt later profile or image
   edits. Generate operation and run IDs in the daemon; accept a separate
   idempotency key for retries. A new start preserves disk, NVRAM, TPM, MAC,
   UUID, and other persistent identity.
3. Run composes create and start. During CLI migration, retain the existing
   stopped-instance reuse behavior with a clear deprecation message; switch
   new scripts to `start INSTANCE` before making an existing name a conflict.
4. Store an `auto_remove` policy only on an instance created with `run --rm`.
   Reject `--rm` for an existing instance and reject combinations with
   automatic restart or snapshots. Removal applies to an intentional stop,
   guest shutdown, or crash only after QEMU and helpers have exited and all
   instance writers are confirmed gone. If cleanup is uncertain, retain the
   instance and report the cleanup failure. Keep a bounded operation/audit
   tombstone after removal so clients can learn why the instance disappeared.
5. `rm` and auto-removal must both revoke stream tickets, close event streams,
   and remove instance-owned files only after terminal run bookkeeping. Shared
   base images and other instances' state are never removed.

## API and migration

- Extend `POST /api/v2/instances` to accept and persist a validated resolved
  configuration and optional disposable policy. Keep the existing minimal body
  temporarily for compatibility, but require a saved configuration for a bare
  start.
- Let `POST /api/v2/instances/{id}/start` generate run and operation IDs when
  omitted. Continue accepting caller-supplied IDs for current clients.
- `POST /api/v2/instances/{id}/restart` serializes stop and start under one
  per-instance lock. A future API revision can report both phases through one
  parent operation; today it returns the new start operation ID.
- Migrate the CLI into `create`, `start`, `stop`, `restart`, `run`, and `rm` while
  preserving the current `run` invocation during a transition period.

## Verification

- Create without starting; start, stop, and start again after daemon restart
  with the same disk and guest identity and distinct run IDs.
- Reject starts with no saved configuration, conflicting create options, or an
  active run without altering instance state.
- Verify `run --rm` removes state after operator stop, guest shutdown, and
  crash; preserves it when process/helper cleanup is uncertain; and never
  removes shared image content or a pre-existing instance.
- Verify snapshot, idempotency, stream ticket, and event cursor behavior across
  manual and automatic removal.
