# Instance files, templates, and images

An image supplies shared base assets. A profile is an optional creation template.
An instance owns its effective launch configuration and writable state. Starting
an instance never resolves its shared template again.

Each instance has exactly one authoritative document:

```text
<workspace>/instances/<id>/
├── instance.json          # or instance.yaml / instance.yml
├── overlay.qcow2         # when the machine has a writable disk
├── OVMF_VARS.fd           # when the machine has firmware variables
├── tpm/                  # when the machine has TPM state
└── profile.json          # generated helper input; not an editable config
```

Creation writes JSON. To use YAML, convert `instance.json` to `instance.yaml`
and remove the JSON file. Multiple documents are an error. Stop the instance
before editing. `start` reads the document on every launch; malformed or missing
files fail explicitly, without falling back to SQLite or a shared profile.
Changes never rewrite an already running QEMU process.

The document contains `schema_version: 1`, `instance_id`, `image_id`,
`profile_id` (template provenance, or `custom`), `auto_remove`, an optional
copied `profile`, and the resolved `launch_plan`. The launch plan includes QEMU
arguments, helper commands, QMP/log paths, and disk/NVRAM/TPM seed references.
Large disk and firmware bytes stay in their referenced files. Structured plan
paths are workspace-relative; paths embedded in QEMU/helper arguments retain
their resolved absolute locations. This is a local machine configuration, not
a portable disk bundle.

`launch_plan` controls the next launch. The copied `profile` supplies helper
settings and creation provenance; changing it alone does not regenerate QEMU
arguments. Change the launch plan when changing QEMU hardware settings. Shared
profile edits only affect newly created instances. Instance, image, and template
IDs must still match the workspace's instance registration.

Create from a reusable template or a complete document:

```sh
machineemu create malware-analysis-x64 analysis01 --image win11-dev
machineemu create --file instance.yaml
machineemu start analysis01
```

`create --file` requires an instance ID, image ID, schema version and launch plan;
no shared profile is required. Omitted `profile_id` is recorded as `custom`.
`run --file instance.yaml` creates and starts the instance. Creation rejects an
existing ID. For a different machine, author a new document with its own ID,
owned paths, and guest identity; merely changing the ID is not a disk clone.

The CLI and API edit the same on-disk source:

```sh
machineemu show instance analysis01 > instance.yaml
machineemu update instance analysis01 --file instance.yaml
machineemu show profile malware-analysis-x64 > profile.yaml
machineemu update profile malware-analysis-x64 --file profile.yaml
machineemu show image win11-dev > image.yaml
machineemu update image win11-dev --file image.yaml
```

Add `--json` to `show` for JSON. The instance API includes an opaque `revision`
derived from document content, so a direct file edit invalidates an older API
editor. This is separate from the runtime lifecycle revision. The token is
ignored when loading an exported document from disk. API updates use atomic file
replacement, preserve the selected JSON/YAML format, and reject edits while the
instance is active. The API also rejects changes to IDs, disposal policy, and
existing disk/NVRAM/TPM preparation. Hand editing those state relationships does
not perform an image reset or move any disk files.

| Document | Read and replace |
| --- | --- |
| Instance | `GET` / `PUT /api/v2/instances/{id}/config` |
| Shared template | `GET` / `PUT /api/v2/profiles/{id}` |
| Image manifest | `GET` / `PUT /api/v2/images/{id}` |

The API returns JSON by default or YAML for `Accept: application/yaml`, and
accepts YAML on `PUT` with `Content-Type: application/yaml`. Template updates
write a workspace override at `profiles/{id}.json`. Image updates replace
`images/{id}/manifest.json`; existing instance disks retain their backing files.

## One-time export from SQLite

Stop the old daemon, then run:

```sh
machineemu migrate-instances --workspace ./machineemu-workspace
```

Opening the workspace with the updated daemon also performs this migration.
It exports the old saved profile and launch plan into each instance document,
checks all exported documents, then drops the legacy `instance_configuration`
and `instance_launch` tables. Existing instance documents are preserved. Export
failure leaves the old tables intact for retry. SQLite continues to own lifecycle,
instance registration, operations, runs and snapshots. There is no legacy
configuration fallback, inline start plan, or daemon-level profile launch map.

## Hardware flags

`create`, `run`, and `config INSTANCE` share hardware overrides. They also work
with `create --file` and `run --file`. `config` edits the saved launch plan through
the revision-checked API; stop the instance first, then start it after editing.
The copied template remains provenance; the launch plan is authoritative.

```sh
machineemu create win11-dev desktop --cpus 4 --memory 8GiB --h264
machineemu config desktop --cpus 8 --memory 16GiB
machineemu config desktop --vnc auto --vnc-password password
machineemu config desktop --h264
machineemu config desktop --network bridge:br0 --network1 bridge:br2 \
  --network2 host --portfwd '2=tcp:127.0.0.1:2222-:22'
machineemu config desktop --network2 off
machineemu config desktop --iso /path/to/installer.iso
machineemu config desktop --iso off
```

- `--h264` selects D-Bus with GL, `virtio-vga-gl`, and USB tablet input with an
  xHCI controller, and disables VNC. Reapplying it does not duplicate the tablet.
- `--vnc auto`, `--vnc 5901`, or `--vnc off` controls loopback VNC. Enabling VNC
  replaces the GL card with standard VGA. `--vnc-password` accepts 1–8 bytes and
  stores them in an owner-only file under the local workspace's `secrets/`;
  the saved arguments reference the file. `--vnc-password-file` is also supported.
- `--network` (alias `--network0`) and `--network1` through `--network3` select
  slots `net0` through `net3`. `host` means QEMU user networking/NAT. Existing NIC
  models and MACs are retained; new slots get stable per-instance MACs.
  `--portfwd SLOT=RULE` is repeatable and only applies to a host/user backend.
  Selecting a backend replaces its old forwarding rules; omitting a slot leaves
  it unchanged. `off` removes the slot. The older `--net` profile override remains
  available on template-based creation.
- `--iso PATH` attaches a read-only CD-ROM; replacing or ejecting it preserves
  the separate cloud-init seed and writable disks. Paths refer to the local host
  used by the daemon.
- `--cpus N` replaces the CPU count and resets any explicit socket/core topology.
  `--memory SIZE` accepts positive whole MiB/GiB/TiB quantities, or bare MiB.

## Firmware boot settings

Profiles support these `boot` fields (milliseconds for both timeouts):

```yaml
boot:
  from: disk
  once: cdrom
  menu: true
  splash: assets/analysis/neutral-boot-logo.bmp
  timeout: 2500
  strict: true
  reboot_timeout: -1
```

`from` sets persistent boot selection and `once` overrides the first boot; both
accept `disk`, `cdrom`, `network`, or `floppy`. The selected device must be
available to the guest. `splash` names an existing local image file, resolved
against the CLI working directory and saved as an absolute path. `timeout` is
the splash duration (`splash-time`); `reboot_timeout` is the delay after boot
failure, with `-1` disabling automatic retry. `menu` and `strict` are booleans.
These settings are passed together in QEMU's `-boot` option. Their visible
behavior depends on the guest firmware; see the
[QEMU boot-option documentation](https://www.qemu.org/docs/master/system/qemu-manpage.html).
Direct boot through `kernel`, `initrd`, `dtb` asset references and an `append`
command line remains supported.
