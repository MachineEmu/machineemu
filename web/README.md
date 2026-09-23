# MachineEmu web client

This browser still consumes the retained API v1
[`../contracts/openapi.json`](../contracts/openapi.json). The Rust daemon serves
API v2, described by [`../contracts/openapi-v2.json`](../contracts/openapi-v2.json),
so the browser has not completed its API migration.

Install JavaScript tooling, generate API types, and type-check with:

```sh
bun install
bun run generate:api-types
bun run typecheck
bun test
bun run build
```

For frontend development, copy `.env.example` to `.env` and set
`MACHINEEMU_API_TOKEN` when using an API v1 server. Vite proxies `/api` and
`/ws` to `MACHINEEMU_API_URL` (default: `http://127.0.0.1:8000`) and adds the
token to proxied requests. The removed Python server supplied that API; the
current Rust daemon does not serve these v1 routes. The token is not exposed
to the browser bundle.

The current client covers its original health, inspection, reconciliation,
start, and stop flows. Type generation above still targets the retained v1
schema; it does not generate Rust API v2 types.
