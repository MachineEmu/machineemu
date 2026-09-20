# Migration status

Last updated: 2026-09-20.

## Completed checkpoints

- The `MachineEmu` organisation owns separate `machineemu` and `qemu` repositories; source and infrastructure policy is AGPL-3.0-only for now.
- QEMU patches, board crates, and display/audio/remote-device helper crates are ported to `machineemu/qemu`.
- Runtime, catalog, API, and browser work have an initial implementation in `machineemu`, but remain partial migrations.
- Direct pushes no longer consume GitHub Actions minutes. Both repositories run CI only for pull requests or manual dispatch; local checks are the default.
- The UniFi firmware foundation now validates containers/FIT images, safe CPIO editing, manifests, and external artifact digests.
- UDM-Pro preparation is ported: generated GPT/SPI state, bounded in-memory SquashFS editing, fresh-disk construction, atomic manifest-backed bundle publication, and opt-in diagnostic lab signing.
- US24Pro preparation is ported: extraction, diagnostic-only pinned signature bypass, opt-in lab signing, and separately materialized SDK-calibration diagnostic initramfs.
- U6+ preparation is ported: MT7981 container/FIT extraction, model-owned EEPROM generation through QEMU `board-tools`, deterministic eMMC layout construction, external-rootfs password handling, and opt-in diagnostic lab signing.
- Compatibility control clients and operator-started Bluetooth/mac80211_hwsim daemons are ported under `scripts/compat`; privileged host setup, namespace integration, and hardware-backed validation remain pending.
- Analysis foundations and profile launch hooks are ported: deterministic seeded identities, read-only host inventory, normalized guest-observation verification, non-secret launch metadata, and x86 UUID/SMBIOS/`kvm=off` arguments. The dedicated QEMU analysis patch series and opt-in host-kernel guard scaffold are now present; clean analysis-engine and privileged-kernel validation remain pending.
- The QEMU `analysis-profile` crate is ported with deterministic identity/clone validation and a JSON CLI; the opt-in QEMU device-hook patch series and analysis host-kernel guard scaffold are present, pending clean engine and privileged-host validation.
- Engine host tooling is partially ported: track checking, reproducible fetch/build scripts, immutable bundle manifest generation, and bundle validation are present in `machineemu/qemu`; privileged kernel and compatibility setup remain pending.
- The canonical OpenAPI contract now includes the migrated lifecycle, screenshot, audio, remote-device, and QMP-backed USB/CD-ROM/network hotplug routes; browser types are regenerated from `contracts/openapi.json`.
- Verified instance snapshots are now exposed through authenticated list/create/restore routes, with restore rejected while any session for the instance is running.
- The session display boundary now includes manifest-checked, loopback-only VNC and framed-video proxies with bounded fragmented RFB parsing, validated video controls, and per-viewer input leases; the launch plan records declared VNC/video socket ownership.
- External loopback VNC exposure now has password-authenticated forwarding with explicit start/status/stop routes, and the compatibility helper control surface now includes session-scoped Wi-Fi medium and Bluetooth advertising routes.
- Lifecycle parity now includes restart/delete operations with bounded operation status records, safe stopped-session removal, and polling WebSocket views for hwsim and Bluetooth helper state.
- GDB/MI support now includes manifest-owned Unix/TCP debug endpoints, a shared server-side GDB console, authenticated multiplexed WebSocket control, and a browser transcript/command view.
- Analysis device parity now includes catalog-backed inventory/detail/launch validation/session creation and safe baseline cloning from explicitly declared disk and firmware-variable assets.
- Session diagnostics now expose bounded public hardware configuration, analysis environment metadata, and tailed stdout/stderr logs without returning runtime host paths.

## Still pending

- Privileged Bluetooth/Wi-Fi host setup and namespace integration, clean QEMU analysis-engine and richer device-descriptor validation, and the remaining runtime/profile/API migration surface are still pending. QMP hotplug and host USB code still require hardware-backed validation; catalog clone validation remains limited to profiles that explicitly declare disk and firmware-variable assets.
- Browser-client migration now has a React/Vite foundation covering health, catalog profile/session creation, safe profile-detail views, a session directory, session inspection, reconciliation, start/stop/pause/resume/reset, state inventory and snapshot controls, an initial profile-declared UART terminal, validated front-panel and LCD streams with semantic touch input, restricted QMP status/inspection, a session-owned display screenshot, source-compatible capability reasons, SPICE audio playback/capture with leases and teardown, interactive noVNC VNC and WebCodecs H.264 display views with lease controls, a GDB/MI console, and remote-device capability, reservation, ticket, and socket-proxy plumbing. Privileged/hardware-backed audio validation remains pending.

## Verification baseline

The latest local main-repository run completed `115` Python tests successfully with `3` skipped; the browser client has `10` Bun tests, and the production browser build passes. The QEMU `analysis-profile` crate has `2` passing tests and the track verifier passes. Full QEMU workspace testing remains environment-blocked because `pkg-config` and the GLib/GStreamer development packages are unavailable; hardware-backed hotplug/audio/compatibility validation is therefore still pending. The firmware changes are recorded in commits through `8d2a30f`; see `unifi-qemu.yaml` for the capability ledger and evidence links.
