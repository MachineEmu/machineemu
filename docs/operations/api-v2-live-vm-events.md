# Rust API v2: live VM events

## Starting a stopped VM again

After `POST /api/v2/instances/{id}/stop` reaches `stopped`, call
`POST /api/v2/instances/{id}/start` with `{}` to use the saved launch plan and
let the daemon generate a new `run_id`, `operation_id`, and `idempotency_key`.
Existing clients may continue to supply these IDs and an inline `launch_plan`.
The old run remains in history and its disk, NVRAM, and TPM state are reused.
`machineemu start INSTANCE` starts an existing stopped instance. The combined
`POST /api/v2/instances/{id}/restart` route and `machineemu restart INSTANCE`
stop the current run and start another from saved configuration.

Open `GET /api/v2/instances/{id}/events` with the daemon bearer token on TCP,
or through its protected Unix socket. The instance may be stopped. The response
is `text/event-stream` with `Cache-Control: no-cache, no-transform` and
`X-Accel-Buffering: no`. Keep proxy buffering disabled for this route.

Each message has `event`, `id`, and one JSON `data` line. The `snapshot` event
contains the current instance state, revision, active run, and active operation
summaries. Later `state`, `operation`, and `qmp` events carry committed changes
or selected QMP observations. All payloads include `schema_version`,
`instance_id`, `run_id` (or `null`), and an RFC 3339 `timestamp`. QMP payloads
contain only the fields approved for that event name. Keyboard, mouse,
clipboard, video, and audio data are never included.

For a new connection, expect a `snapshot` first. On reconnect, send the last
received `id` in the `Last-Event-ID` header. When contiguous history still
covers that cursor, the server replays only messages after it and then sends
live messages. If history is gone, the cursor is ahead of the stream, or it
belongs to another instance or daemon generation, the server sends a new
`snapshot`. Replace the local instance view with that snapshot and refresh any
operation being tracked through `GET /api/v2/operations/{id}`. Do not parse an
event ID for timing or other meaning. A malformed cursor returns HTTP 400.

Heartbeat comments arrive every 15 seconds and have no event ID. A slow
subscriber whose 64-message queue fills is disconnected; reconnect with the
last ID actually received. A client may receive HTTP 429 when the per-instance
or global subscriber limit is reached. The server retains up to 256 messages
and 1 MiB per instance, subject to a 32 MiB global history limit.

The endpoint is for lifecycle and operation metadata. Obtain VNC or H.264
display and keyboard/mouse input through their separate WebSocket tickets, as
described in [API v2 streams and live devices](api-v2-streams-devices.md).
Native browser `EventSource` cannot attach a bearer header; browser adoption
requires the planned short-lived HttpOnly cookie or a `fetch` stream with an
explicit short-lived bearer credential. Do not place credentials in the URL.
