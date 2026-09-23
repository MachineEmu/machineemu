# Image store and portable bundles

MachineEmu uses two representations for images:

* The workspace store is an internal content-addressed cache. It is efficient
  for deduplication and integrity checks, but its digest filenames are not a
  good hand-editing or copy format.
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
component before publishing it into the workspace store, and export recreates
the stable component names. Bundle paths are relative to the bundle directory;
absolute paths and parent-directory traversal are rejected.

The workspace uses editable image manifests alongside its content-addressed
blob store:

```text
<workspace>/
├── metadata.sqlite3
├── blobs/sha256/<digest>
├── images/<image-id>/manifest.json
├── instances/<instance-id>/
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
editing compatibility; portable bundles under `images/` are separate exports
and are not the live workspace configuration.

To migrate an existing workspace:

```sh
target/debug/machineemu migrate-images --workspace ./machineemu-workspace
```

Migration also runs automatically when the updated daemon opens a workspace.
It exports legacy image records without replacing existing valid manifests,
then removes their configuration columns from SQLite. Stop the daemon before
running the standalone migration command. SQLite continues to own instance,
operation, run, and snapshot state, with only image IDs retained for instance
references. Image listing and metadata reads use files exclusively.

This means a profile or image can be copied as one directory, inspected in a
normal file browser, and imported on another workspace without reproducing
opaque digest filenames by hand. Runtime state under `instances/` and
`snapshots/` is deliberately separate from the portable image bundle.

Profiles can set `storage.disk.size`, such as `64GiB`. The planner converts
that value to QEMU's size syntax and the daemon grows the instance overlay
during preparation. Existing overlays are never shrunk.

Select a registered base image independently of the launch profile:

```sh
target/debug/machineemu run malware-analysis-x64 analysis01 --image win11-dev --net bridge:br0
```

When `--image` is omitted, the profile's `image` field selects the registered
base image. `--image` overrides that field for this invocation and binds
the selected disk and NVRAM to the asset names referenced by the profile. It
also supplies the image's TPM seed when the profile enables TPM. The profile
still selects the engine and hardware; image and profile targets must match.
Firmware code is not part of the image record: import it and bind the profile's
loader asset before launching.
The analysis example requires its analysis-capable QEMU engine to be configured
or selected with `--qemu`. The Rust planner encodes the analysis payload and
SMBIOS identity for that engine. Bridge networking requires a privileged QEMU
bridge helper on the host.

Image manifests, portable bundle manifests, and image registration API requests
accept an optional `supported_engine_tracks` array of additional compatible
tracks, for example:

```json
"engine_track": "unifi-10.2",
"supported_engine_tracks": ["unifi-10.2-analysis"]
```

The original `engine_track` remains supported. Older manifests without the
array support only that original track. `run --image` checks the profile's
track against these declarations. `--force` overrides a track mismatch for
that invocation and prints a warning; it does not change the image's declared
compatibility or bypass architecture, asset, or QEMU capability checks.

An existing instance with a different image is rejected. Choose a new instance
name, or explicitly use `--fresh` to discard and recreate its state. Neither
the stored profile nor the image record is changed by `--image`. Without the
flag, existing profile asset bindings and image selection remain unchanged.

To import a `vmmanager-sh` base, point the importer at the immutable base
directory, not at an instance directory under `vm-state`:

```sh
cargo run -p machineemu -- import-vmmanager-base \
  --workspace ./machineemu-workspace \
  --source "$HOME/.vm-base/win11-dev" \
  --image-id win11-dev \
  --engine-track unifi-10.2 \
  --target x86_64-softmmu \
  --export-bundle ./images/win11-dev
```

The importer reads `disk.qcow2`, `OVMF_VARS.fd`, and
`tpm/tpm2-00.permall`. It deliberately excludes TPM lock/PID files and does
not import `vm-state/*/overlay.qcow2`, which is writable instance state.

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

A named profile resolves from `<workspace>/profiles/` before the catalog.
An explicit profile path takes precedence over both. Use the UDM importer
for this bundle; the generic PC importer requires a `disk` component.
