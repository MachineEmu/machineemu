# Migration status

Last updated: 2026-09-22.

## Completed checkpoints

- The Rust step-0 planner now validates new-format profile machine, CPU,
  accelerator, NIC, video, audio, TPM, and analysis-property choices against
  the selected QEMU executable. The planner has nine unit tests and runs in CI;
  runtime state and QEMU process ownership remain outside this checkpoint.
- The Rust runtime foundation now owns one explicit workspace root at a time,
  persists image manifests and instance revisions in SQLite, validates stable
  IDs, and records idempotent operation keys with conflict detection and
  guarded lifecycle transitions. Staged blob import verifies SHA-256 before
  atomic publication. The runtime also has argv-only process supervision and a
  line-framed QMP client with capability negotiation and event-safe replies;
  durable run records now include PID and Linux process-start identity, and
  recovery marks missing or reused identities as uncertain instead of retrying
  blindly. A tested start path now persists the operation, spawns argv, waits
  for QMP negotiation, and advances the instance to running with cleanup on
  failure. Pause, resume, reset, and graceful stop now use QMP and update
  durable run/lifecycle state in tested orchestration. Daemon restart recovery,
  active-run startup scans, authenticated API routes, and real-QEMU evidence
  are still pending. A Rust daemon binary now exposes bearer-authenticated
  health, instance create/inspect, and active-run reconciliation routes over
  loopback-configurable `/api/v2` endpoints. Stopped-only snapshot capture and
  restore now stage and hash each declared component before publication, with
  tamper detection covered by runtime tests. Snapshot cloning now restores
  independent copied components under a new instance identity. The daemon also
  registers and inspects image manifests, and its route-level bearer-auth
  behavior is tested. The API now exposes durable operation inspection, and a
  router integration test covers image registration followed by instance
  creation and inspection. The daemon can load a local planner-produced launch
  registry and exposes guarded start/stop/pause/resume/reset routes; starts are
  rejected when no approved profile plan is registered. Snapshot capture,
  inspection, and independent clone routes operate only on workspace-owned
  instance directories. Workspace locks now record PID/start identity and
  reclaim only demonstrably stale Linux locks, with live-lock and stale-lock
  recovery tests. The installed QEMU 10.2.4 system binary validates the
  Debian 13 and Windows 11 profiles; the non-analysis system binary correctly
  rejects the analysis profile's custom machine properties. A disposable real
  QEMU 10.2.4 process accepted QMP capabilities and returned `query-status`
  before teardown. Images also have a portable bundle representation with a
  readable manifest and named disk, firmware, and TPM component files; import
  verifies component digests before adding them to the internal store, and
  export recreates the same directory shape. The planner CLI can import an
  immutable `vmmanager-sh` base directory while excluding instance overlays and
  runtime TPM lock/PID files. The Debian and Windows profiles now declare the
  explicit `br0` bridge, and planner inputs can attach a validated per-instance
  NoCloud seed ISO as a read-only CD-ROM. The Debian workspace now registers
  the disk, OVMF code, pristine OVMF variables, and seed as verified assets.
  The Rust-owned `machineemu run PROFILE INSTANCE` path now generates the
  complete profile argv, prepares the overlay and writable per-instance OVMF
  variables/TPM directory, starts `swtpm`, and submits the launch plan inline
  to the authenticated daemon. The daemon records QMP-backed start/stop
  operations and supervises the helper; `machineemu ps`, `stop`, and
  stopped-state `rm --force` use the daemon API. The real Debian QEMU gate has
  passed through start, pause, resume, reset, stop, restart, and forced removal.

- The `MachineEmu` organisation owns separate `machineemu` and `qemu` repositories; source and infrastructure policy is AGPL-3.0-only for now.
- QEMU patches, board crates, and display/audio/remote-device helper crates are ported to `machineemu/qemu`.
- Runtime, catalog, API, and browser work have an initial implementation in `machineemu`; the remaining gaps are tracked as parity and hardware-validation gates below.
- Direct pushes no longer consume GitHub Actions minutes. Both repositories run CI only for pull requests or manual dispatch; local checks are the default.
- The UniFi firmware foundation now validates containers/FIT images, safe CPIO editing, manifests, and external artifact digests.
- UDM-Pro preparation is ported: generated GPT/SPI state, bounded in-memory SquashFS editing, fresh-disk construction, atomic manifest-backed bundle publication, and opt-in diagnostic lab signing.
- US24Pro preparation is ported: extraction, diagnostic-only pinned signature bypass, opt-in lab signing, and separately materialized SDK-calibration diagnostic initramfs.
- U6+ preparation is ported: MT7981 container/FIT extraction, model-owned EEPROM generation through QEMU `board-tools`, deterministic eMMC layout construction, external-rootfs password handling, and opt-in diagnostic lab signing.
- Compatibility control clients and operator-started Bluetooth/mac80211_hwsim daemons are ported under `scripts/compat`; privileged host setup, namespace integration, and hardware-backed validation remain pending.
- Analysis foundations and profile launch hooks are ported: deterministic seeded identities, read-only host inventory, normalized guest-observation verification, ACPI table capture, non-secret launch metadata, and x86 UUID/SMBIOS/`kvm=off` arguments. The dedicated QEMU analysis patch series is now activated with the non-secret Rust validation payload and profile-owned ACPI/sensor/PCI properties; the opt-in host-kernel guard scaffold and session-safe KVM-guard load/status/snapshot CLI are also present. A clean x86_64 analysis engine build and validated bundle manifest now pass. On privileged Debian lab host `lab1`, the guard module compiles through `.ko` generation against the running 6.12.107 headers; module insertion remains unverified because Secure Boot rejects unsigned modules.
- Analysis sessions now write a source-compatible, non-secret `environment.json` artifact with identity, QEMU, asset, machine, network, firmware, and endpoint provenance; raw seeds and host paths are excluded.
- Profile networking now validates the source-supported `disabled`, `user`, and `bridge` modes and emits deterministic QEMU `-nic` arguments; analysis CPU policy is resolved with `kvm=off` enforcement. Analysis cloning now creates QCOW2 backing overlays, copies OVMF/TPM state, records baseline hashes, and supports clone validation; guest-observation recording is available through the main CLI as well as the compatibility script.
- The catalog now includes a metadata-only `malware-analysis-x64` profile with explicit disk/OVMF asset gates, disabled-by-default networking, and the source-aligned non-secret SMBIOS, ACPI, display, USB, storage, sensor, PCI, and CPU descriptor set.
- The QEMU `analysis-profile` crate is ported with deterministic identity/clone validation and a JSON CLI; the opt-in QEMU device-hook patch series and analysis host-kernel guard scaffold are present, with clean engine validation recorded below and privileged-host validation pending.
- Engine host tooling is partially ported: track checking, reproducible fetch/build scripts, immutable bundle manifest generation, and bundle validation are present in `machineemu/qemu`; the analysis wrapper now uses a dedicated x86_64 build directory and records the selected target in its manifest. Privileged kernel and compatibility setup remain pending.
- The canonical OpenAPI contract now includes the migrated lifecycle, screenshot, audio, remote-device, and QMP-backed USB/CD-ROM/network hotplug routes; browser types are regenerated from `contracts/openapi.json`. Host USB inventory now includes sysfs identity metadata while excluding root hubs and interface nodes.
- Verified instance snapshots are now exposed through authenticated list/create/restore routes, with restore rejected while any session for the instance is running.
- The session display boundary now includes manifest-checked, loopback-only VNC and framed-video proxies with bounded fragmented RFB parsing, validated video controls, and per-viewer input leases; the launch plan records declared VNC/video socket ownership.
- External loopback VNC exposure now has password-authenticated forwarding with explicit start/status/stop routes, and the compatibility helper control surface now includes session-scoped Wi-Fi medium and Bluetooth advertising routes.
- Lifecycle parity now includes restart/delete operations with bounded operation status records, safe stopped-session removal, and polling WebSocket views for hwsim and Bluetooth helper state.
- GDB/MI support now includes manifest-owned Unix/TCP debug endpoints, a shared server-side GDB console, authenticated multiplexed WebSocket control, and a browser transcript/command view.
- Analysis device parity now includes catalog-backed inventory/detail/launch validation/session creation, schema-checked device descriptors, and safe baseline cloning from explicitly declared disk and firmware-variable assets.
- Session diagnostics now expose bounded public hardware configuration, analysis environment metadata, and tailed stdout/stderr logs without returning runtime host paths; catalog, device, and clone responses likewise redact seeds and artifact paths.

## Still pending

- Privileged Bluetooth/Wi-Fi host setup and namespace integration remain pending. The available lab1 host exposes no IEEE 802.11, Bluetooth, sound, or mac80211_hwsim devices, so those checks require a different hardware-capable host. The analysis-kernel guard now has a privileged exact-kernel compile result, but insertion/status validation remains blocked by Secure Boot rejecting the unsigned module. QMP hotplug and host USB code still require hardware-backed validation; catalog clone validation remains limited to profiles that explicitly declare disk and firmware-variable assets.
- Browser-client migration now has a React/Vite foundation covering health, catalog profile/session creation, safe profile-detail views, a session directory, session inspection, reconciliation, start/stop/pause/resume/reset, state inventory and instance/session snapshot controls, an initial profile-declared UART terminal, validated front-panel and LCD streams with semantic touch input, restricted QMP status/inspection, a session-owned display screenshot, source-compatible capability reasons, SPICE audio playback/capture with leases and teardown, interactive noVNC VNC and WebCodecs H.264 display views with lease controls, a GDB/MI console, and remote-device capability, reservation, ticket, and socket-proxy plumbing. Typed client coverage now includes CD-ROM and network hotplug mutations. Privileged/hardware-backed audio validation remains pending.

## Verification baseline

The latest local main-repository run completed `128` Python tests successfully with `3` skipped; the browser client has `11` Bun tests, and the production browser build passes. Under a Nix dependency shell with GBM/OpenGL and GStreamer runtime libraries, the complete QEMU workspace test suite passes (with one optional encoder test ignored); the `analysis-profile` crate has `4` passing tests, including resolved-payload validation, and the track verifier passes. The clean analysis build `bash qemu/scripts/build-analysis-unifi-10.2.sh` completes for `x86_64-softmmu`, `qemu/scripts/validate_engine_bundle.py` validates its `engine-build.json`, and a runtime QEMU smoke accepts a valid opt-in analysis payload while rejecting an invalid network mode before startup. The analysis-kernel module also compiles to `.ko` against lab1’s running 6.12.107 kernel; insertion is blocked by Secure Boot. Hardware-backed hotplug/audio/compatibility validation remains pending. The firmware changes are recorded in commits through `8d2a30f`; see `unifi-qemu.yaml` for the capability ledger and evidence links.
