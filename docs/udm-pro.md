# Booting UDM Pro

The portable base and its default lab template live in
[`images/udm-pro/`](../images/udm-pro/README.md). Import them with
`python3 scripts/import-udm-pro.py images/udm-pro`, then launch by name with
`machineemu run udm-pro-lab udmlab`.

The `udm-pro` template boots a prepared UDM Pro firmware bundle using the
`qemu-10.2-unifi` AArch64 engine. Import creates a workspace template; the instance
gets its own document when created.

Build the Rust CLI and import the prepared bundle (not a vendor firmware
update file):

```sh
cargo build --workspace --locked
python3 scripts/import-udm-pro.py /path/to/prepared-bundle
target/debug/machineemu run machineemu-workspace/profiles/udm-pro.json udm01 --net none
```

The bundle must contain `Image`, `initramfs.cpio`, `boot.img`, and `spi.img`.
Use the pristine bundle directory, not its previously booted `run/` directory.
The importer hashes and copies these files into the workspace and writes a
local profile with their asset references. Firmware and host paths stay out
of bundled profiles. Configure the `qemu-10.2-unifi` engine as described in
[configuration](configuration.md).

This template boots offline, with four CPUs, 2 GiB RAM, and serial output in
`machineemu-workspace/instances/udm01/serial.log`. The run command preserves
template networking by default; `--net none` can explicitly disable it.
It omits `-cpu` so the board retains the AL324 CPU identity, and omits the
vendor DTB so QEMU supplies its matching device tree.

Both storage backends use QEMU temporary snapshots. Writes are discarded on
process exit, so each start gets pristine GPT and SPI contents; this profile
is not intended for persistent configuration or disk snapshots. The kernel
command line masks the first-boot reboot and vendor Bluetooth service.
LCD, Bluetooth, and the two-bridge topology are provided by the
`udm-pro-lab` profile below.

For an engine built in a Nix development shell, run it with that shell's
runtime libraries available, or configure a wrapper executable as the engine
path. QEMU's temporary snapshot directory must also be writable (`TMPDIR`
can point at the workspace's `staging/` directory). A wrapper supplied with
`--qemu` must use an absolute path.

The local smoke run with prepared firmware 5.1.19 reached systemd and
identified `UDMPRO.al324.v5.1.19`. It was still starting the UI status DB
daemon at the end of the check; a login prompt and application readiness
have not yet been verified.


## WAN, LAN, LCD, and Bluetooth

`udm-pro-lab` uses the board's fixed PCI order:

| Slot | Guest interface | Connection |
| --- | --- | --- |
| 0 | eth9 | Disabled |
| 1 | eth8 (primary WAN) | br0 |
| 2 | eth10 (SFP+ LAN) | br10 |
| 3 | switch0 | Disabled |

The disabled slots have private empty hubs so they do not shift the connected
ports. Each instance gets four distinct, stable MAC addresses. This connects
QEMU's interfaces; IP addressing, routing and DHCP service are guest settings.
The firmware's internal switch ports are not connected by this profile.

Import with the lab profile to install the H4 Bluetooth hook in the initramfs:

```sh
python3 scripts/import-udm-pro.py /path/to/prepared-bundle --profile udm-pro-lab

target/debug/machineemu run machineemu-workspace/profiles/udm-pro-lab.json udmlab \
  --net profile --bridge-helper /run/wrappers/bin/qemu-bridge-helper
```

The bridges must already exist and the privileged helper must allow both in
its bridge configuration (on this host, `/etc/qemu/bridge.conf` contains
`allow br0` and `allow br10`). `--net profile` is the default. Supplying
`--net bridge:br0` replaces the entire four-port mapping with one connection.

For this checkout's locally prepared engine wrapper, add:

```sh
  --qemu "$PWD/machineemu-workspace/udm-qemu"
```

The daemon starts and stops the simulated Bluetooth helper with the instance.
Its socket and control socket live under `instances/udmlab/`. The patched
initramfs installs
`qemu-btattach.service`, which runs `btattach -B /dev/ttyS1 -P h4`, and sets the
`UDMPRO` Bluetooth shortname. The vendor BCSP/GPIO controller service remains
masked. This is a simulated controller with simulated BLE peers; it does not
broadcast through a physical host radio. Use a distinct controller address
for each concurrently running instance.

The emulated LCD is attached to xHCI on `pcie-external` at slot 9. View its
semantic screen state in another terminal:

```sh
python3 scripts/compat/lcm-view.py \
  machineemu-workspace/instances/udmlab/lcm-events.sock
```

`--json` prints complete display snapshots. The viewer waits until the guest
sends a display update. `lcm-input.sock` carries semantic touch input. The
Rust daemon does not yet connect these sockets to the browser LCD panel, and
the terminal viewer is not a pixel-exact rendering.

Validation: a paused QEMU launch confirmed WAN on `br0`, LAN on `br10`, a
`UniFi LCM` USB device, and connectable LCD and Bluetooth sockets. Guest
Bluetooth initialization, network connectivity and a ready LCD screen have
not yet been verified.

## Console and logs

```sh
target/debug/machineemu logs udmlab          # Last 100 console lines
target/debug/machineemu logs udmlab -f       # Follow output
target/debug/machineemu logs udmlab --source stderr
target/debug/machineemu serial udmlab        # Interactive UART; Ctrl-] detaches
```

These commands read the local workspace selected by configuration or
`--workspace`. `logs` falls back to QEMU stderr when no console output was
produced, so it also diagnoses failed starts. `--source serial`, `stderr`, or
`stdout` selects an explicit stream; `-n` controls the number of lines.

The UDM templates use `devices.serial=socket`, with output also saved to
`serial.log`. Ctrl-C is forwarded to the guest; Ctrl-] disconnects without
stopping QEMU. Existing instances keep their saved launch settings; editing a
shared template does not reconfigure them.
