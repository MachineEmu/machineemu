# Rust workspace

The application has two Rust packages: `machineemu-core` for planning, storage,
and runtime services, and `machineemu` for the CLI and daemon.

| Package/module | Responsibility |
| --- | --- |
| `machineemu-core::domain` | Existing IDs, persisted records and transition rules; no filesystem ownership |
| `machineemu-core::config` | Operator configuration and config-relative path resolution |
| `machineemu-core::launch` | Typed launch specification and planner conversion shared by CLI and API |
| `machineemu-core::engine` | Document loading, QEMU capability inspection, configuration validation and launch planning |
| `machineemu-core::storage` | Workspace lock/schema, blobs, image bundles, instances, operations, run records and snapshots |
| `machineemu-core::runtime` | Owned child processes and lifecycle orchestration through `StartRequest` |
| `machineemu-core::protocols` | QMP and guest-agent communication |
| `machineemu::api` | Server setup, authentication, request/response types and resource handlers |
| `machineemu::cli` | Argument dispatch, daemon client and launch workflow |
| `display-stream-protocol` | Display record framing, configuration and control messages shared by streamer and viewer |
| `display-stream` | QEMU D-Bus capture, H.264 encoding, recording and video socket |
| `machineemu-viewer` | Native display client and decoder |

Core has no dependency on the application, Axum or Clap. It uses Tokio for
asynchronous lifecycle and protocol I/O; its optional `api-schema` feature
adds Utoipa schemas to the shared launch contract. Application
binaries are thin entry points. Storage keeps its SQLite connection private;
runtime uses workspace methods instead of accessing the database directly.
Future catalog, analysis and protocol modules should be added when implemented,
rather than creating empty packages in advance.

Build both binaries before using CLI daemon auto-start:

```sh
cargo build --workspace --locked
cargo run -p machineemu -- plan --help
cargo run -p machineemu --bin machineemu-daemon -- --help
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The executables remain `machineemu` and `machineemu-daemon`. The redundant
`machineemu-plan` binary and the old planner/runtime/daemon package names have
been removed; use `cargo run -p machineemu -- plan ...` for planning.
Tests require `qemu-img` for real overlay preparation. Planner tests retain their
existing fixtures, workspace integration tests exercise the core's public
interfaces, and API tests cover authentication and routing.

Rust API v2 stream and live-device routes are documented in
[`docs/operations/api-v2-streams-devices.md`](../docs/operations/api-v2-streams-devices.md).

IDs validate during construction and deserialization. Instance lifecycle states
are enums with a shared transition table; persisted JSON/SQLite values retain
the existing lowercase strings. Lifecycle orchestration uses one async
implementation. Per-instance gates serialize mutations, while a per-run
supervisor owns the QMP connection, helpers, display process and watcher state.
Control requests and event polling share that QMP connection.

Each instance JSON/YAML file owns its configuration. Migration exports the old
SQLite profile and plan records, then drops those tables. `profile.json` is a
derived helper input. Atomic file replacement commits configuration changes;
content revisions detect direct edits as well as API edits.
Instance deletion commits its metadata changes and a cleanup record together;
retry or workspace reopen completes filesystem cleanup before an ID is reused.

A daemon restart verifies process identities and observes QMP status before
reconciling operations. Successfully published VMs and helpers survive daemon
shutdown and are adopted after restart. Temporary QMP failure leaves the live
process intact and observation retries. Run the real-QEMU restart regression
with `python3 scripts/test_daemon_recovery.py` after building the daemon. It uses
TCG and an isolated temporary workspace, and covers graceful shutdown, forced
exit, recovery of a persisted error, shared QMP control and helper cleanup.
