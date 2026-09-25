# Image store and portable bundles

MachineEmu uses two representations for images:

* The workspace image package is the live, editable local representation. It
  keeps named files under `images/<image-id>/components/` and records their
  digests in `manifest.json`.
* A portable image bundle is the human-readable interchange format. It keeps
  the image identity and component names in `manifest.json`, with the actual
  files beside it.

An exported bundle has this shape:

```text
win11-dev/
├── manifest.json
└── components/
    ├── disk.qcow2
    ├── firmware.fd       # optional
    └── tpm-state          # optional
```

`manifest.json` is intentionally small and readable:

```json
{
  "schema_version": 1,
  "image_id": "win11-dev",
  "engine_track": "qemu-10.2",
  "target": "x86_64-softmmu",
  "components": {
    "disk": {
      "path": "components/disk.qcow2",
      "sha256": "sha256:..."
    },
    "firmware": {
      "path": "components/firmware.fd",
      "sha256": "sha256:..."
    },
    "tpm_state": {
      "path": "components/tpm-state",
      "sha256": "sha256:..."
    }
  }
}
```

The digest is verification metadata, not the filename. Import verifies every
component before publishing it into the workspace image package, and export
recreates the stable component names. Bundle paths are relative to the bundle
directory; absolute paths and parent-directory traversal are rejected.

The workspace keeps image manifests and named components together:

```text
<workspace>/
├── metadata.sqlite3
├── images/<image-id>/
│   ├── manifest.json
│   └── components/
│       ├── disk.qcow2
│       ├── firmware.fd       # optional
│       └── tpm-state          # optional
├── instances/<instance-id>/instance.json
├── snapshots/<snapshot-id>/
└── staging/
```

The workspace image manifest is the source of truth for image configuration.
For example, edit `machineemu-workspace/images/win11-dev/manifest.json` to add
an engine to `supported_engine_tracks`. The CLI and daemon read the file on
each lookup, so no re-registration or restart is required after an edit.
`machineemu images` shows the editable path. These workspace manifests contain
`disk_sha256`, optional `firmware_sha256` and `tpm_state_sha256`, `target`,
`engine_track`, and optional `supported_engine_tracks`. The `image_id` must
match the enclosing directory name. Keep the component digests intact when
editing compatibility.

To migrate an existing workspace:

```sh
target/debug/machineemu migrate-images --workspace ./machineemu-workspace
```

Migration also runs automatically when the updated daemon opens a workspace.
It exports legacy image records without replacing existing valid manifests,
then removes their configuration columns from SQLite. Stop the daemon before
running the standalone migration command. Instance configurations also live in
`instances/<id>/instance.json` (or YAML). SQLite owns instance lifecycle,
operation, run, and snapshot state, with image IDs retained for references.
Image listing and metadata reads use files exclusively.

An image can be copied as one bundle directory and inspected in a normal file
browser without reproducing opaque digest filenames by hand. A profile is a
reusable template file; each instance owns a separate configuration document
and writable state. Runtime state under `instances/` and `snapshots/` is
separate from the portable image bundle.

Profiles can set `storage.disk.size`, such as `64GiB`. The planner converts
that value to QEMU's size syntax and the daemon grows the instance overlay
during preparation. Existing overlays are never shrunk.

Select a registered base image independently of the launch profile:

```sh
target/debug/machineemu run malware-analysis-x64 analysis01 --image win11-dev --net bridge:br0
```

When `--image` is omitted, the profile's `image` field selects the registered
base image. `--image` overrides that field for this invocation and binds
the selected disk and NVRAM component paths to the asset names referenced by
the profile. It also supplies the image's TPM seed when the profile enables
TPM. The profile still selects the engine and hardware; image and profile
targets must match. Firmware code is not part of the image record: keep it as
a normal profile asset and bind the profile's loader asset before launching.
The analysis example requires its analysis-capable QEMU engine to be configured
or selected with `--qemu`. The Rust planner encodes the analysis payload and
SMBIOS identity for that engine. Bridge networking requires a privileged QEMU
bridge helper on the host.

Image manifests, portable bundle manifests, and image registration API requests
accept an optional `supported_engine_tracks` array of additional compatible
tracks, for example:

```json
"engine_track": "qemu-10.2-unifi",
"supported_engine_tracks": ["qemu-10.2-analysis"]
```

The original `engine_track` remains supported. Older manifests without the
array support only that original track. `run --image` checks the profile's
track against these declarations. `--force` overrides a track mismatch for
that invocation and prints a warning; it does not change the image's declared
compatibility or bypass architecture, asset, or QEMU capability checks.

`run` requires a new instance name. To reuse an existing instance, call
`start`; to discard its state and create it from a selected template and image,
use `run --fresh --image IMAGE`. Changing a template or image manifest does not
change an existing instance's saved launch plan.

To import a `vmmanager-sh` base, point the importer at the immutable base
directory, not at an instance directory under `vm-state`. By default,
MachineEmu reads `~/.vm-base/<image-id>`:

```sh
cargo run -p machineemu -- import-vmmanager-base \
  --workspace ./machineemu-workspace \
  --image-id win11-dev \
  --engine-track qemu-10.2-unifi \
  --target x86_64-softmmu \
  --export-bundle ./images/win11-dev
```

Use `--source /path/to/base` when the base lives somewhere else.

The importer reads `disk.qcow2`, `OVMF_VARS.fd`, and
`tpm/tpm2-00.permall`. It deliberately excludes TPM lock/PID files and does
not import `vm-state/*/overlay.qcow2`, which is writable instance state.
The CLI starts a daemon-side import job and follows its SSE progress stream.
If the event stream disconnects, it reconnects with `Last-Event-ID` and falls
back to the job status endpoint before continuing.

## UDM Pro firmware base

[`images/udm-pro/`](../images/udm-pro/README.md) contains a prepared firmware
base with `kernel`, `initrd`, `boot`, and `spi` components. Its `profile.json`
is the default launch template and deliberately contains no asset hashes.
The UDM importer verifies the portable component hashes, imports the files,
and binds that template to the workspace:

```sh
python3 scripts/import-udm-pro.py images/udm-pro
target/debug/machineemu run udm-pro-lab udmlab
```

A named profile resolves from `<workspace>/profiles/` before the repository's `profiles/`.
An explicit profile path takes precedence over both. Use the UDM importer
for this bundle; the generic PC importer requires a `disk` component.
