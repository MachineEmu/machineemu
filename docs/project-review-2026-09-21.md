# Project review — 2026-09-21

Follow-up decision: the [Rust migration plan](migration/rust.md) supersedes this
review's API-preservation recommendation. The new domain model and API are a
clean break; the findings below remain the review baseline.

Rust is a practical option for the runtime, CLI, and HTTP/WebSocket server. The
largest simplification comes from consolidating process ownership, mutable state,
and contracts first. Translating the current module structure directly would
preserve several correctness problems and add migration work.

This is an architecture and implementation review of this checkout: Python
runtime/API, profiles, catalog, assets, domain integrations, compatibility scripts,
browser client, tests, contracts, and release configuration. The separate QEMU
repository and privileged hardware behavior were not independently reviewed or
tested. No runtime implementation was changed for this review.

## Scale and existing strengths

The Python package has 71 files and 9,571 lines; Python scripts add 2,297 lines.
Python tests occupy 3,417 lines. Handwritten browser TypeScript/TSX occupies 2,938
lines, excluding generated declarations. The largest central module is `api.py`
at 1,703 lines. These are physical line counts, not measures of complexity.

Keep the existing immutable engine-bundle boundary, content-addressed asset
references, explicit instance/session distinction, deterministic launch planning,
and generated OpenAPI-to-TypeScript workflow. Firmware preparation already has a
useful recipe boundary with staged publication and digest verification. Small,
device-specific recipe files are not inherently a problem.

## Findings to address before or during migration

### 1. High: lifecycle state vocabulary disagrees across layers

`runtime/application.py:211` sends QMP `stop` for pause and then writes `paused`.
`runtime/state.py:145` rejects `paused`. A successful QMP pause therefore becomes
an application error while the manifest continues to say `running`.

A temporary probe using a fake QMP transport and the real application/store
reproduced: `QMP command accepted: stop`, `unknown session state: paused`, and
recorded state `running`. This verifies the application/store mismatch, not a
hardware QMP interaction. The API test at `tests/test_api.py:360` mocks the whole
application action, so it cannot catch it.

Use one lifecycle enum and transition policy, including starting, running,
paused, stopping, stopped, and failed. Stop, restart, removal, and reconciliation
must consume the same policy. Merely adding `paused` to the store is incomplete:
recovered stop only accepts `running`, and removal currently excludes only
`running` and `stopping`.

### 2. High: snapshots do not represent current machine state reliably

`runtime/instance.py:176` copies current file contents but copies their old
import-time hashes into the snapshot manifest. Restore recomputes those hashes
at line 248. Importing a disk, modifying it, snapshotting it, and staging restore
reproduced `snapshot digest mismatch: disk.img` with the real store.

There is also a separate inventory problem: startup seeds overlay/NVRAM/TPM
through `runtime/machine_state.py`, while snapshots enumerate only the
`state_files` recorded by import. A normally seeded machine does not automatically
get a complete snapshot inventory, including its TPM directory.

Make one instance-state inventory describe all writable components and backing
dependencies. Snapshot a quiescent instance, hash the copied bytes, and record
snapshot-time metadata. Stream hashing instead of loading an entire disk through
`read_bytes()`. Exercise boot/write/stop/snapshot/restore as one behavior.

Restore also needs explicit failure semantics: `apply_staged_restore` copies
files over live paths and adds each to its rollback list only after copying
finishes. A failed copy can leave the current file truncated and outside that
list; a process crash bypasses rollback entirely. Stage complete replacement
files and retain a recoverable restore journal or generation boundary.

### 3. High: analysis clones retain a mutable parent disk

`runtime/application.py:91` deliberately selects the source instance's writable
`overlay.qcow2`. `domains/analysis/clone.py:55` makes the clone's backing file that
same overlay. Later writes to the parent can alter data visible through the
clone; deleting or restoring the parent can invalidate it. Checking hashes during
creation does not freeze the backing image. The application also lacks a stopped
instance check here.

Clone from an immutable captured generation, or create an independent image while
the parent is stopped. Copy matching NVRAM and TPM state under the same instance
lock. Add a test that changes/restarts the parent after cloning and verifies the
clone remains stable. This finding is from source inspection, not a QEMU disk
corruption experiment.

### 4. High: process ownership and instance exclusivity are fragmented

`runtime/application.py:417` constructs a new supervisor for each start. The API
returns the PID from `RunningSession` and does not retain that session object in a
runtime owner. Stop uses the recovered-PID path even for a process started by the
same API. There is no persistent child-exit watcher in this path.

`runtime/supervisor.py:88` establishes liveness with `kill(pid, 0)`;
`stop_recovered` later signals the PID. Neither proves the process is still the
original QEMU. PID reuse can therefore reconcile or signal an unrelated process.

Start validates a session's state, but does not reserve its instance against a
second session using the same disk/NVRAM/TPM. `InstanceStore.ensure` also accepts
an existing identity without checking the requested profile against its recorded
profile. Atomic JSON replacement does not serialize a read/check/write operation
across the CLI and API.

Use a long-lived runtime manager with one active owner per instance, retained
child/QMP/helper handles, exit monitoring, and explicit recovery identity checks.
On Linux, consider pidfds for live ownership and process start-time/boot identity
checks for recovery. Serialize start/stop/snapshot/restore/clone under the same
instance boundary. If the CLI continues to mutate state directly, use an OS lock;
an in-process async lock alone is insufficient.

### 5. Medium: blocking work executes inside async request handlers

`api.py:1657` calls synchronous stop, whose implementation polls with
`time.sleep`. Startup calls subprocess and state preparation synchronously before
QMP attachment. Snapshot, restore, and clone routes also perform filesystem work
directly. These operations can stall unrelated HTTP and streaming activity in
the same event loop.

`api.py:1570` reads and splits the complete log before returning its last lines;
the response is bounded but its disk/memory work is not.

Move slow operations behind a bounded job executor and retain per-instance
serialization. Return an operation ID promptly for asynchronous operations;
restart/delete currently return 202 after doing the work inline. Use bounded
tail reading and subprocess deadlines. Rust still requires separating blocking
work from async tasks.

### 6. Medium: content-addressed publication hashes different reads

`assets/store.py:19` hashes the source, reopens it, and copies it later. If the
source changes between reads, the published bytes can disagree with their digest
name. Existing entries are accepted without rehashing. Mutable-state import has
a similar hash-then-copy pattern.

Combine copy and hashing into one streaming staging operation, verify the staged
digest, and publish without replacing a different concurrent entry. Centralize
this primitive along with atomic JSON writing, rather than independently
maintaining it in each store.

## Simplifications with the highest return

1. **One runtime manager and instance-state service.** Centralize ownership,
   lifecycle transitions, writable inventory, and recovery. This removes more
   conceptual duplication than simply merging files.
2. **Thin API routers.** Separate lifecycle, catalog, diagnostics, and transports
   from `api.py`; move resource registries into explicit services with shutdown
   cleanup. Share WebSocket task cancellation and transport cleanup, while
   retaining protocol-specific validation, frame limits, and authorization.
3. **Typed internal and public models.** Parse manifests into validated types
   once per operation. Share QMP endpoint validation/connection cleanup and the
   common session-creation path. Give public responses concrete schemas: several
   endpoints currently return generic dictionaries, and `web/src/client.ts`
   hand-defines types alongside generated ones. Keep private manifests and
   redacted public responses distinct.
4. **A single browser implementation.** `web/index.html` loads `react-main.tsx`;
   the standalone DOM application in `web/src/main.ts` has no tracked caller.
   Remove it after confirming no external consumer. Split the large React file
   by page and lazy-load display/debugger components if useful; the current
   production JS chunk is about 505.5 kB before gzip.
5. **Clear helper packaging and release behavior.** Keep compatibility daemons
   separate from the privileged namespace wrapper. The BLE tunnel imports
   `msgpack` and PyNaCl, which are absent from the declared extras; its imports
   are not exercised by the current compatibility-daemon tests. Declare a helper
   extra or an explicit separate environment. Avoid a generic plugin framework
   for three firmware recipes.
6. **One supported browser-serving/authentication path.** The browser uses an
   empty token and relies on Vite to inject authentication into proxied requests.
   The production build has no equivalent serving integration here. Define that
   deployment boundary explicitly, without embedding a long-lived token in the
   bundle. Refresh README claims about bootstrap status and unported UI features.
7. **Integration-focused validation.** Add tests across real application/store
   boundaries with fake external transports, plus opt-in QEMU/hardware checks.
   Current unit tests passing does not establish lifecycle or state parity.
   CI should include the production browser build and a documented optional
   dependency/hardware matrix.

## Rust feasibility and proposed boundary

| Area | Recommendation | Main consideration |
| --- | --- | --- |
| Lifecycle, QMP, process supervision, instance state | First Rust scope | Explicit ownership, typed states, cancellation, bounded I/O |
| API and WebSocket gateways | Migrate with runtime after contract tests | Preserve authentication, leases, binary framing, disconnect cleanup |
| Catalog, profiles, launch plans, asset/engine metadata | Good Rust fit | Preserve JSON/YAML behavior, defaults, deterministic argv and validation |
| Runtime CLI | Share the Rust runtime contract | Avoid a second writer/implementation of lifecycle rules |
| Firmware preparation and lab utilities | Keep Python initially | Specialized recipes, signing, binary formats and libsquashfs FFI make parity costly |
| Bluetooth/hwsim daemons | Consider later, independently | Need captured protocol fixtures and hardware validation first |
| React/noVNC/WebCodecs/SPICE browser code | Keep TypeScript | Already uses browser APIs; a Rust/WASM rewrite adds interop work |
| QEMU engine and board/helper crates | Preserve separate bundle boundary | Outside this checkout; existing migration ledger describes Rust work there |

Start with a small Cargo workspace: a runtime/domain library and a daemon/CLI
package, using ordinary modules internally. Do not create a crate for each
existing Python file or introduce a plugin ABI. A daemon owning lifecycle state
and a CLI talking to it is a simpler eventual ownership model.

Tokio plus Axum is a plausible server foundation: Axum supports WebSockets,
Tokio documents state ownership through a dedicated task and message passing,
and Utoipa can derive OpenAPI schemas. These capabilities support the proposed
design; they do not establish behavioral compatibility automatically. Keep
blocking disk/FFI work outside async request tasks.

- [Axum WebSocket documentation](https://docs.rs/axum/latest/axum/extract/ws/)
- [Tokio shared-state design](https://tokio.rs/tokio/tutorial/shared-state)
- [Utoipa schema derivation](https://docs.rs/utoipa/latest/utoipa/derive.ToSchema.html)

No measured Python CPU bottleneck was established in this review. Expected Rust
benefits are ownership clarity, stronger models, and distribution of the runtime
as a native executable. It will not remove QEMU, swtpm, image utilities, firmware
dependencies, hardware tests, or the need to design crash recovery.

## Migration sequence and acceptance gates

1. Correct and document lifecycle, snapshot, clone, and publication semantics.
   Add regression tests for the reproduced bugs and parent/clone independence.
2. Freeze representative profile/launch-plan, manifest, HTTP, and WebSocket
   fixtures. Include unknown fields, error responses, authentication, lease
   expiry, partial frames, cancellation, and restart recovery. Existing OpenAPI
   alone cannot describe WebSocket behavior.
3. Implement Rust profile resolution and launch planning first, comparing outputs
   with Python against shared fixtures. Add the runtime owner and state service
   with fake process/QMP tests and an opt-in real QEMU smoke.
4. Implement the existing API surface on that runtime. Keep the TypeScript client
   and exercise it against both implementations. Preserve status codes, response
   schemas and security checks intentionally, rather than merely regenerating
   types until compilation succeeds.
5. Switch one disposable test instance at a time. Python and Rust must never
   concurrently own the same instance. Version any manifest changes; retain a
   compatible reader or backup for rollback. Keep Python firmware tooling behind
   its existing artifact/CLI boundary instead of adding an in-process FFI layer.
6. Remove the old Python runtime/API only after parity. Reassess firmware/helper
   migration separately, based on operational benefit and available fixtures.

This is a moderate runtime rewrite with substantial integration validation, not
a syntax conversion. A full Python removal is technically possible but has a
weaker immediate return than migrating the runtime while retaining research and
firmware tooling.

## Verification performed

- `uv run --extra api --extra test --extra lab python -m pytest -q`: 183 passed;
  one Starlette/AnyIO deprecation warning.
- `bun test`: 11 passed.
- `bun run build`: passed, including TypeScript checking; chunk-size warning.
- OpenAPI consistency and release-set checks: passed.
- Regenerated TypeScript API declarations: no tracked diff.
- Temporary probes reproduced pause persistence failure and snapshot digest
  failure without changing production code or adding permanent tests.

No real QEMU lifecycle, privileged host USB, Bluetooth/hwsim, audio hardware,
performance benchmark, or migration prototype was run. Findings identified as
source inspection should be validated with focused integration tests before
implementation changes are declared complete.
