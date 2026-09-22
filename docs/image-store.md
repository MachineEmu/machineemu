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

The internal workspace layout remains private:

```text
<workspace>/
├── metadata.sqlite3
├── blobs/sha256/<digest>
├── images/
├── instances/<instance-id>/
├── snapshots/<snapshot-id>/
└── staging/
```

This means a profile or image can be copied as one directory, inspected in a
normal file browser, and imported on another workspace without reproducing
opaque digest filenames by hand. Runtime state under `instances/` and
`snapshots/` is deliberately separate from the portable image bundle.

Profiles can set `storage.disk.size`, such as `64GiB`. The planner converts
that value to QEMU's size syntax and the daemon grows the instance overlay
during preparation. Existing overlays are never shrunk.

To import a `vmmanager-sh` base, point the importer at the immutable base
directory, not at an instance directory under `vm-state`:

```sh
cargo run -p machineemu-plan -- import-vmmanager-base \
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
