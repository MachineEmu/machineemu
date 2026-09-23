# API v2 live VM events

## Goal

Add an authenticated server-sent events (SSE) endpoint for instance state,
operation progress, and selected QMP events. Today, clients poll
`GET /api/v2/operations/{id}` and instance reads. The Python API has no
equivalent live-events endpoint; its QMP client only consumes events while
handling commands or waiting for a named event. Display, audio, and device
WebSockets remain separate data streams.

## Display and input boundary

SSE carries VM lifecycle and operation notifications, not display frames or
guest input. The existing API v2 `vnc` WebSocket carries raw RFB in both
directions, including keyboard and mouse input. The `video` WebSocket carries
framed H.264/audio records toward the viewer and allowlisted JSON keyboard,
pointer, resize, and clipboard controls toward `display-stream`. See
`docs/operations/api-v2-streams-devices.md` for tickets and wire formats.

Both transports need one shared input owner per run when VNC and H.264 are
enabled together. The Python API uses one owner for VNC and video; the current
Rust API keys control by stream kind, so it can admit a VNC controller and a
video controller at the same time. Align the Rust control lease across both
display transports before browser API v2 input parity. Define claim, takeover,
release, expiry, and disconnect behavior; permit view-only VNC connections by
filtering RFB input messages. Bind tickets and input ownership to the current
run, and revoke them when the run ends or is replaced. Never publish individual
keystrokes, pointer movements, or clipboard contents to SSE history.

## Contract

- `GET /api/v2/instances/{id}/events` returns `text/event-stream` for an
  existing instance, including one with no active run. Return 404 for an
  unknown instance and 400 for a malformed ID or `Last-Event-ID`. For the
  current daemon, require its bearer token on TCP or the protected Unix socket.
  The future same-origin browser uses the short-lived HttpOnly authentication
  cookie described in `docs/migration/rust.md`; native `EventSource` cannot set
  a bearer header. Browser adoption of this endpoint waits for that cookie
  flow, or uses a `fetch`-based reader with an explicitly supplied short-lived
  bearer credential. Never embed a permanent token or put credentials in URLs.
- Use SSE `event`, `id`, and single-line JSON `data` fields. Every JSON payload
  has `schema_version`, `instance_id`, `run_id` (null when absent), and an RFC
  3339 timestamp. The SSE `id` is an opaque cursor containing the instance ID,
  daemon generation, and increasing per-instance sequence number; clients must
  not infer elapsed time from it. Heartbeat comments have no ID.
- Define typed payloads: `snapshot` contains the current instance state and
  revision, active run ID/status if any, and IDs/statuses of active operations;
  `state` contains the new instance state, revision, run status when applicable,
  and a safe reason code; `operation` contains operation ID, kind, status, and
  safe result or failure code; `qmp` contains the event name and only explicitly
  approved fields for that name. Clients can use the existing resource GET
  routes for full details, especially after a resync. Do not expose raw QMP
  payloads, arbitrary command responses, host paths, bearer tokens, or guest
  clipboard contents.
- Allowlist `SHUTDOWN`, `RESET`, `STOP`, `RESUME`, `POWERDOWN`,
  `DEVICE_DELETED`, and `BLOCK_IO_ERROR`. Define a bounded DTO for each,
  including a maximum serialized event size. `POWERDOWN` is a request, and
  `SHUTDOWN` is not a terminal instance state until process exit and cleanup
  are confirmed.
- Without `Last-Event-ID`, send one `snapshot`, then live events. With a valid
  cursor still covered by contiguous history, replay events strictly after it,
  then continue live without an initial snapshot. If the cursor is expired,
  ahead of the head, from another instance or daemon generation, send a new
  `snapshot` and then live events. A snapshot replaces the client's prior view;
  clients waiting for an operation should refresh that operation by ID after
  resync. Document this behavior for reconnecting clients.
- The hub retains at most 256 events and 1 MiB per instance, with a 32 MiB
  global history budget. Eviction of any required event makes replay
  unavailable and triggers snapshot resync. Cap each event at 16 KiB, each
  subscriber queue at 64 events, subscribers at 8 per instance and 128 globally.
  Send heartbeat comments every 15 seconds. Disconnect a slow subscriber when
  its queue fills; it may reconnect using its last received ID.

## Ordering and state ownership

- The per-instance owner serializes lifecycle commands, QMP observations,
  child-exit observations, reconciliation, and snapshot/subscription setup.
  Commit a state or operation change before publishing its event, then publish
  before releasing the per-instance ordering boundary. The hub must not call
  into the workspace while holding its own lock. This prevents a snapshot from
  seeing newer state followed by an older queued event, or missing an event
  between the state read and subscription.
- Allocate IDs and register the subscriber at the same history boundary used
  to select replay or create a snapshot. Snapshot events have IDs and are kept
  in history so reconnecting after a snapshot is well defined. Emit each
  committed transition once, including operation acceptance, completion, and
  failure. Do not emit a new transition for an idempotent retry that returns an
  existing operation.
- QMP `STOP` and `RESUME` observations update the authoritative instance state
  through the run owner, including guest-initiated changes. On initial attach
  and after a QMP reader reconnect, run `query-status` on that reader's
  connection and reconcile its response with events in order: events before
  the response are superseded by it; later events must not be overwritten.
  Process exit takes precedence over pending QMP observations. Use the runtime
  lifecycle rules in `docs/migration/rust.md`.
  Publish a terminal state only after the same cleanup and writer-exit checks
  used by Stop. `RESET` and `POWERDOWN` alone do not imply a state transition.
- Give each active run one owned QMP event reader on a separate connection so
  it cannot consume command replies or device-unplug results from the command
  connection. Feed its observations to the run owner. Bound frames, queues,
  reconnect attempts, and cancellation; discard observations whose run ID is
  no longer current. Reconnect while the run is alive and reconcile state
  before publishing further state observations. Stop the reader with the run.

## Implementation sequence

1. Define versioned, size-limited event DTOs and a daemon-owned hub with the
   stated history, ID, subscription, and eviction rules.
2. Route committed lifecycle and operation transitions, QMP observations, and
   child exits through the per-instance owner. Cover startup failure, guest
   shutdown, unexpected QEMU exit, and reconciliation as well as successful
   start, stop, pause, resume, reset, and snapshot operations.
3. Add the per-run QMP reader, status reconciliation, and cancellation. Keep
   command-side QMP event handling intact for operations such as device unplug.
4. Add the SSE route with authentication, atomic snapshot/replay handoff,
   heartbeat, disconnect cancellation, and backpressure limits. Specify
   no-cache and proxy buffering behavior for deployed clients.
5. Add the route and event schemas to the generated Utoipa document. Document
   SSE framing and reconnect semantics alongside OpenAPI. Update browser API
   types and client code when the web client migrates to `/api/v2`.

## Verification

- Start, pause, resume, reset, and stop a VM; verify ordered event IDs,
  matching instance/run IDs, operation acceptance and terminal outcomes, and
  no duplicate event on an idempotent retry.
- Trigger guest-side `STOP`, `RESUME`, and `SHUTDOWN`; verify state follows the
  QMP observation and terminal state waits for process and helper cleanup.
  Force a QEMU crash and startup failure; verify safe failure/terminal events.
- Race subscription and reconnection against state commits. Verify that a
  recent cursor replays exactly the missing events with no initial snapshot;
  expired, future, foreign-instance, and earlier-generation cursors resync;
  malformed cursors fail before streaming. Restart the daemon and verify
  snapshot resync plus operation GET recovery.
- Disconnect and reconnect the QMP event socket, including events around the
  `query-status` response; verify state reconciliation, stale-run filtering,
  and that device-unplug command handling still receives its event.
- Exercise two simultaneous subscribers, limits, history eviction, a slow
  subscriber, a stopped instance, and a malformed instance ID. Verify bearer
  authentication over TCP and access through the protected Unix socket. When
  browser cookie authentication exists, test its SSE access and expiry.
- With VNC and H.264 enabled together, verify that both viewers can observe,
  only the shared input owner can send keyboard/mouse controls, takeover
  revokes the previous owner, and run end revokes both transports' tickets and
  leases. Confirm that SSE contains none of those input messages.
- Check the generated OpenAPI path, schemas, and `text/event-stream` response;
  exercise actual SSE framing and heartbeats with a streaming client.
