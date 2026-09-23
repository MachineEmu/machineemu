# MachineEmu

MachineEmu is a local research lab for virtual machines and device firmware.
This repository owns the runtime, API, CLI, browser client and catalog. It
consumes immutable engine bundles produced by `machineemu/qemu`.

The repository is licensed under AGPL-3.0-or-later. Third-party and restricted
inputs retain their own licensing and distribution requirements.

## Rust development

The Rust implementation uses two packages: `machineemu-core` for planning,
storage and runtime services, and `machineemu` for the CLI and daemon.

```sh
cargo build --workspace --locked
cargo run -p machineemu -- --help
cargo test --workspace --locked
```

List registered images and available named profiles:

```sh
cargo run -p machineemu -- images
cargo run -p machineemu -- profiles
# Select another workspace or emit JSON:
cargo run -p machineemu -- images --workspace ./my-workspace --json
cargo run -p machineemu -- profiles --workspace ./my-workspace --json
```

Both commands default to `./machineemu-workspace` and leave it unchanged.
Images come from editable `images/<image-id>/manifest.json` files in the
workspace. Existing SQLite image metadata can be migrated with
`cargo run -p machineemu -- migrate-images` (also automatic when the updated
daemon opens the workspace). Profiles combine its `profiles/*.json`
with `./catalog/profiles/*.json`; workspace profiles take precedence for the
same filename, as they do for `machineemu run`.

Create a durable VM once, then start and stop its runs while retaining its disk
and guest identity:

```sh
machineemu create PROFILE INSTANCE --image IMAGE
machineemu start INSTANCE
machineemu stop INSTANCE
machineemu restart INSTANCE
machineemu rm INSTANCE
```

`machineemu run PROFILE INSTANCE` combines create and start. For a disposable
VM, `machineemu run --rm PROFILE INSTANCE` removes the new instance after its
terminal run and helper cleanup; it refuses an existing instance. `run` on an
existing stopped instance remains available temporarily and prints a
deprecation message. See [instance lifecycle](docs/operations/instance-lifecycle.md).

For a VNC-enabled profile, set `devices.vnc` to `{"port":"auto"}` or
`{"port":5901}`. `machineemu run PROFILE INSTANCE --vnc auto` overrides the
profile port; `--vnc 5901` selects a fixed port, and `--vnc none` disables VNC
for that run. VNC listens on `127.0.0.1` and `run` prints the chosen port.
To require a password, create a file containing 1–8 bytes with no newline,
set its permissions to `0600`, and pass `--vnc-password-file /path/to/file`.
You can also set `devices.vnc.password_file` in the profile; relative paths
there are resolved from the profile file's directory. QEMU needs a crypto
backend with DES support for VNC passwords. The analysis QEMU build must enable
libgcrypt, GnuTLS, or Nettle before using this option.

`console.uart: true` enables an interactive socket at the instance's
`serial.sock`; connect with `machineemu serial INSTANCE`. A running instance
must be restarted for profile or port changes to take effect.

`machineemu inspect INSTANCE` shows what an instance is running: the QEMU and
helper processes (swtpm), every socket they listen on or have connected, with
its role (QMP, relay, VNC, serial, gdb, TPM), and the hardware the guest sees
over QMP: machine, CPUs, memory, PCI, block devices, NICs, USB, TPM and
chardevs. A stopped instance shows its daemon record and state directory.
`--json` prints the same report for scripts. It is Linux-only, since sockets
are read from `/proc`.

`machineemu analysis-target INSTANCE...` points the patched `kvm_intel` at one
or more analysis instances (pods) by writing their QEMU thread-group ids to the
module. It targets several pods at once through the module's
`analysis_target_tgids` array, replacing the set by default or extending it with
`--add`, and `--clear` detaches every pod. Loading the patched module and the
write itself need root; the write goes through root-agent when available, else
sudo. A single-target module (only `analysis_target_tgid`) still works for one
pod. See [`qemu/kernel/analysis-kvm/README.md`](../qemu/kernel/analysis-kvm/README.md).

Every `machineemu run` creates a separate QMP relay socket for external
applications while MachineEmu retains its internal QMP connection. The socket
is under `<workspace>/s/<instance-hash>.relay.qmp`, and `run` prints its
absolute path. To choose a different path:

```sh
machineemu run malware-analysis-x64 analysis01 --image win11-dev \
  --qmp-socket /tmp/analysis01.qmp
```

An explicit socket path must be unused, have an existing parent directory,
and fit the Unix socket path limit. Give the printed path to the external
QMP client; it must perform the normal QMP capability negotiation. The socket
is created on launch, so existing runs need a restart to get one.

Each instance has a saved profile and resolved launch plan. Use
`machineemu show instance INSTANCE` to export them as YAML, then
`machineemu update instance INSTANCE --file instance.yaml` while it is stopped.
The same commands accept `profile` and `image`; see
[document operations](docs/operations/config-documents.md).
SQLite commits the instance profile, plan and revision together. The file at
`<workspace>/instances/<instance>/profile.json` is a derived helper cache.
`machineemu start INSTANCE` uses the saved launch plan; changing the profile in
the document does not automatically replan it. Use `machineemu run` during the
compatibility period to replan an existing stopped instance explicitly.
Shared catalog edits do not change existing instances. `--fresh --image IMAGE`
removes the instance and creates new settings from the selected shared profile.

Tests require `qemu-img` for overlay preparation. See the
[crate map](crates/README.md) for module boundaries and validation commands.

The migration is in progress. Python and the existing browser remain in the
repository while Rust replacement gates are completed; the Rust pilot is not
yet a full runtime cutover.

## Documentation

- [Configuration](docs/configuration.md)
- [Image store and portable bundles](docs/image-store.md)
- [Analysis firmware](docs/analysis-firmware.md)
- [Domain model](docs/domain-model.md)
- [Rust migration plan](docs/migration/rust.md)
- [First usable milestone](docs/migration/rust-first-milestone.md)

Build and test this repository without a sibling checkout. Engine installations
are explicit; releases record the engine build digest in `release-set.json`.
