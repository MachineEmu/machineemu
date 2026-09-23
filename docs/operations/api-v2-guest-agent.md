# Rust API v2: guest agent information

Enable `devices.guest_agent: true` in the creation template and install and start the
QEMU guest agent inside the guest. The planner exposes the agent on the
instance-owned `qga.sock` channel. The daemon reads from that channel only.

`GET /api/v2/instances/{id}/guest-agent` uses the normal API bearer token (or
the protected Unix socket). It returns HTTP 200 for an existing instance even
when the agent is unavailable:

```json
{
  "available": false,
  "instance_id": "lab01",
  "run_id": null,
  "reason": "not_running",
  "version": null,
  "hostname": null,
  "os": null,
  "interfaces": null,
  "filesystems": null,
  "users": null,
  "timezone": null,
  "vcpus": null
}
```

Possible unavailable reasons are `not_running`, `not_configured`,
`not_responding`, and `run_changed` (the VM run changed during the query).
When `available` is true, `version` is the agent version;
the other fields contain the corresponding guest-agent responses when each
command is supported and enabled. Optional fields may be `null` if the guest
does not support a command or stops responding during the query. The response
is sampled on request and is not stored in the event history.

The endpoint invokes only `guest-info`, `guest-get-host-name`,
`guest-get-osinfo`, `guest-network-get-interfaces`, `guest-get-fsinfo`,
`guest-get-users`, `guest-get-timezone`, and `guest-get-vcpus`. It does not
execute arbitrary guest-agent commands. Guest-provided strings and arrays
should be treated as untrusted data by clients.

Clipboard access is a display feature, not a QEMU guest-agent command. Use the
ticketed VNC or H.264 display stream for clipboard operations.
