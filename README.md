# MachineEmu

MachineEmu is the runtime, API, browser client, catalog, and domain integration
repository. It consumes immutable engine bundles produced by `machineemu/qemu`.

The repository is currently licensed under AGPL-3.0-or-later. Third-party and
restricted inputs retain their own licensing and distribution requirements.

This checkout is the first migration bootstrap. Source code is intentionally not
copied until the M0 ledger and baseline are reviewed.

## Independent checkout

This repository must build and test without a sibling checkout. Local development
may point an explicit engine installation at `MACHINEEMU_ENGINE_ROOT`; releases
record the exact engine-build digest in `release-set.json`.

Initial ownership is split by responsibility:

- `python/machineemu/runtime`: lifecycle, state, launch supervision
- `python/machineemu/api`: HTTP/WebSocket and authentication boundaries
- `python/machineemu/engines`: installed-engine resolution
- `python/machineemu/assets`: content-addressed asset handling
- `python/machineemu/profiles`: profile validation and launch inputs
- `python/machineemu/domains`: device-research and analysis integrations
- `web`: current browser client, moved after the first runtime slice
- `catalog`: redistributable profile metadata
- `contracts`: exported schemas and protocol fixtures

The first implementation keeps the existing Python package and FastAPI schema
generation direction. It does not require Rust schema bindings or a plugin ABI.
