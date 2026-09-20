# UniFi compatibility daemons

These are operator-started, session-scoped compatibility helpers. They own
only their private Unix sockets and simulated medium state; they do not load
kernel modules, modify the host Bluetooth stack, or configure networking.

- `hci_simulator.py` provides a modelled H4 Bluetooth controller and JSON
  datagram control socket.
- `hwsim_adapter.py` bridges raw mac80211_hwsim frames with bounded RF
  settings and a JSON datagram control socket.
- `unifi_ble_tunnel.py` provides the BLE tunnel used by the Bluetooth helper.
- `run-isolated-hwsim.sh` is the explicitly operator-gated namespace wrapper.

Run them only in an isolated lab namespace with paths below the configured
session runtime directory. Host setup and privilege escalation remain outside
these daemons.
