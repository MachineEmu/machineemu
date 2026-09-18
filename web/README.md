# MachineEmu web client

This package is the incremental browser boundary for MachineEmu. It consumes
the checked-in [`../contracts/openapi.json`](../contracts/openapi.json) and
keeps session lifecycle calls in one client module while the existing lab UI is
ported in slices.

Install JavaScript tooling, generate API types, and type-check with:

```sh
bun install
bun run generate:api-types
bun run typecheck
```

The initial client intentionally covers health, inspection, reconciliation,
start, and stop. Display, input, audio, and remote-device transports remain
separate migration slices.
