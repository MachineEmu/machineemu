# Rust API v2: streams and live devices

These routes belong to `machineemu-daemon`. HTTP requests use the daemon's bearer token (or its protected Unix socket). The instance must have a live run. Device changes are sent to QEMU over QMP and can fail when the VM or guest does not support hotplug.

## Display and USB streams

Create a single-use ticket with `POST /api/v2/instances/{id}/streams/{kind}/ticket` and body `{"control":true}`. The response contains `ticket` and `expires_in_seconds` (30). Open `ws(s)://{host}/ws/v2/instances/{id}/{kind}?ticket={ticket}` from a page whose `Origin` matches the WebSocket `Host`. Ticket creation uses the normal bearer token; the WebSocket uses the ticket. A ticket belongs to one instance, run, and stream type. Only one control connection per run and kind is accepted.

| `kind` | Upstream | WebSocket data |
| --- | --- | --- |
| `vnc` | The local TCP or `instances/{id}/vnc.sock` endpoint reported by QMP `query-vnc` | Raw RFB bytes in binary frames, both directions. Use `control:true`. |
| `video` | QEMU D-Bus display through the sibling QEMU project's `display-stream`, exposed at `instances/{id}/video.sock` | One framed display-stream record per binary frame, including H.264 video and D-Bus audio when enabled. Viewers may use `control:false`; JSON `request_idr` is allowed. `control:true` also permits keyboard, pointer, resize, and clipboard control messages. |
| `audio-dbus` | The same QEMU D-Bus display and `display-stream` socket | Playback only: framed audio configuration records (type 5) and PCM data records (type 6). Request with `control:false`; browser messages other than WebSocket control frames are rejected. |
| `usbredir` | `instances/{id}/usbredir.sock` | Raw usbredir protocol bytes in binary frames, both directions. Use `control:true`. |

For H.264, set `devices.h264: true` and `devices.video.type: virtio-vga-gl` in a Rust-planned profile, or start QEMU with `-display dbus,p2p=on,gl=on` and a GL-capable virtual GPU in a custom launch plan. A profile with both `devices.h264` and `devices.vnc` keeps raw VNC on a Unix socket alongside D-Bus display. Build the sibling QEMU project's `display-stream` binary (`cargo build -p display-stream --release` from that project, with its GStreamer/VA-API development dependencies), then configure the daemon's `--display-stream` path or put the binary on `PATH`. The first video ticket starts the streamer, passes a private D-Bus socket to QEMU through QMP, and records to `instances/{id}/screen.mp4`. The streamer captures QEMU DMABUF scanout and uses VA-API H.264 hardware encoding when available; its own software fallback reports its encoder and `hardware` status in the video configuration record. The daemon stops the streamer with the VM. Records use the streamer's 16-byte display header: bytes 4–7 are the big-endian payload length, followed by that many payload bytes. Records over 16 MiB are rejected. The USB redirection stream requires a usbredir-capable client, and the `usbredir` device must first be attached below. Browser WebUSB does not itself speak usbredir.

## Guest audio

The Rust planner accepts `audio` at the profile root (or `devices.audio`) with `model: ich9` and `backend: {type: dbus|spice, id: pc-audio}`. Both backends add an HDA controller and duplex codec. `model: none` or backend `type: none` disables guest audio.

With `type: dbus`, QEMU's D-Bus display carries guest audio to `display-stream`. The `video` WebSocket includes record type 5 for JSON audio configuration and type 6 for raw PCM audio bytes. The `audio-dbus` stream sends only those records and starts the same streamer if needed, so a raw VNC viewer can receive sound independently. The configuration specifies sample format, frequency, channel count, volume, mute, and enabled state. D-Bus audio provides playback; use SPICE audio when microphone capture is needed. When VNC is enabled without H.264, the planner starts a D-Bus display for audio and keeps raw VNC on its separate socket.

With `type: spice`, the planner starts a headless SPICE server at the run's `sockets/spice.sock`. Request channel tickets with `POST /api/v2/instances/{id}/audio/spice/tickets` and body `{"microphone":false}` or `{"microphone":true}`. The response has one-use tickets for `main`, `playback`, and optionally `record`. Open the `main` WebSocket first at `/ws/v2/instances/{id}/spice-main?ticket=...`; after its SPICE main handshake, open `spice-playback`, then `spice-record` if requested. Each WebSocket carries raw SPICE bytes in binary frames. The proxy validates the link's channel and connection ID and forwards only audio channel messages. Only one record channel per run can hold microphone capture at a time. Tickets expire after 30 seconds; issue a new set when reconnecting.

## Live devices

All device IDs are caller supplied and must start with the matching prefix. Put media files in `workspace/media`; API paths are relative to that directory. `GET /api/v2/instances/{id}/devices/{kind}` queries QEMU's current USB, block, or PCI devices.

| Operation | Method and path | JSON body |
| --- | --- | --- |
| Attach host USB | `POST .../devices/usb-host` | `{"device_id":"me-usbh-1","hostbus":1,"hostaddr":2}` |
| Attach USB image | `POST .../devices/usb-image` | `{"device_id":"me-usbi-1","path":"disk.img","read_only":false}` |
| Attach USB redirection | `POST .../devices/usbredir` | `{"device_id":"me-redir-0"}` |
| Attach ISO drive | `POST .../devices/iso` | `{"device_id":"me-iso-1","path":"installer.iso","bus":"pcie-root-port-0"}` |
| Change ISO | `POST .../devices/iso/me-iso-1/change` | `{"path":"second.iso"}` |
| Eject ISO | `POST .../devices/iso/me-iso-1/eject` | `{"force":false}` |
| Attach network card | `POST .../devices/network` | `{"device_id":"me-net-1","bus":"pcie-root-port-1","model":"virtio-net-pci","mac":"52:54:00:12:34:56"}` |
| Detach any managed device | `DELETE .../devices/{kind}/{device_id}` | None |

For a q35 VM, reserve PCIe root ports in the launch plan before starting QEMU, for example `-device pcie-root-port,id=pcie-root-port-0,chassis=1,slot=1` and a second port with a different ID, chassis, and slot for the network card. The `bus` field must name an available reserved port. The VM needs a USB controller such as `-device qemu-xhci,id=usb0` for USB devices. Network hotplug currently creates a QEMU user-mode network backend. `model` can be `virtio-net-pci`, `e1000`, or `rtl8139`.

Detaching waits up to five seconds for QEMU's `DEVICE_DELETED` event before deleting the associated block, network, or character backend. A response with `"detached":false` means guest removal remains pending; stop the VM before reusing that device ID.
