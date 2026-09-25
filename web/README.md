# MachineEmu web client

The browser consumes the Rust daemon's API v2, described by
[`../contracts/openapi-v2.json`](../contracts/openapi-v2.json). The checked-in
TypeScript declarations are generated from that contract.

Install JavaScript tooling, generate API types, and type-check with:

```sh
bun install
bun run generate:api-types
bun run typecheck
bun test
bun run build
```

For frontend development, copy `.env.example` to `.env` and set
`MACHINEEMU_API_TOKEN` when the daemon requires bearer authentication. Vite
proxies `/api` and `/ws` to `MACHINEEMU_API_URL` (default:
`http://127.0.0.1:8000`) and adds the token to proxied requests. The token is
not exposed to the browser bundle.

The client covers profile/image discovery, instance lifecycle, configuration
metadata, screenshots, and ticketed v2 streams. Regenerate the declarations
after changing the Rust contract with `bun run generate:api-types`.
