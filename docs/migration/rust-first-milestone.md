# Rust migration: first usable milestone

Date: 2026-09-21. Status: steps 0 and A foundation in progress; the standalone
Rust planner, QEMU capability validation, exclusive workspace, SQLite metadata,
image manifests, portable image bundles, instances, and idempotent operation
records are implemented. Process/QMP lifecycle control and the pilot gates
remain pending.

First port the existing YAML/JSON-to-QEMU launch-planning code directly to Rust.
Deliver a standalone CLI that reads the existing configuration format and emits
the resolved executable, argv and required preparation details. Then build a
daemon around that planner, add state operations and a small browser flow, and
migrate one real machine.
The [full Rust migration design](rust.md) remains the destination architecture.
This plan takes precedence for implementation order and first-milestone scope;
it does not weaken its ownership, state-integrity or recovery requirements.

The first deliverable is a **working configuration-to-argv port**. The later
runtime milestone is a **single-machine pilot**, not runtime cutover or full Python
removal. Existing Python capabilities remain available for machines that have
not migrated. Python and Rust must never own the same writable state.

## 1. Scope and constraints

The planner port covers the configuration behavior already implemented in this
checkout, using its existing tests and examples. It does not first require users
to split profiles into new model/image/preset documents. Preserve existing
supported input fields and defaults, with explicit corrections for known bugs.
New domain schemas and persistence are subsequent work.

For the runtime pilot, start with x86_64 Linux, one explicit workspace root, one PC model revision,
one prepared image and one installed, verified engine bundle. Use a small,
redistributable guest for repeatable integration tests. The selected PC fixture
must exercise a writable disk, UEFI variables and TPM state so the first state
implementation cannot accidentally become disk-only.

Use the existing analysis PC workflow as the initial model target, exposing only
the configuration needed by the selected fixtures and pilot machine. Record
unsupported settings explicitly and reject them. Do not silently discard
configuration to make an import or launch succeed.

Before implementing the pilot importer, inventory the intended real machine
read-only and confirm that its model, engine and hardware settings fit this
slice. If no machine has been supplied, complete the synthetic gates and leave
the real-machine gate open; do not claim migration evidence from a fixture.

The following scope applies to the runtime pilot after the planner port:

| Include in the runtime pilot | Defer until a concrete next use requires it |
| --- | --- |
| Model, image, instance and run IDs; pinned references | Broad model catalog and preset management |
| One workspace ID, database and exclusive root lock | Workspace discovery, registry and one daemon managing multiple roots |
| Workspace-local immutable blobs and engine references | Shared-store deduplication, offline workspace pins and cross-workspace GC |
| Resolved typed configuration saved with each instance | General hardware-profile import and host-profile collection |
| Exact installed engine identity and capability validation | Local development executable registration and portable engine import |
| Start, stop, pause, resume, guest reset, logs and recovery | Attachments, hotplug, debugger, audio and specialized device transports |
| Stopped snapshots, restore and independent clones | Image publication from snapshots |
| Fixed durable records for operations that need recovery | Generic workflow engine, plugins and configurable job graphs |
| One verified legacy layout for the pilot import | A universal importer for every historical layout |

Deferred capabilities remain in the full migration plan. Their absence must be
visible in the CLI/API; they are not silently reported as supported.

## 2. Keep the foundations small

Use the two crates proposed in the full plan, initially containing only the
configuration parser/resolver, launch planner and CLI. Add dependencies only when
used; SQLite, Axum and the async runtime are not prerequisites for generating argv.
Use typed structures for the existing configuration and plan rather than designing
the entire future domain first. Add storage and runtime modules in step A, with
abstractions only at tested external boundaries.

The following persistence and ownership foundations apply when adding execution,
not to the standalone read-only planner:

Keep workspace identity explicit in storage and service calls, but open exactly
one root per daemon. Select it explicitly when starting the daemon and connecting
the CLI. The root can later serve a global or project workspace without changing
instance identity. Do not implement workspace registration yet.

Use SQLite for metadata and private directories for blobs, generations and
staging. Record the workspace's engine dependency and do not remove it while
referenced. Do not implement shared storage or garbage collection. Clean only
staging proven unreachable by operation recovery; retain other unreferenced
objects until explicit deletion rules are implemented.

Persist intent, reserved targets, input revisions and results for asynchronous
creation/start, snapshot, restore, clone, delete and import as they are introduced.
Use explicit handlers for these operations and a bounded worker pool. Retrying
an accepted request must return its existing operation/result; a reused key with
different inputs is a conflict. Interrupted launch operations must reconcile
ownership before any retry can spawn another writer.

Use the lifecycle table and cleanup rules in the full design, including initial
QMP status/event reconciliation and helper cleanup after every QEMU exit. Keep
the instance reserved while cleanup is uncertain. These are prerequisites for
the first usable runtime, not follow-up hardening.

## 3. Delivery sequence

Each step should produce a small reviewable change or short series of changes.
Do not require the full capability inventory or complete future API before
executing the first real guest.

### 0. Port the existing YAML/JSON-to-QEMU code

Port these production boundaries directly, retaining their separation between
resolution and deterministic command generation:

| Existing source | Rust responsibility |
| --- | --- |
| `python/machineemu/documents.py` | Read JSON/YAML operator configuration |
| `python/machineemu/profiles/resolve.py` | Validate configuration; resolve engine, assets and analysis settings |
| `python/machineemu/profiles/machine.py` | Generate CPU/memory/topology, firmware, storage and TPM arguments and preparation plans |
| `python/machineemu/profiles/plan.py` | Assemble ordered QEMU argv, environment, endpoints and preparation metadata |
| `python/machineemu/engines/{manifest,registry}.py` | Validate and resolve the existing release-set/engine-bundle contract |
| `python/machineemu/domains/analysis` | Port only validation/identity transformations called by the resolver/planner |

1. Create the two-crate Cargo workspace, locked toolchain/dependencies and basic
   CI. Capture representative inputs and Python outputs from
   `tests/test_documents.py`, `test_profile_resolve.py`, `test_launch_plan.py`,
   `test_machine_arguments.py`, engine tests and relevant analysis tests.
2. Port parsing and validation of the existing schema, then read-only engine and
   asset lookup. Use existing on-disk inputs; do not introduce a database, import
   pipeline or new catalog format just to resolve them. Missing assets or engines
   produce errors, not fallback selections.
3. Port argument generation and preparation metadata. Preserve argument order,
   QEMU option escaping, deterministic analysis payload serialization, explicitly
   supplied identity, and runtime/state path resolution. Cover every currently
   implemented argument family, including appliance and PC fixtures. This does
   not claim that every catalog entry already has complete launch wiring.
4. Expose a local `machineemu plan` command accepting a configuration file,
   release-set/bundle and asset locations, target, runtime directory and state
   directory. `--json` returns the executable/argv array, environment and required
   disk/NVRAM/TPM preparation details. A readable command preview is optional;
   the argv array is authoritative and never goes through a shell.

The command reads inputs and emits a plan. It must not create writable machine
state, seed TPM/NVRAM, start helpers or spawn QEMU. Record required preparation
so that the later runtime can execute it; emitting argv alone does not imply
the referenced writable files already exist. Python is a fixture oracle during
development, never a subprocess dependency of the Rust planner.

For valid supported inputs, compare Rust and Python executable, ordered argv,
environment and preparation semantics using identical explicit paths and identity.
Include equivalent JSON/YAML inputs, defaults, overrides, comma escaping, multiple
assets and deterministic analysis payloads. Compare decoded metadata structurally;
preserve exact serialized bytes where they form a QEMU argument or external
helper contract. Do not require legacy session-manifest or product API emulation.

Do not freeze known bugs as parity requirements. In particular, report
`network.mode` versus `network.type` drift instead of silently selecting user
networking, and require explicit asset wiring instead of reproducing the unknown
asset-to-raw-disk fallback. Record each intended correction with an expected Rust
result and a focused fixture. Unsupported and ambiguous inputs must fail clearly.

**Gate:** the standalone Rust CLI generates the expected launch plans from the
existing supported configuration fixtures with no Python dependency. Every
implemented argument family has a comparison fixture or an explicitly documented
correction. Planning is deterministic and leaves input/state directories unchanged.
This gate requires neither QEMU execution nor a daemon, SQLite, new domain schemas,
HTTP API or browser. Finish and review this port before starting step A.

### A. Boot and control one persistent instance

1. Extend the existing Rust workspace with the minimal model/image/component
   schemas, validated IDs,
   errors and lifecycle states. Add one model and image-manifest fixture.
2. Implement the workspace lock, metadata worker and staged local image import.
   Verify the engine manifest and all required assets. Create persistent identity
   and complete disk/NVRAM/TPM state with recorded backing references. Preserve
   published manifest bytes and verify their digests.
3. Feed resolved instance configuration into the planner from step 0. Keep one
   argument-generation implementation as the domain evolves. Add daemon-owned
   processes, QMP, operation records, exit monitoring and startup recovery.
4. Expose a minimal `/api/v2` contract and daemon-backed CLI for import, creation,
   inspection, start/stop, pause/resume, reset, logs and operation waiting. Use
   loopback serving with explicit bearer authentication from the outset. Reject
   arbitrary server paths and shell commands. A protected local boundary handles
   engine registration and local inputs. Generate schemas for implemented routes.

**Gate:** import → create → start → pause → resume → reset → stop → start works
with real QEMU. Disk changes, NVRAM, TPM state and guest identity survive restart;
each process launch has a fresh run ID. CLI disconnect leaves the VM running.

Fake QMP/process tests must demonstrate concurrent-start exclusion, paused startup,
status/event ordering, process exit during negotiation, daemon crash recovery,
PID-reuse rejection and cleanup of surviving helpers. A second start or snapshot
must stay blocked while a writer survives or ownership is uncertain. Hash/copy
work must not block status requests, and imports must survive interrupted
publication without exposing invalid objects.

This is the first usable Rust runtime. Stop expanding infrastructure once this
gate passes; use the resulting machine to identify the next practical gaps.

### B. Protect and reproduce experiments

Implement stopped-only snapshot capture, restore into a new generation and
independent cloning. Capture and hash every writable component, including TPM
trees and NVRAM. Retain immutable backing dependencies and never use a parent's
live writable disk as clone backing.

For the initial fixture, support a tested normal-clone identity transformation.
Reject normal cloning of identity-bound or TPM-sealed state when that
transformation is unsupported. Exact-identity duplication must remain explicit.
Do not promise universal guest reprovisioning in this step.

Add reference-aware deletion for the implemented instance/snapshot resources.
Keep destructive operations under the same instance owner as lifecycle commands.

**Gate:** boot/write/stop/snapshot/modify/restore recovers disk/NVRAM/TPM contents
and matching configuration. Inject failures during copy, publication and metadata
commit: recovery retains either the old usable state or the fully committed new
generation. Parent writes, restore and deletion cannot change a clone. Retrying
an operation cannot silently create another instance or snapshot.

### C. Add the smallest useful browser flow

Connect the existing TypeScript application to the implemented Rust contract.
Provide instance creation from the available model/image, instance inspection,
start/stop/pause/resume, operation progress, logs and snapshot/restore controls.
Show only implemented capabilities. Keep CLI and browser on the same new API.

Serve production assets from the daemon. Add the bootstrap-credential exchange,
cookie, Origin and CSRF rules specified in the full design; do not rely on Vite
token injection. Port one existing display path only if needed to operate the
pilot guest, including its run-scoped tickets and viewer cleanup. Other display,
audio and debugger protocols remain deferred.

**Gate:** browser end-to-end tests exercise creation, lifecycle, snapshot/restore
and an operation failure against Rust. A production build works without Vite.
When a viewer is included, reconnect and run changes do not reuse stale tickets
or leak transports. Package the CLI, daemon, browser and required metadata from
an independent checkout and verify a fresh installation.

### D. Migrate one real machine

Implement inspect/plan/import/verify for the selected legacy layout using the
full design's offline import rules. Report unsupported layouts or settings with
specific reasons; expand this slice only if the selected pilot requires it.

Prove source QEMU/helpers are stopped before copying. Import into a separate Rust
root, preserve guest identity, rebuild and verify backing chains, and keep legacy
files intact. Make retries resumable/idempotent. Validate destination hashes and
inventory, then boot a disposable imported copy while the original remains stopped.
Verify the original guest identity and the research workflow the machine is used
for. Record the operator checks needed for that particular guest.

**Gate:** the selected machine's workflow succeeds under Rust with evidence for
state completeness, backing independence, lifecycle and recovery. Document how
to select the untouched legacy installation before new guest writes, and that
rollback afterward loses new changes unless separately exported. Do not remove
the Python runtime or claim full runtime cutover at this gate.

## 4. Evidence and review discipline

Record the fixture, engine digest, host, commands and outcomes for each gate.
Keep unit/integration tests for invariants, process-level fault injection for
recovery, and real-QEMU evidence for the complete path. Hardware-dependent
claims require real evidence; mocks establish protocol behavior only.

Normal CI starts with formatting, linting and parser/planner fixture tests.
Add schema drift checks when introducing the API and browser checks in step C.
Run the required real-engine gates for steps A–D before declaring them complete,
without requiring privileged hardware for unrelated
edits. Do not use increasing test counts or line counts as milestones.

No generic performance project is required. Verify bounded queues and streaming
disk/log processing, and confirm status responsiveness during large copies.
Investigate performance further only when measurements expose a practical issue.

## 5. After the pilot

Review the remaining full-plan capabilities against actual users and machines.
Choose the next increment by the workflow it enables: another appliance model,
project discovery, hardware profiles or a needed transport. Keep global/project
support and engine customization on the roadmap; implement shared deduplication
only when disk duplication justifies its coordination and recovery cost.

Retire Python capabilities individually only after their replacement gates pass.
Full runtime cutover and full Python removal retain the separate completion
criteria in the full design. Re-estimate that work after the pilot rather than
assigning a calendar to the entire rewrite now.
