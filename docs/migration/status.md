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

## Still pending

- U6+ firmware preparation, including its model-owned EEPROM generation through QEMU `board-tools`.
- Compatibility adapters, analysis domain, host tooling, and the remaining runtime/profile/API migration surface.
- Browser-client migration now has a React/Vite foundation covering health, catalog profile/session creation, safe profile-detail views, a session directory, session inspection, reconciliation, start/stop, state inventory, an initial profile-declared UART terminal, and restricted QMP status. Display, audio, remote-device, and hardware views remain blocked on their corresponding target API contracts.

## Verification baseline

The latest local main-repository run completed `76` Python tests successfully; the browser client has `10` Bun tests. The firmware changes are recorded in commits through `8d2a30f`; see `unifi-qemu.yaml` for the capability ledger and evidence links.
