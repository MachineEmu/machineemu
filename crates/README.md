# Rust workspace

The workspace follows the two-package boundary in
[the Rust migration plan](../docs/migration/rust.md). The
[first usable milestone](../docs/migration/rust-first-milestone.md) still controls
implementation order.

| Package/module | Responsibility |
| --- | --- |
| `machineemu-core::domain` | Existing IDs, persisted records and transition rules; no filesystem ownership |
| `machineemu-core::config` | Operator configuration and config-relative path resolution |
| `machineemu-core::engine` | Document loading, QEMU capability inspection, configuration validation and launch planning |
| `machineemu-core::storage` | Workspace lock/schema, blobs, image bundles, instances, operations, run records and snapshots |
| `machineemu-core::runtime` | Owned child processes and lifecycle orchestration through `StartRequest` |
| `machineemu-core::protocols` | QMP and guest-agent communication |
| `machineemu::api` | Server setup, authentication, request/response types and resource handlers |
| `machineemu::cli` | Argument dispatch, daemon client and launch workflow |

Core has no dependency on the application, Axum, Clap or Tokio. Application
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

This refactor preserves current configuration, database and wire formats. It does
not complete the domain or runtime migration. IDs and status strings still need
the destination domain contract. Planning still resolves existing on-disk inputs.
Lifecycle handlers run in a bounded blocking pool. Per-instance locks serialize
mutations, and attached database connections let long QMP waits proceed without
holding the workspace owner's mutex. A daemon restart reconciles persisted runs;
live runs can reconnect to QMP on their next lifecycle action. QMP event
reconciliation, durable helper recovery, and a single core-owned runtime service
remain future work. Existing snapshot behavior is not
evidence that the full state-generation gates pass. No real-machine pilot gate
is established by this structural change.
