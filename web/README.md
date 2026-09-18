# MachineEmu web client

This package is the incremental browser boundary for MachineEmu. It consumes
the checked-in [`../contracts/openapi.json`](../contracts/openapi.json) and
keeps session lifecycle calls in one client module while the existing lab UI is
ported in slices.

The workspace-level `../clients/` directory remains a planned cross-platform
repository scaffold. The first browser contract stays here with the server so
it can migrate without introducing a third release boundary; it can move to
that repository once an independent client release is justified.

Install JavaScript tooling, generate API types, and type-check with:

```sh
bun install
bun run generate:api-types
bun run typecheck
```

The initial client intentionally covers health, inspection, reconciliation,
start, and stop. Display, input, audio, and remote-device transports remain
separate migration slices.
