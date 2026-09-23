# Rust migration plan

Date: 2026-09-21. Status: migration in progress; planner and runtime foundations
are implemented. The full-migration gates below remain acceptance criteria.

For initial delivery, follow the [first usable milestone](rust-first-milestone.md).
It takes precedence for first-milestone scope and implementation order. This
document defines the destination architecture and full-migration gates; completing
all of it is not a prerequisite for piloting one machine under Rust.
Begin with a direct port of the existing YAML/JSON configuration resolver and
QEMU argument generator, before new domain schemas, persistence or daemon work.
Existing authored configuration is the initial planner input; the product API
still has no backward-compatibility requirement.

This plan replaces the Python implementation with Rust using the domain model
in [domain-model.md](../domain-model.md): **device model, image, instance, run**.
Its gates cover lifecycle ownership, coherent snapshots, independent clones,
nonblocking operations and verified content-addressed publication.

The HTTP API, WebSocket control protocol, CLI, catalog layout, and local metadata
formats may change. **There is no API backward-compatibility requirement.** Do
not build aliases, proxy adapters, dual-write code, or old response emulation.
The browser and CLI move to the new contract together. Preserving valuable
existing machine state is a separate, offline data-import concern.

## 1. Outcomes and scope

The product is an AI-operated local research lab: quickly start, stop, inspect
and experiment with VMs and exotic device firmware for reverse engineering.
It evolves the shared-base/project-local-state workflow in `vmmanager-sh` with
explicit device-model, image, instance and run definitions. Production fleet
management is not the target; correctness protects experiments and their baselines.

Deliver a Linux-first Rust daemon and CLI, with the existing TypeScript browser,
that create instances from device models and immutable images, own their runs,
and manage coherent persistent state. Retain the separate QEMU engine-bundle
release boundary. The initial supported host remains x86_64 Linux; emulated
guest targets remain those actually advertised and validated by installed bundles.

There are two explicit completion milestones:

1. **Runtime cutover:** Rust owns catalog/image import, instances, execution,
   snapshots, clones, API, streaming gateways, analysis integration and CLI.
   Python firmware preparation and compatibility daemons may temporarily run as
   separate tools behind documented artifact/protocol boundaries.
2. **Full production migration:** firmware recipes, compatibility daemons and
   necessary operator tooling are Rust too. Remove Python production packages,
   entry points and runtime dependencies. TypeScript, QEMU, swtpm, system image
   utilities and privileged host setup are not rewritten merely to remove them.

No new plugin ABI, distributed control plane, live migration, RAM snapshots,
Windows/macOS host support, Rust browser rewrite, or QEMU rewrite is part of this
migration. Experimental hardware features remain explicitly experimental until
their validation gates pass; do not silently drop their implementations.

## 2. Domain contract

### 2.1 Resources and identifiers

| Resource | Identity and essential fields | Rules |
| --- | --- | --- |
| `DeviceModel` | `device_model_id`, revision, manifest digest, architecture, hardware constraints, component slots, engine requirements | Published revisions immutable; no installed OS, host paths or per-instance serials |
| `Image` | Manifest digest, name/version metadata, compatible model revisions, component manifests, provenance | Digest identifies exact contents; name/tag is only a lookup aid |
| `Instance` | `instance_id`, name, pinned model/image references, configuration revision, guest identity, active state-generation reference | Durable across runs; owns every writable component |
| `Run` | `run_id`, `instance_id`, config revision, state-generation reference, engine build digest, state, timestamps, exit/failure information | One process launch; no reuse of IDs on restart |
| `Snapshot` | `snapshot_id`, source instance/configuration, immutable state manifest and backing references | Captures stopped machine state; not a public image by default |
| `Operation` | `operation_id`, kind, target IDs, state, progress, result/error, request key | Durable tracking of long-running mutations |
| `Attachment` | Attachment ID, type, scope, approved resource reference, lease state | Explicit instance configuration or run-scoped resource |

Use newtyped IDs and validated digest types in Rust, not interchangeable strings.
Model IDs are stable catalog slugs; instance/run/operation IDs are server-generated
opaque UUIDs. Human names are separate from IDs and filesystem paths. Resolve
model revisions and image tags before creation, then persist exact references.

Use `device_model_id`, `image_digest`, `instance_id`, and `run_id` consistently in
API payloads, logs, Rust types, UI state and tests. Reserve `device` without the
`model` qualifier for a named peripheral context such as host USB. Remove
`session_id` from the new product contract. Guest login sessions and browser
authentication are unrelated to a run.

### 2.2 Models, images and presets

- A model defines valid hardware configurations and named component slots, such
  as `system_disk`, `firmware_code`, `firmware_vars`, `spi`, or `tpm_state`.
- An image binds those slots to immutable blobs or tree manifests, with explicit
  initialization: `read_only`, `copy`, or `qcow2_overlay`. Do not infer wiring
  from a filename or silently attach an unknown blob as a raw disk.
- An image component is a tagged union for a file or a directory tree. Tree
  entries have safe relative paths, digests, lengths and permitted metadata;
  preserve required TPM state structure without following arbitrary symlinks.
- A preset chooses a model, optional image and configuration defaults. It has no
  lifecycle state. `malware-analysis-x64` becomes an analysis preset over an x86
  model; `udm-pro-lab` becomes a UDM Pro model selection plus lab defaults.
- Restricted firmware remains outside the source repository. Catalog metadata
  can describe an unavailable image requirement; instance creation requires all
  selected components to be present and verified.
- Configuration resolution applies model defaults, then preset defaults, then
  the selected hardware configuration profile, then explicit instance settings.
  Validate the complete result against model/image
  requirements and authoritative operator policy. Policy denies cannot be
  overridden by presets or user configuration.
- Separate `supported` hardware features from `available` host/runtime features
  and `allowed` policy decisions. Return stable reason codes for unavailable
  actions, rather than scattering booleans across model and run manifests.

Instance creation generates guest identity once. No shared catalog seed should
give every created machine the same UUID/MAC/serials. Imported machines preserve
their identity. Reset-to-image, normal cloning, and exact-identity duplication
have separate policies for identity, NVRAM and TPM consistency.

### 2.3 Run state and exclusive ownership

Implement one transition table shared by storage, runtime and API:

```text
starting -> running | paused
running <-> paused
starting/running/paused -> stopping
stopping -> stopped | failed
starting -> failed                       # failure before any child was spawned
```

`failed` and `stopped` are terminal for that run. A new start creates another run.
Guest reset uses the existing run; instance restart stops the old run and creates
a new one. Record whether shutdown was graceful, forced, guest-initiated, or due
to a host/runtime failure; intentional forced termination need not be confused
with an unexplained emulator crash.

Once children have been spawned, every exit path, including guest shutdown,
startup failure and emulator crash, passes through `stopping` while owned
children and state-writing helpers are cleaned up. Publish a terminal state and
release instance exclusion only after all writers are confirmed exited. If that
cannot be established, retain the reservation and set `recovery_required`;
record the observed QEMU exit and cleanup failure without declaring the run
terminal. `stopping` describes cleanup as well as an operator-requested stop.

An instance exposes its active run and derived execution status. Do not keep a
second mutable `running` flag on the instance. Recovery uncertainty is an
instance management condition (`recovery_required`) that blocks mutations; it
must not pretend the instance is stopped.

The daemon is the sole writer of managed state. A workspace-root lock prevents two
daemons from opening the same state root. A per-instance command owner serializes
start, stop, configuration mutation, snapshot, restore, clone capture and delete.
A database uniqueness constraint also prevents two nonterminal runs for an
instance. Conflicting mutations return `instance_busy` rather than starting a
second job. Read-only status and unrelated instances remain responsive.

### 2.4 Global and project workspaces

Support two storage scopes with the same domain types and lifecycle operations:

| Scope | Definition and mutable state | Typical use |
| --- | --- | --- |
| Global | User-level workspace under `$XDG_STATE_HOME/machineemu` (default `~/.local/state/machineemu`) | Persistent background lab machines, shared research services |
| Project | Versionable `machineemu.yaml` plus ignored `./.machineemu/` state | Repository-specific firmware exploration and repeatable experiments |

A workspace is a storage and name-resolution boundary, not another device model
or VM type. Each workspace owns its metadata database, instance generations,
snapshots, run history and artifacts. Project state must be real local state,
not just a project manifest pointing at mutable instances in the global workspace.
Names are unique within a workspace; instance and run UUIDs remain opaque IDs.

The CLI resolves an explicit `--project <directory>` or `--global` first; otherwise
it discovers the nearest ancestor `machineemu.yaml`, falling back to global scope.
An invalid discovered project is an error, not a reason to silently use global
state. Include the resolved workspace in structured results. Project manifests
declare named instance specifications using model/image references and typed
configuration; keep runtime IDs, credentials and mutable state out of that file.
Persist resolved digests locally so subsequent starts do not follow moved tags.

A user daemon can manage the global workspace and registered project workspaces.
Open each root under its own exclusive lock and use its database as authoritative;
a user-level workspace registry records locations, not duplicate lifecycle state.
Canonicalize roots to avoid opening one workspace twice through path aliases.
Registration uses a protected local CLI boundary; browser/API requests select
registered workspace IDs and cannot open arbitrary host paths. APIs carry explicit
workspace context; the browser can select a workspace or list registered ones.

Storage scope is independent of execution lifetime. Both global and project runs
continue after CLI/AI/viewer disconnect unless explicitly stopped. Closing an
editor or leaving a directory does not stop a project VM. Automatic cleanup is
an explicit disposable-experiment option; workspace scope never implies deletion.

Models, immutable image blobs and engine bundles may be shared to avoid repeated
downloads. Record durable workspace pins before using shared backing objects;
an offline, unregistered or missing project is not evidence that its pins are
unused. Release pins through explicit workspace removal, never just disappearance.
Project-local state is not automatically a portable archive: export must include
required blobs and rewrite verified backing references. Moving a stopped project
requires explicit relocation/re-registration; copying it requires clone/import
semantics for new IDs and guest identity. Reject moving an active workspace.

### 2.5 Dedicated hardware configuration profiles

Models can expose dedicated, typed configuration sections for their research
capabilities. Preserve the customization represented by
`unifi-qemu/config/malware-analysis-x64.yaml` and `windows11-pro-tpm.yaml`: CPU
policy/topology, memory, SMBIOS, ACPI identity, PCI properties, storage/display/USB
descriptors and sensors. The model defines what can be configured and which
engine capabilities implement it; a hardware configuration profile supplies
reusable values. An image supplies boot contents, not these hardware choices.
An analysis preset can select a model, image and hardware configuration profile
together without becoming a separate lifecycle owner.

Allow a dedicated YAML/JSON file selected from a project manifest or supplied
through the CLI for a global instance. Resolve project-relative references from
the project root, not the daemon's working directory. Local files are imported
through the protected local boundary; API clients submit bounded content or an
already imported digest. Profiles have a schema version, compatible model and
engine requirements, and a digest of their normalized contents. Initially support
one selected hardware profile plus explicit overrides, rather than arbitrary
include chains or executable configuration.

`analysis-profile host-profile` output is a supported source for this import,
through an explicit schema mapping and the installed helper's versioned JSON
boundary. Inspect the actual helper schema; do not assume its output is already
the new profile format. Retain source digest, helper version and selected values
as provenance. Reuse its validation/identity contract where applicable instead of
reimplementing divergent algorithms. Host collection is explicit preparation,
not a hidden probe on every start. Other models can have their own typed sections;
they need not accept PC analysis fields.

Validate the resolved combination before launch, including CPU/topology coherence
and engine support for requested properties. Keep hardware descriptions separate
from per-instance UUID/MAC/serial identity: a reusable host-derived profile must
not silently give every instance the captured host's identity. Exact identity
reproduction remains an explicit policy. Persist the resolved configuration and
profile digest in the instance revision; runs pin that revision. Editing the
source file affects existing instances only through explicit stopped-instance
reconfiguration, with NVRAM/TPM and identity consistency checks. Expose the
effective configuration and provenance for AI inspection.

### 2.6 Model-level QEMU version and path pins

A published device-model revision may pin an exact QEMU version and, for patched
or reproducibility-sensitive models, an exact engine-bundle build digest. Keep
the upstream QEMU version, QEMU machine type/version and bundle digest distinct:
two patched builds can report the same upstream version but implement different
hardware. Required targets, machine types and helper/protocol capabilities remain
mandatory even when the version matches. Models without an exact pin declare
explicit validated compatibility constraints; never assume any installed QEMU works.

Also support a model-specific executable path for local/custom QEMU development
builds. A project or user-local model binding can select an absolute path or a
project-relative path; resolve the latter against the project root, never daemon
CWD or shell expansion. Portable published model manifests retain version and
capability requirements; machine-specific executable paths live in the local
binding. This allows a model to use a checkout build without requiring a released
bundle. Path selection must satisfy any declared version/build/capability pins;
conflicts are errors, not fallback instructions. Register executable paths through
the protected local CLI boundary, not unrestricted remote API fields.

For a path-selected engine, capture the canonical path, executable digest,
reported version and required runtime dependency/helper references as a local
engine record. Validate the actual executable and required capabilities. A path
is a locator, not an immutable build identity: verify it before start and report
`engine_changed` or `engine_unavailable` if it was rebuilt, moved or removed.
Accepting a rebuilt binary is explicit stopped-instance reconfiguration. For
reproducible runs, import the executable and required runtime assets into a
managed immutable bundle; executable hashing alone does not pin dynamic libraries
or other external dependencies of an in-place development build.

Resolve the model requirements and any additional image/hardware-profile constraints
to one installed bundle or registered local engine at instance creation, and save
its build identity in the instance
configuration revision. Each run records that exact selection. Missing pinned
bundles produce a structured unavailable reason before launch; never silently
substitute the system QEMU, another patch build or a newer installed version.
Presets and instance overrides cannot relax a model pin. Install bundles side by
side so global and project instances can use different versions concurrently.

Changing a published model pin requires a new model revision. Moving an existing
instance to that revision or another compatible bundle is explicit stopped-instance
reconfiguration with state compatibility validation; installing a bundle alone
does not upgrade instances. Snapshots retain the required configuration/bundle
reference, and engine removal must respect instance/snapshot dependencies.

## 3. Rust workspace and dependency boundaries

The workspace now uses two packages and ordinary modules. See
[the current crate map](../../crates/README.md) for implemented boundaries and
remaining work. The tree below describes the destination layout:

```text
Cargo.toml
Cargo.lock
rust-toolchain.toml
crates/
  machineemu-core/
    src/
      domain/          # typed models, IDs, compatibility, configuration, errors
      catalog/         # model/image/preset manifests
      storage/         # database, blobs, state generations, snapshots, imports
      engine/          # exact bundle resolution and launch planning
      runtime/         # instance owners, process/QMP lifecycle, recovery, jobs
      protocols/       # QMP, RFB/video/audio/helper framing, GDB MI
      analysis/        # identity, observations, environment and host inspection
  machineemu/
    src/
      api/             # public DTOs, routes, auth, WebSockets, OpenAPI
      cli/             # daemon client and explicitly offline commands
      bin/
        machineemud.rs
        machineemu.rs
web/                   # React/TypeScript, generated types from the new API
catalog/
  device-models/
  presets/
contracts/
  openapi.json
  fixtures/
```

`machineemu-core::domain` has no server or filesystem ownership. The launch planner
is a deterministic transformation of validated domain inputs. Runtime code owns
effects; routes only authenticate, validate requests, invoke services and map
errors. Introduce traits at real external boundaries (processes, QMP transport,
clock, filesystem failure injection), not an interface for every struct.

Add a separate `machineemu-firmware` package in the firmware phase to isolate
binary parsing and native-library bindings. Compatibility helper binaries may
live in the application package with shared protocol modules; split another
crate only if independent packaging or privilege boundaries require it.

Use Tokio for async process/socket work, Axum for HTTP/WebSockets, Serde for
typed serialization, Utoipa for OpenAPI generation, Clap for the CLI, tracing for
structured diagnostics, and rusqlite for local metadata. Use a maintained YAML
parser for operator inputs selected and pinned during the scaffold phase; stored
manifests and wire formats are JSON. Add hashing, IDs and OS-interface libraries
only where required. Keep cryptography behind the later firmware package.

Pin a supported stable Rust toolchain and minimum Rust version when scaffolding;
commit the lockfile and use workspace dependency declarations. Do not preselect
unverified future versions in this plan. Keep Axum/OpenAPI annotations on public
DTOs rather than coupling persistent records to HTTP schemas.

## 4. Persistence and machine state

### 4.1 Storage ownership

Use one SQLite metadata database per workspace plus files for large data.
`state-root` is the global workspace or a project's `.machineemu/` directory;
`artifact-root` is workspace-local. `blob-root` may be a shared immutable store,
and `runtime-root` is a user runtime directory with workspace/run-scoped sockets:

```text
state-root/
  workspace.lock
  machineemu.sqlite
  instances/<instance_id>/generations/<generation_id>/...
  staging/<operation_id>/...
blob-root/
  sha256/<digest>
artifact-root/
  runs/<run_id>/...                  # logs, reports, screenshots
runtime-root/
  workspaces/<workspace_id>/...
  runs/<run_id>/...                  # sockets and process ownership evidence
```

Tables cover model revisions, images/tags/components, instances/config revisions,
state generations, snapshots, runs, operations and reference edges. Store typed,
versioned JSON for configuration bodies where relational querying is unnecessary.
Foreign keys and uniqueness constraints protect relationships. A dedicated
database worker performs short transactions; do not share one connection across
async handlers or hold a transaction while copying disks.

Use SQLite transactions for metadata only. They do not atomically commit a VM
disk or rename a directory. Choose local-filesystem durability settings, enable
foreign keys, configure a bounded busy timeout, and use a supported backup
procedure. Exported JSON manifests are immutable artifacts, not competing mutable
databases. Version database migrations and reject unsupported newer schemas.

### 4.2 Publication and crash recovery

Every multi-file mutation has a durable operation record and explicit steps:

1. Reserve the target under its instance owner and record intended inputs.
2. Build output in a private staging directory on the destination filesystem.
3. Stream copy/hash the actual output bytes; validate component completeness,
   lengths, backing dependencies and model constraints.
4. Flush files and directories as required and atomically publish a complete
   generation or content-addressed entry without overwriting unrelated data.
5. Commit metadata references and the operation result in one short transaction.
6. Release ownership and clean unreachable staging only when recovery is safe.

Persist enough intent that a restart distinguishes uncommitted staging, published
but unreferenced output, and a committed operation awaiting cleanup. Reconcile
idempotently. Do not claim that database and filesystem form one transaction.
An error after commit must not roll back by deleting now-referenced output.

Hash while copying to staging. Verify existing digest entries before trusting
them; make published blobs read-only under the supported operator model. Asset
publication, instance imports and snapshots share this primitive. File-based
backing references use stable managed paths and explicit digest dependencies.

### 4.3 State generations, snapshots and clones

An instance points to one current writable generation. Only its active run may
write it. A snapshot is an immutable captured generation, not a rename of a
directory QEMU is still using. The complete inventory includes every writable
disk, firmware-variable store, flash/EEPROM component and TPM tree.

Initially, capture is stopped-only. Confirm QEMU and state-writing helpers have
exited, then copy/hash a coherent inventory. Stream large files and preserve
sparse files where supported. Use reflinks only as an optimization with a tested
copy fallback; never hard-link writable state into a snapshot.

Restore creates and verifies a new writable generation and then commits the
instance's generation/configuration reference. Never copy a partial restore over
live files. The old generation remains available until commit/recovery completes.
This also avoids the current per-file rollback gap.

Clone from a snapshot. Initially favor independent captured disks for simplicity;
allow overlays only over immutable managed backing generations with reference
tracking. Test parent restart, modification, restore and deletion after cloning.

Normal clones get new guest identity only where the model/image supports a
coherent transformation. TPM-sealed or identity-bound state may need reseeding
and guest reprovisioning; reject unsupported combinations rather than silently
breaking them. Exact-identity duplication is explicit and marked as such. Image
publication from a snapshot is a separate operation with identity/secret handling,
not a synonym for cloning.

Garbage collection marks references from images, instances, snapshots, runs that
pin replay inputs, and in-flight operations. It deletes only unreachable objects
under a coordinated publication/GC boundary. Begin with an explicit dry-run CLI
command; automatic background GC is not required for cutover.

## 5. Runtime and process lifecycle

One daemon owns per-instance command tasks and a bounded blocking-I/O job pool.
An active run owns QEMU, QMP, swtpm and attached helper handles, cancellation
scopes, leases and transport tasks. Filesystem hashing/copying, SQLite work and
native FFI must not monopolize async workers. Blocking jobs need explicit
cancellation checkpoints; dropping an async wrapper is not proof they stopped.

Start must validate and reserve the instance, resolve an exact compatible engine,
materialize complete state, persist a `starting` run, spawn helpers/QEMU, validate
QMP, and reconcile initial execution state before publishing `running` or
`paused`. QMP connectivity alone does not establish guest execution. Query
`query-status` and serialize its response with QMP events through the run owner:
events preceding the response are superseded by that status observation; events
following it must not be overwritten by startup completion. Map `running` to
`running` and `paused`/`prelaunch` to `paused`; do not automatically resume a
paused guest. Other initial QEMU statuses fail startup through owned cleanup
with the observed status recorded. An observed process exit takes precedence
over any pending startup response and must never be overwritten by it.
Every failure path cleans owned children, reaps them and records whether
writable state is safe to reuse. Reject arbitrary shell commands; construct
argv without a shell and use an environment allowlist.

Retain a child-exit watcher throughout the run. QMP events update guest execution
state; exit observation records the outcome and begins the same cleanup path as
Stop, including for guest shutdown and emulator crashes. Use command IDs,
absolute deadlines, bounded frames/event queues and cancellation-aware QMP
request handling. A flood of unrelated events must not extend a command timeout
indefinitely.

Stop sends the selected graceful request, waits for a bounded interval, then
escalates under verified ownership. On every exit path, reap owned children and
confirm state-writing helpers have exited before publishing a terminal run or
marking the instance available. Bound helper cleanup and retain instance exclusion
with `recovery_required` if a writer's exit cannot be established. A viewer
disconnect never kills the VM. Daemon shutdown has an explicit default of
draining/stopping owned runs; an unexpected daemon crash is handled through
recovery, not assumed cleanup.

Recovery acquires exclusive daemon ownership before serving mutations. Verify
boot identity, PID start time, executable identity, run-owned pidfile/socket
evidence and QMP endpoint before adopting surviving processes. Use pidfds where
available for stable signaling after validation. Neither `kill(pid, 0)` nor
`kill_on_drop` alone establishes safe recovery. Account for adopted processes
that cannot be reaped as direct children.

Persist run intent before spawn and write ownership evidence as early as possible.
Test the crash window between spawning QEMU and recording its PID. If ownership
cannot be established, retain the instance reservation as `recovery_required`;
never signal an unverified PID or start another writer. Recovery also invalidates
old viewer tickets/leases and reconciles interrupted operations according to
their actual committed state.

## 6. New API, authentication and CLI

Use `/api/v2` and `/ws/v2` to make the clean break unambiguous. Do not serve the
old `/devices`, `/sessions`, `/catalog/sessions` or profile-based launch routes.

### 6.1 Resource endpoints

| Method and route | Behavior |
| --- | --- |
| `GET /api/v2/health` | Readiness, including whether startup recovery is complete |
| `GET /api/v2/device-models` | List models and revisions, hardware support and availability |
| `GET /api/v2/device-models/{id}/revisions/{revision}` | Retrieve a published model definition |
| `GET /api/v2/images?device_model_id=...` | List image revisions and compatibility/availability |
| `GET /api/v2/images/{digest}` | Image manifest and provenance |
| `POST /api/v2/image-imports` | Begin bounded upload/import; verify and publish an image |
| `GET /api/v2/presets` | Optional creation defaults, without lifecycle state |
| `POST /api/v2/instance-validations` | Resolve creation inputs and report structured errors, without mutation |
| `POST /api/v2/instances` | Create durable identity/configuration; return 201 for metadata creation |
| `GET /api/v2/instances` | Instance directory, active run, management condition and available actions |
| `GET/PATCH /api/v2/instances/{id}` | Inspect/update allowed configuration; stopped-only initially |
| `DELETE /api/v2/instances/{id}` | Delete a stopped instance through a tracked operation |
| `POST /api/v2/instances/{id}/runs` | Start a new run; return a tracked operation |
| `POST /api/v2/instances/{id}/restart` | Stop active run and start a new run as one serialized operation |
| `GET /api/v2/instances/{id}/runs` | Execution history |
| `GET /api/v2/runs/{id}` | Recorded and current execution state |
| `POST /api/v2/runs/{id}/{stop,pause,resume,reset}` | Explicit run actions; braces denote four concrete routes |
| `GET /api/v2/runs/{id}/{logs,environment,hardware,screenshot}` | Bounded, redacted diagnostics; four concrete routes |
| `GET/POST /api/v2/instances/{id}/snapshots` | List or capture stopped state |
| `POST /api/v2/instances/{id}/restore` | Restore selected compatible snapshot |
| `POST /api/v2/instances/{id}/reset-to-image` | Explicit state replacement, distinct from guest reset |
| `DELETE /api/v2/snapshots/{id}` | Delete only when backing/reference policy permits |
| `POST /api/v2/snapshots/{id}/clones` | Create an instance with explicit identity policy |
| `GET /api/v2/operations/{id}` | Durable progress/result/error |
| `POST /api/v2/runs/{id}/viewer-tickets` | Short-lived, single-use, channel-scoped authorization |
| `GET/POST/DELETE /api/v2/runs/{id}/attachments[...]` | Typed, allowlisted peripheral operations |
| `WS /ws/v2/runs/{id}/{channel}` | Run-scoped transport with explicit channel protocol |

Define a separate operator/host inventory route for approved USB resources and
host capabilities. Host paths and arbitrary backend executable arguments are not
browser inputs. The CLI uploads local files or uses a protected local import
boundary; an API body is never an unrestricted server-side path reader.

Creation inputs contain a pinned model reference, image digest, name and typed
configuration overrides. Output contains `instance_id` and configuration revision;
it does not invent a run before start. Long mutations return 202 with
`operation_id`, a status URL and affected resource IDs as available.

### 6.2 Contract rules

- Stable error envelope: `code`, safe `message`, field errors, `request_id`, and
  relevant resource/operation IDs. Do not expose raw filesystem exceptions.
- Use 404 for absent resources, 409 for state/ownership conflicts, 422 for invalid
  domain inputs and 503 for unavailable required host services.
- Support idempotency keys for creation and long mutations. Store a request
  fingerprint with the result; the same key and different input is a conflict.
- Configuration updates use an expected revision to prevent lost edits. Long
  jobs pin that revision when accepted.
- Operation states: `queued`, `running`, `succeeded`, `failed`, `cancelled`.
  Define cancellable steps; irreversible commit sections complete reconciliation
  before reporting cancellation. Browser disconnect does not cancel a job.
- Paginate instance/image/run histories; cap payloads, logs, queued commands,
  channel buffers and per-principal resources.
- Generate OpenAPI from Rust DTOs and TypeScript from OpenAPI. Check generated
  files into the repository and verify drift in CI. Document WebSocket envelopes
  and binary fixtures separately; OpenAPI alone is insufficient.

### 6.3 Authentication and browser delivery

The daemon serves the built browser and API on the same loopback origin in the
initial release. The browser exchanges an explicitly supplied operator bootstrap
credential for a short-lived HttpOnly, SameSite authentication cookie; protect
mutations with exact Origin validation and a CSRF token. Do not embed a permanent
API token into JavaScript or pass it in URLs. Specify Secure-cookie behavior for
TLS deployments and the loopback HTTP development case.

The CLI uses explicit bearer authentication or the protected local control
endpoint. It does not need browser Origin semantics because its credentials are
not ambient cookies. WebSockets require a matching Origin and a single-use ticket
bound to principal, run, channel and permissions. Lease claim/renew/release is
independent of connection authorization. Define expiry and revocation tests.

Vite remains optional development tooling and no longer provides the only token
injection path. Remote/network exposure and multi-user authorization are separate
deployment features, not implicit consequences of binding a different host.

### 6.4 CLI commands

```text
machineemu workspace init|list|register|relocate|remove
machineemu device-model list|inspect
machineemu image list|inspect|import|prepare
machineemu instance create|list|inspect|configure|start|stop|restart|delete
machineemu instance reset-to-image
machineemu run list|inspect|pause|resume|reset|logs
machineemu snapshot create|list|restore|clone|delete
machineemu operation inspect|wait
machineemu analysis host-inspect|observe|acpi-dump|guard-status|guard-load-command
machineemu migrate inspect|plan|import|verify
machineemu storage verify|gc
```

Operational commands call the daemon; they do not open the live database or write
instance files themselves. Offline migration, backup/verification and recipe
preparation are explicit separate modes with installation exclusion where needed.
Provide `--json` output and consistent exit codes. A start can wait on its
operation without turning the CLI into another process supervisor.

All workspace-scoped commands accept `--global` or `--project <directory>` using
the discovery rules above. `workspace init` creates the project definition and
ignored state directory. Registration removal releases shared pins only after
explicit disposition of dependent instances/snapshots; it never silently deletes
project data. Retain CLI/AI access to background project runs from outside their
directory through explicit workspace selection.

## 7. Feature migration inventory

Track each row with fixture references, implementation PR, unit/integration
evidence and hardware status. API redesign is allowed; capability loss must be
explicitly documented rather than hidden by a successful build.

| Current source | Rust destination / disposition | Required evidence |
| --- | --- | --- |
| `documents`, `catalog`, `profiles` | Model/image/preset parsing and typed creation/launch planning | JSON/YAML inputs, compatibility rejection, deterministic argv, policy precedence |
| `engines`, release scripts | Bundle registry/verification | Pinned digest, executable hashes, target/helper availability, no sibling checkout |
| `assets`, `instance`, `machine_state`, `migration` | Blob store, generations, snapshots and offline importer | Hash/copy integrity, full state inventory, crash injection and backing chains |
| `process`, `supervisor`, `state`, `qmp` | Instance owner, run lifecycle/recovery and QMP transport | Concurrent-start exclusion, pause persistence, exit watcher, PID reuse, cancellation |
| `operations`, application services | Durable jobs and typed use cases | Retry/idempotency, recovery, nonblocking progress |
| `api`, `api_server`, runtime CLI | Axum API, daemon, CLI | New resource flows, authorization, errors, browser/CLI integration |
| `terminal`, VNC, external VNC, framed video | Run-scoped transport services | Fragmentation, limits, leases, disconnect cleanup, external-VNC auth |
| `audio`, `spice_audio` | Run-scoped audio registry/proxy | Playback/capture, lease takeover, framing and cleanup |
| `gdb` | Shared per-run debugger service | MI parsing, command multiplexing, child cleanup and reconnect |
| Host USB, CD-ROM/network hotplug, remote devices | Typed attachments and allowed QMP operations | Ownership, failure rollback, bounded ticketed proxy, host-backed checks |
| Front panel, LCD/touch, hwsim/Bluetooth controls | Typed model capabilities and run channels | Protocol fixtures, semantic input validation, helper unavailable reasons |
| `domains/analysis` | Rust analysis integration | Persistent identity, environment provenance, observations, ACPI, guard command/status |
| `domains/unifi/firmware` | Isolated firmware package, after runtime cutover | Container/FIT/CPIO/GPT/SPI/eMMC fixtures, signatures, SquashFS metadata and failure tests |
| `scripts/compat` | Rust helper binaries; privileged namespace wrapper remains explicit | H4/BLE/netlink traces, simulated controls and hardware acceptance |
| `web/src` | Retain TS transport implementations; replace resource model/client/pages | Model/image creation flow, instances/runs, real component lifecycle tests |

Preserve QEMU/helper wire contracts where the installed engine requires them.
The permission to break the product API does not change an external engine's
protocol. Coordinate any engine protocol change through manifest capabilities
and exact bundle versions.

## 8. Implementation phases and gates

Every phase ends with reviewable artifacts and an executable gate. Fix the known
bugs in the new implementation and record expected semantic changes; do not
spend the migration reproducing old bugs for parity or require a preliminary
rewrite of the entire Python codebase.

### P0 — Freeze vocabulary and inventory

Deliver the domain schemas, lifecycle transition table, component-role registry,
feature ledger and failure cases for lifecycle ownership, snapshot consistency,
clone independence and blob publication. Record current test results as a
reference; test counts are not acceptance criteria for Rust.

Split the two checked-in profiles on paper into models, presets, image
requirements and operator settings. Identify drift such as `network.mode` versus
`network.type`; conversion must be deliberate, not silently default to user
networking. Inventory existing data roots read-only if they are supplied for an
actual migration. Use synthetic fixtures otherwise.

**Gate:** every production capability and persistent file category has a named
owner and planned disposition. No new schema uses `device_id` to mean profile
or `session_id` to mean instance.

### P1 — Workspace, domain types and new contract

Create the two-package workspace, locked dependencies, formatter/lint configuration,
test harness and initial schema export command. Implement validated IDs/digests,
model/image/component/configuration types, public errors and lifecycle enums.
Define the v2 API and TypeScript generation against small in-memory fixtures.

Resolve JSON/YAML parsing policy, unknown-field rejection for authored config,
schema versions and deterministic JSON serialization for hashed manifests.
Define hashing over the exact published canonical manifest bytes and preserve
those bytes; reject duplicate keys and ambiguous/nonfinite numeric encodings.

**Gate:** fixtures round-trip, invalid references/roles/paths are rejected,
OpenAPI/types regenerate without drift, domain code is independent of HTTP.

### P2 — Metadata, blob store and image import

Implement SQLite schema/migrations, per-workspace database workers/root locks,
workspace discovery/registration and shared-store pin records,
operation records and reference constraints. Implement staged streaming uploads,
hash verification, file/tree manifests and safe publication. Add model/image
catalog APIs and CLI import/inspection. Port engine-manifest validation.

**Gate:** a source changed during import cannot be published under the wrong
digest; duplicate concurrent imports are safe; restart at each publication step
leaves either a valid committed object or recoverable staging. Missing restricted
assets are reported before instance creation. No QEMU is required for this gate.
Two projects can use the same instance name without sharing mutable state;
explicit global selection overrides project discovery. Missing project roots
retain shared-store pins, and path aliases cannot bypass workspace exclusion.

### P3 — Instance creation and launch planning

Implement preset resolution, compatibility checks, configuration revisions,
persistent identity generation and complete state-component inventories.
Materialize disk overlays, NVRAM and TPM state using bounded external tools and
typed plans. Pin exact model/image references and validate engine requirements.

Generate launch plans from typed slots, not nested arbitrary dictionaries. Port
analysis CPU/SMBIOS/ACPI/sensor/PCI properties and deterministic network wiring.
Record private resolved launch inputs separately from public diagnostics.

**Gate:** two instances from one image have distinct identity and writable state;
reopening one preserves both. Golden launch fixtures cover PC and each appliance
model. Unknown component kinds, incompatible image/model pairs, and forbidden
network/USB requests fail before spawning anything.
Hardware-profile fixtures cover imported `analysis-profile` output, explicit
override precedence, unsupported model/engine properties and stable saved values
after source edits. Two instances sharing a profile retain distinct generated
identities; restart preserves each instance's resolved hardware configuration.
Engine-selection fixtures cover exact version/build pins, two bundles with the
same upstream version, missing pins, incompatible overrides and side-by-side
models. Installing a newer bundle must not change an existing instance's engine.
Path-selection fixtures cover project-relative resolution, version mismatches,
missing/replaced executables and explicit acceptance of a rebuilt local engine.

### P4 — First complete runtime slice

Implement daemon instance owners, durable run/operation state, child/helper
ownership, QMP, start/stop/pause/resume/reset/restart, exit monitoring and startup
recovery. Add the new lifecycle API and daemon-backed CLI.

Use a small known engine/guest fixture for a real launch, while fake processes
and QMP peers drive all failure and race cases. Startup/readiness must not depend
on browser connectivity.

**Gate:** create instance → start → pause → resume → guest reset → stop → start
preserves identity/state and yields the correct run IDs. Simultaneous starts
produce one owner. Killing/restarting the daemon neither launches a second writer
nor signals a reused PID. Slow operations do not stall health/status requests.
QMP fixtures cover initial running, paused/prelaunch and unsupported/error states,
STOP/RESUME events on either side of the initial status response, and process
exit during negotiation; startup must not overwrite a later event or exit.
Guest shutdown, startup failure and emulator crash fixtures leave a state-writing
helper alive while another start or snapshot is requested: both remain blocked
until cleanup completes. Cleanup timeout or uncertain helper ownership retains
`recovery_required` across daemon restart without releasing the reservation.
Exercise both global and project instances. CLI exit leaves background runs
controllable; daemon recovery reopens registered roots without duplicate writers.

### P5 — Snapshots, restore, clones and deletion

Implement generation publication and stopped-only coherent capture. Add restore,
reset-to-image, clone policies, reference-aware deletion and GC dry-run. Wire
tracked operation APIs and CLI commands.

**Gate:** boot/write/stop/snapshot/modify/restore returns the complete expected
disk/NVRAM/TPM state. Restore failure leaves a usable old generation. Snapshot
hashes describe captured bytes. A clone stays unchanged after parent writes,
restore or deletion. No active run or unknown recovery condition permits capture
or destructive state replacement.

### P6 — Diagnostics, transports and attachment parity

Port bounded logs, screenshots, hardware/environment reports, UART, VNC/video,
front panel/LCD, GDB, SPICE audio, remote-device tickets and hotplug operations.
Port compatibility control clients while Python daemons temporarily provide the
same external helper protocols.

Create one transport-lifetime abstraction for paired pumps, cancellation,
deadlines and cleanup. Keep protocol-specific codecs and authorization decisions
explicit. Multi-step hotplug gets compensating cleanup when later QMP steps fail.

**Gate:** fixture-driven fragmented/malformed/oversized frames, stalled peers,
viewer disconnect, lease expiry/takeover and run termination all release their
resources. Test real display/debugger where available. Hardware-dependent cases
have named evidence or an explicit experimental status; mocked success is not
reported as host validation.

### P7 — Browser and production serving

Replace the profile/session navigation with Models, Images, Instances and run
history. Creation selects a model and compatible image, shows missing assets,
then creates a durable instance. Start returns operation progress; views bind to
the resulting run. Show instance state separately from execution history.

Regenerate the client and remove handwritten duplicate DTOs and the unused
`web/src/main.ts` DOM application. Retain working noVNC, WebCodecs and SPICE
browser codecs. Bind effects, tickets and lease teardown to `run_id` changes.
Implement same-origin production assets and browser authentication in the daemon.

**Gate:** browser end-to-end tests cover creation, start/stop/restart, snapshots,
restore and operation failure against Rust, plus viewer unmount/reconnect.
Production build works without the Vite token-injecting proxy. No UI/API code
uses legacy session/profile launch routes.

### P8 — Offline import and runtime cutover

Implement the read-only inventory/plan/import/verify workflow below. Package
daemon, CLI, web assets and model/preset metadata from an independent checkout.
Update release-set semantics, installation instructions and service lifecycle.

**Gate:** a disposable copy of legacy state imports with matching identity,
component hashes and verified backing chains; legacy roots remain unchanged.
A fresh installation also works with no importer. Run the clean-break browser
and CLI against Rust only. Disable the Python API and lifecycle entry points;
retain only explicitly separated preparation/helper tools for the next phases.

This is the **runtime cutover milestone**, not full Python removal.

### P9 — Firmware preparation and analysis-tool completion

Port firmware inspection/preparation in bounded steps: container/FIT parsing,
CPIO editing, GPT/SPI/eMMC generation, per-model recipes, signing and SquashFS.
Retain strict stage/verify/publish semantics and external asset provenance.
Use generated/bounded synthetic binary fixtures plus permitted known-good test
inputs; do not commit restricted firmware.

Keep libsquashfs bindings behind a narrow audited unsafe module if a safe
equivalent does not preserve required metadata. Do not replace it with a parser
that loses xattrs, inode details or deterministic recipe behavior. Use established
crypto implementations and independently verify signing/encryption vectors.
Preserve explicit lab/diagnostic modes and secret redaction.

Complete host inventory, guest observations, ACPI capture and guard command/status
utilities. Inspect engine-provided analysis/board helper contracts before sharing
code across repositories; reuse a versioned JSON command boundary rather than
copying divergent identity algorithms or coupling builds to a sibling checkout.

**Gate:** prepared outputs validate and match required logical contents and,
where deterministic, bytes; malformed inputs and interrupted publication are
safe. Each recipe has end-to-end evidence before its Python implementation is
removed. Native-library availability is included in the release dependency list.

### P10 — Compatibility daemon ports

Port Bluetooth H4 controller simulation, BLE tunnel framing/crypto and hwsim
netlink/medium logic as independent executable slices. Share bounded framing
utilities, but keep privileged namespace/network setup in an explicit operator
wrapper. The daemon must not acquire new implicit host-configuration privileges.

**Gate:** recorded command/event traces, seeded medium behavior, disconnect and
overflow tests match intended behavior. Validate on a suitable Bluetooth/Wi-Fi
host; track unavailable hardware separately. Package helper binaries and their
protocol versions with explicit engine/runtime compatibility.

### P11 — Remove Python and finalize releases

Remove replaced Python production modules, compatibility scripts, console entry
points, FastAPI generation and obsolete uv/package dependencies. Port required
release/OpenAPI/migration verification commands into Rust tooling. Retain old
tests as fixtures only where needed until equivalent Rust coverage is established,
then remove the Python test harness from the release pipeline.

Update README, operations, catalog docs, migration ledger and release packaging.
Document remaining system dependencies, experimental hardware capabilities,
supported host/engine combinations and the new resource vocabulary.

**Gate:** build, install, image preparation, normal runtime operation and shipped
helper invocation work in an environment without Python. CI checks no production
path shells out to Python. All capability-ledger rows are implemented/verified or
explicitly experimental with no Python fallback. This is **full production
migration**.

## 9. Existing-state import and rollback

API incompatibility does not require discarding existing VM data. Do not mutate
legacy roots in place or infer correctness from their status strings.

1. `migrate inspect` reads legacy catalog, instance/session manifests, asset
   references and actual state trees. Record hashes, permissions, backing chains,
   missing components, running-process evidence and unresolved configuration.
2. `migrate plan` writes explicit old-to-new IDs, model/preset mapping, image
   component mapping, identity handling and disk-space requirements. Reject
   conflicting profile associations and unsupported layouts; report repair steps.
3. Stop the old API and all relevant QEMU/helpers. Prove quiescence before copy;
   a legacy manifest saying `stopped` is not sufficient. Keep the source intact.
4. Import into a separate Rust installation root. Copy mutable data, rebuild image
   references, and rewrite/rebase copied QCOW2 chains through verified image tools
   when legacy absolute paths would otherwise remain. Never point a new writable
   instance at a legacy writable backing file or hard-link writable disks.
5. Create instances with imported identity/configuration and provenance. Historical
   session metadata is archival evidence, not proof of distinct past launches;
   do not fabricate run history when session IDs were reused. Rehash legacy
   snapshots from their captured contents and flag missing components, because
   old recorded hashes/inventories may be stale or incomplete.
6. Verify destination hashes, tree completeness, chain independence and identity;
   boot a disposable imported copy before selecting the new installation.
7. Keep a migration report and source backup. Import is resumable/idempotent by
   inventory fingerprint and mapping; retry cannot duplicate instances silently.

Before Rust first writes guest state, rollback can select the untouched Python
installation. After new guest writes, reverting loses those changes unless they
are explicitly exported and re-imported. Do not promise transparent downgrade,
dual ownership or a reverse API adapter. Database schema upgrades require a tested
backup and documented restore procedure, coordinated with state generations.

## 10. Validation and release gates

| Layer | Required checks |
| --- | --- |
| Domain | IDs/digests, model/image compatibility, policy precedence, schema versions, stable identity, lifecycle transition matrix |
| Storage | Concurrent import, digest mismatch, symlink/path rejection, sparse large files, disk-full/permission errors, reference integrity, interrupted publish/restore/GC |
| Runtime | QMP request/event ordering, timeouts, malformed frames, start/stop races, child exit, PID reuse, daemon crash windows, helper cleanup, cancellation |
| API/CLI | New endpoints, typed errors, idempotency, optimistic revisions, auth/CSRF/origin, run/channel-scoped tickets, operation recovery |
| Browser | Component and end-to-end tests, production serving/auth, instance/run routing, operation progress, viewer teardown and stale-request handling |
| Engine | Actual QEMU start/pause/resume/reset/stop and persistent state on installed pinned bundles |
| Firmware/helpers | Deterministic outputs/protocol vectors, native FFI checks, hardware-specific acceptance where available |
| Release | Independent checkout, reproducible locked builds, clean installation, correct bundled assets, no Python at final milestone |

Use Rust unit/integration tests, a fake QMP/socket peer, bounded property/fuzz
tests for exposed binary parsers, and process-level fault injection. Preserve
legacy fixtures only where semantics remain valid. Python success is useful as
a comparison oracle for deterministic transforms, not for known broken lifecycle
or snapshot behavior.

CI runs formatting, Clippy with an agreed warning policy, workspace tests,
schema/type drift checks, browser tests/build and release smoke. Separate
real-engine jobs from privileged/hardware jobs, and fail advertised stable-feature
gates rather than quietly skipping missing dependencies. Avoid adding blanket
workspace acceptance criteria that require unavailable hardware for every edit.

Measure start/stop latency, concurrent viewer behavior, event-loop responsiveness,
copy/hash throughput and peak memory on representative large disks. Establish
baselines on a named host and engine before claiming performance gains. Resource
budgets must bound queues and memory independently of total disk/log size.

## 11. Suggested implementation PR sequence

| PR | Deliverable | Depends on |
| --- | --- | --- |
| 1 | Domain schema fixtures, feature ledger, model/image split examples | This plan |
| 2 | Cargo workspace, domain types, CI and schema/type generation | 1 |
| 3 | SQLite metadata, operations, installation lock and blob publication | 2 |
| 4 | Model/image import, engine validation, catalog API/CLI | 3 |
| 5 | Instance/config/identity creation and deterministic launch planner | 4 |
| 6 | QMP/process runtime, lifecycle, instance exclusion and recovery | 5 |
| 7 | State generations, snapshots, restore, cloning and GC dry-run | 6 |
| 8 | Diagnostics, UART, display and input leases | 6 |
| 9 | Audio, debugger, attachments and helper control clients | 8 |
| 10 | New browser resource flow and daemon production serving/auth | 4–9 |
| 11 | Offline importer, release packaging and runtime cutover | 7, 9, 10 |
| 12+ | Firmware and analysis utilities, one verifiable recipe/slice per PR | 11 |
| Next | Compatibility helper ports, one protocol slice per PR | 11 |
| Final | Remove Python production paths and complete release evidence | All replacement gates |

PRs may be divided further to remain reviewable. Storage/runtime correctness is
the critical path; browser scaffolding can begin after the contract stabilizes.
Re-estimate remaining work after the first real QEMU runtime slice and the first
ported firmware recipe. A reliable calendar estimate needs those measurements
and confirmed hardware access; line counts alone do not provide one.

## 12. Technical references

The following primary documentation supports the tool choices, not a claim that
the proposed application is already implemented:

- [Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html):
  workspace layout, shared lockfile and dependency configuration.
- [Axum WebSockets](https://docs.rs/axum/latest/axum/extract/ws/): HTTP upgrade and
  WebSocket transport support.
- [Tokio child processes](https://docs.rs/tokio/latest/tokio/process/struct.Child.html):
  explicit process lifecycle; dropping a child handle is not a complete cleanup
  or recovery policy.
- [Utoipa schemas](https://docs.rs/utoipa/latest/utoipa/derive.ToSchema.html):
  derive public OpenAPI schemas from Rust types.
- [rusqlite connection API](https://docs.rs/rusqlite/latest/rusqlite/struct.Connection.html):
  connection and backup interfaces for the local metadata worker.
- [SQLite atomic commit](https://www.sqlite.org/atomiccommit.html): database
  durability mechanics; VM files still require their own publication protocol.
