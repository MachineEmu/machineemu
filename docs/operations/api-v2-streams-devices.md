# Rust API v2: streams and live devices

These routes belong to `machineemu-daemon`. HTTP requests use the daemon's bearer token (or its protected Unix socket). The instance must have a live run. Device changes are sent to QEMU over QMP and can fail when the VM or guest does not support hotplug.

The daemon uses async QMP for live device changes, VM start/stop/pause/resume/reset,
VNC and SPICE discovery, and the D-Bus display socket handoff. Per-instance
locks serialize lifecycle changes, device changes, and display setup. The CLI uses async QMP for live inspection
and async Unix or TCP connections for daemon requests. Workspace SQLite work
remains synchronous and is kept outside socket waits.

## Display and USB streams

Create a single-use ticket with `POST /api/v2/instances/{id}/streams/{kind}/ticket` and body `{"control":true}` for input or `{"control":false}` for viewing. The response contains `ticket` and `expires_in_seconds` (30). Open `ws(s)://{host}/ws/v2/instances/{id}/{kind}?ticket={ticket}` from a page whose `Origin` matches the WebSocket `Host`. Ticket creation uses the normal bearer token; the WebSocket uses the ticket. A ticket belongs to one instance, run, and stream type. VNC and video share one input owner per run. A new control ticket with `{"control":true,"takeover":true}` revokes the previous display controller when redeemed; closing a controller releases its input ownership. View-only VNC filters guest-directed RFB keyboard, pointer, and clipboard messages on the server.

| `kind` | Upstream | WebSocket data |
| --- | --- | --- |
| `vnc` | The local TCP or `instances/{id}/vnc.sock` endpoint reported by QMP `query-vnc` | Raw RFB bytes in binary frames, both directions. `control:false` permits display and protocol negotiation while filtering keyboard, pointer, and clipboard input. |
| `video` | QEMU D-Bus display through `display-stream`, exposed at `instances/{id}/video.sock` | One framed display-stream record per binary frame, including H.264 video and D-Bus audio when enabled. Viewers may use `control:false`; JSON `request_idr` is allowed. `control:true` also permits keyboard, pointer, resize, and clipboard control messages. |
| `audio-dbus` | The same QEMU D-Bus display and `display-stream` socket | Playback only: framed audio configuration records (type 5) and PCM data records (type 6). Request with `control:false`; browser messages other than WebSocket control frames are rejected. |
| `usbredir` | `instances/{id}/usbredir.sock` | Raw usbredir protocol bytes in binary frames, both directions. Use `control:true`. |
| `serial` | `instances/{id}/sockets/serial.sock` | Raw UART bytes in binary frames, both directions. Use `control:true`. A serial ticket and WebSocket may be opened while the instance is stopped; the daemon waits for QEMU's socket so the client captures output from the beginning of boot. The CLI does this automatically with `machineemu serial INSTANCE`. |
| `lcm` | Supervised UDM Pro LCD hub at `instances/{id}/display.sock` | Read-only `unifi.lcm.v1` JSON text frames. Use `control:false`. |
| `frontpanel` | Supervised UDM Pro panel hub at `instances/{id}/frontpanel.sock` | Read-only `unifi.frontpanel.v1` JSON text frames. Use `control:false`. UDM Pro port link state is derived from configuration and host carrier, rather than a modeled LED register. |

The `video` stream also carries QEMU's separate cursor plane in type-4 control
records with `type: "cursor"`. Each record contains guest pixel position,
visibility, hotspot, and the full little-endian ARGB cursor image. The latest
cursor state is replayed when a viewer joins or resynchronizes; viewers draw it
over the decoded H.264 frame.

To capture the complete serial boot, create the instance first, attach in one
terminal, and start it in another:

```sh
machineemu create --profile PROFILE INSTANCE
machineemu serial INSTANCE
# In another terminal:
machineemu start INSTANCE
```

For H.264, set `devices.h264: true` and `devices.video.type: virtio-vga-gl` in a Rust-planned profile, or start QEMU with `-display dbus,p2p=on,gl=on` and a GL-capable virtual GPU in a custom launch plan. A Rust-planned profile cannot enable VNC with a GL video device; select one display mode. Build this workspace's `display-stream` binary (`nix develop -c cargo build -p display-stream --release`), then configure the daemon's `--display-stream` path or put the binary on `PATH`. The first video ticket starts the streamer, passes a private D-Bus socket to QEMU through QMP, and records to `instances/{id}/screen.mp4`. The streamer captures QEMU DMABUF scanout and uses VA-API H.264 hardware encoding when available; its own software fallback reports its encoder and `hardware` status in the video configuration record. The daemon stops the streamer with the VM. Records use the streamer's 16-byte display header: bytes 4–7 are the big-endian payload length, followed by that many payload bytes. Records over 16 MiB are rejected. The USB redirection stream requires a usbredir-capable client, and the `usbredir` device must first be attached below. Browser WebUSB does not itself speak usbredir.

### Native viewer

`machineemu-viewer <instance>` (crate `crates/machineemu-viewer`) is a desktop client for the `video` stream. It reads the daemon endpoint and token from the `client` section of `machineemu.yaml` unless you pass `--endpoint` and `--token`. It decodes with GStreamer and prefers hardware. `--decoder auto` (the default) picks the first available `vah264dec`, `nvh264dec`, V4L2, D3D or VideoToolbox decoder. If none is available, or if the hardware decoder fails during the session, it switches to `avdec_h264` or `openh264dec` and requests a new keyframe. `--decoder hardware` fails instead of falling back, and `--decoder software` or an element name forces a specific decoder. The window title shows the active decoder. With VA-API, `vapostproc` converts to BGRx and scales to the window's viewport on the GPU, so the CPU receives each frame already at display size.

By default the viewer takes display input (`--view-only` disables it, `--takeover` revokes the current controller). It forwards keys as QEMU qnums by physical position, sends the absolute pointer, mouse buttons and the wheel, requests a guest resolution matching the window 400 ms after a resize (`--no-resize-guest` disables this), plays D-Bus guest audio (`--no-audio`), and syncs clipboard text (`--no-clipboard`). The guest clipboard is copied to the host when it changes, and the host clipboard is sent to the guest when the window gains focus. The dev shell provides the GStreamer plugins and the Wayland/X11 libraries: `nix develop -c cargo run --release -p machineemu-viewer -- <instance>`.

The top bar controls the live session. `SIZE` cycles through 1024×768,
1280×800, 1920×1080, and 2560×1440 guest resolutions. `SCALE` cycles through
Fit, 100%, 125%, 150%, and 200% local presentation without changing the guest.
`AUTO` toggles guest resolution changes when the window is resized. Click the
display or `INPUT` to confine keyboard and pointer input to the viewer; press
`Ctrl+Alt+G` to release it. The toolbar remains outside the guest viewport and
is never included in guest pointer coordinates or automatic resolution sizes.

## Screenshot and key chords

`POST /api/v2/instances/{id}/screenshot` returns the primary display as
`image/png` with `Cache-Control: no-store`. The daemon captures through QMP,
reads at most 16 MiB, and removes its temporary image. It requires a live VM
and a QEMU build that supports PNG `screendump`.

`POST /api/v2/instances/{id}/send-key` accepts a QEMU qcode chord, for example
`{"keys":["ctrl","alt","delete"],"hold_time_ms":100}`, and returns 204.
The chord supports 1–16 qcodes and a hold time up to 5000 ms. These HTTP
actions use the daemon bearer token; continuous keyboard and pointer input
belongs on the ticketed display WebSocket.

## Guest audio

The Rust planner accepts `audio` at the profile root (or `devices.audio`) with `model: ich9` and `backend: {type: dbus|spice, id: pc-audio}`. Both backends add an HDA controller and duplex codec. `model: none` or backend `type: none` disables guest audio.

With `type: dbus`, QEMU's D-Bus display carries guest audio to `display-stream`. The `video` WebSocket includes record type 5 for JSON audio configuration and type 6 for raw PCM audio bytes. The `audio-dbus` stream sends only those records and starts the same streamer if needed, so a raw VNC viewer can receive sound independently. The configuration specifies sample format, frequency, channel count, volume, mute, and enabled state. D-Bus audio provides playback; use SPICE audio when microphone capture is needed. When VNC is enabled without H.264, the planner starts a D-Bus display for audio and keeps raw VNC on its separate socket.

With `type: spice`, the planner starts a headless SPICE server at the run's `sockets/spice.sock`. Request channel tickets with `POST /api/v2/instances/{id}/audio/spice/tickets` and body `{"microphone":false}` or `{"microphone":true}`. The response has one-use tickets for `main`, `playback`, and optionally `record`. Open the `main` WebSocket first at `/ws/v2/instances/{id}/spice-main?ticket=...`; after its SPICE main handshake, open `spice-playback`, then `spice-record` if requested. Each WebSocket carries raw SPICE bytes in binary frames. The proxy validates the link's channel and connection ID and forwards only audio channel messages. Only one record channel per run can hold microphone capture at a time. Tickets expire after 30 seconds; issue a new set when reconnecting.

## UniFi companion helpers

The Rust CLI adds named helper processes to its launch plan. The daemon starts `swtpm` and any isolated Wi-Fi helper before QEMU. It starts the UDM Pro or US24PRO front-panel hub, optional UDM Pro LCD hub, and optional Bluetooth HCI simulator after QEMU creates their Unix sockets, then resumes the paused guest. It stops helpers in reverse order. The copied helper scripts live in `scripts/compat`; set `helpers.bluetooth_simulator`, `helpers.unifi_hub`, or `helpers.wifi_simulator` in `machineemu.yaml` to use installed paths outside this checkout. Helper stderr is saved as `instances/{id}/helper-{name}.log`. US24PRO panel events come from its modeled LED registers; UDM Pro port link state is derived from its network configuration and host carrier.

For UDM Pro, `devices.lcd:true` enables the LCD event and input sockets, and `devices.bluetooth:true` enables the UART and simulator. The Bluetooth control socket is `instances/{id}/bluetooth-control.sock`. `GET /api/v2/instances/{id}/helpers/bluetooth` returns simulator status. `POST` to the same path accepts `{"type":"configure","settings":{"name":"Lab"}}` or `{"type":"advertise","peer":{"address":"aa:bb:cc:dd:ee:ff"}}`. `POST /api/v2/instances/{id}/helpers/lcm` forwards one validated touch action such as `{"screensaver":false}` and returns QEMU's reply. Front-panel data is read-only.

For an MT7981 profile, `wifi.enabled:true` attaches QEMU to `instances/{id}/wifi.sock`. The Rust CLI requires `wifi.namespace` (an existing, dedicated hwsim network namespace) and `wifi.radios`, such as `["radio0=02:00:00:00:00:01", "radio1=02:00:00:00:00:02"]`. It launches `hwsim_adapter.py` inside that namespace with explicit medium ownership. The daemon needs permission to enter the namespace and manage its hwsim radios; it does not create the namespace or load the kernel module. `GET /api/v2/instances/{id}/helpers/wifi` returns medium status; `POST` accepts `{"type":"configure","settings":{"signal":-50}}`. A namespace must not be shared with a second helper claiming the same hwsim medium.

## Live devices

All device IDs are caller supplied and must start with the matching prefix. Media paths may be absolute host paths; relative paths resolve beneath `workspace/media`. `GET /api/v2/instances/{id}/devices/{kind}` queries QEMU's current USB, block, or PCI devices.

The CLI exposes the same live QMP operations:

```sh
machineemu device list lab01 usb-host
machineemu device add lab01 usb-host me-usbh-1 --hostbus 1 --hostaddr 2
machineemu device add lab01 usb-image me-usbi-1 --path tools.img
machineemu device add lab01 iso me-iso-1 --path /path/to/installer.iso
machineemu device iso-change lab01 /path/to/second.iso
machineemu device iso-eject lab01 me-iso-1
machineemu device add lab01 network me-net-1 --bus pcie-root-port-1 --model virtio-net-pci
machineemu device remove lab01 iso me-iso-1
```

These commands require a running instance. `config` remains the command for
hardware that must be present from boot.
Absolute media paths are attached directly. Add `--copy-media` when the file
should first be retained beneath `workspace/media`; an identical existing copy
is reused.

| Operation | Method and path | JSON body |
| --- | --- | --- |
| Attach host USB | `POST .../devices/usb-host` | `{"device_id":"me-usbh-1","hostbus":1,"hostaddr":2}` |
| Attach USB image | `POST .../devices/usb-image` | `{"device_id":"me-usbi-1","path":"disk.img","read_only":false}` |
| Attach USB redirection | `POST .../devices/usbredir` | `{"device_id":"me-redir-0"}` |
| Attach ISO drive | `POST .../devices/iso` | `{"device_id":"me-iso-1","path":"installer.iso","bus":"pcie-root-port-iso"}` |
| Change ISO | `POST .../devices/iso/me-iso-1/change` | `{"path":"second.iso"}` |
| Eject ISO | `POST .../devices/iso/me-iso-1/eject` | `{"force":false}` |
| Attach network card | `POST .../devices/network` | `{"device_id":"me-net-1","bus":"pcie-root-port-1","model":"virtio-net-pci","mac":"52:54:00:12:34:56"}` |
| Detach any managed device | `DELETE .../devices/{kind}/{device_id}` | None |

Q35 launch plans reserve `pcie-root-port-iso`, `pcie-root-port-1`, and `pcie-root-port-2` for live devices. ISO hotplug defaults to a virtio-SCSI controller on `pcie-root-port-iso` with a SCSI CD-ROM behind it. Pass `--bus` to select another reserved port. Network cards use one of the remaining reserved ports. Network hotplug currently creates a QEMU user-mode backend. `model` can be `virtio-net-pci`, `e1000`, or `rtl8139`.

A Windows guest needs its virtio-SCSI driver before it can see a live ISO on
the reserved PCIe port. Bootstrap that driver with `machineemu config INSTANCE
--iso /absolute/path/to/virtio-win.iso`, start the VM, and install the driver;
that boot-time optical drive uses the built-in AHCI controller.

Detaching waits up to five seconds for QEMU's `DEVICE_DELETED` event before deleting the associated block, network, or character backend. A response with `"detached":false` means guest removal remains pending; stop the VM before reusing that device ID.
