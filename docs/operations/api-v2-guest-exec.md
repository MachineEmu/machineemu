# Rust API v2: guest command execution

The instance must have been created with `devices.guest_agent: true`, and the QEMU guest
agent must be running inside the guest. Command execution uses QGA
`guest-exec`, as in `vmmanager-sh/src/vm-exec`.
Guest-agent calls and polling use Tokio Unix sockets and timers. The daemon
checks that the run ID is still current before each poll.

Start an execution with `POST /api/v2/instances/{id}/guest-executions` and a
JSON body such as:

```json
{"command":"uname -a","shell":"auto","timeout_seconds":120}
```

`shell` may be `auto` (the default), `sh`, or `powershell`. The auto choice
uses QGA OS information when available; if it is unavailable, it chooses
`/bin/sh`. The command is limited to 4096 bytes, and the timeout may be 1 to
600 seconds. The response is HTTP 202 with an `execution_id` and an
`events_url`. Supply the daemon bearer token to both requests.

Open the returned `GET /api/v2/guest-executions/{id}/events` route for SSE.
`status` events report `queued` and `running` with the guest PID when known.
At process exit, `output` events carry base64 chunks with `channel` set to
`stdout` or `stderr`; a final `complete` event contains the exit code or
signal and truncation flags. A failed or timed-out job sends a `status` event
and a final `complete` event with a safe `error_code`. Reconnecting within ten
minutes after completion replays the current state and its retained output.
The daemon keeps at most 32 jobs and at most 256 KiB per output channel.

QGA provides captured stdout and stderr only **after** the process exits.
The SSE stream therefore reports progress while the process runs, then emits
its output at completion; it cannot show live output. A timeout ends API
polling but does not stop the guest process. QGA also has no interactive stdin
or PTY. An interactive WebSocket requires a separate guest-side transport,
such as a dedicated virtio-serial helper or SSH PTY, and is not part of this
endpoint. Command output is kept out of the VM lifecycle SSE history.
