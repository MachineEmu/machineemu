# Start the Python API

The API runs as a loopback-bound FastAPI service. From the repository root:

```sh
uv sync --extra api
```

Create an operator configuration such as `operator.json`:

```json
{
  "schema_version": 1,
  "roots": {
    "engine_root": "engines",
    "asset_root": "assets",
    "state_root": "state",
    "runtime_root": "runtime",
    "artifact_root": "artifacts"
  }
}
```

Start it with an explicit development token:

```sh
uv run machineemu-api \
  --operator-config operator.json \
  --token dev-token
```

The default address is `http://127.0.0.1:8000`. Override it with
`--host` and `--port`. The service also accepts
`MACHINEEMU_API_TOKEN` instead of `--token`; if neither is supplied, it prints
a generated token once at startup.

Verify the service:

```sh
curl -H 'X-MachineEmu-Token: dev-token' \
  http://127.0.0.1:8000/api/v1/health
```

Expected response:

```json
{"status":"ok"}
```

For catalog-backed session creation, add `--catalog-root`. It must point at the
directory holding the profile files themselves, not at `catalog/`. Session
creation also needs both `--release-set` and `--bundle-root`, pointing to the
validated engine release metadata and installed engine bundles.

Operator-authored files -- catalog profiles, standalone profiles, the operator
config, and the release set -- are read as either JSON (`.json`) or YAML
(`.yaml`, `.yml`); YAML needs the `yaml` extra (`python -m pip install -e
'.[yaml]'`). Runtime manifests and session state this project writes itself are
always JSON. A profile ID must not be defined in more than one format.

```sh
uv run machineemu-api \
  --operator-config operator.json \
  --catalog-root catalog/profiles \
  --release-set release-set.json \
  --bundle-root engines \
  --token dev-token
```

Mutating API requests must include the token and a same-origin `Origin` header.
The server deliberately rejects non-loopback hosts in this initial service.
Put a TLS-authenticated reverse proxy in front of it before exposing it beyond
the local machine.
