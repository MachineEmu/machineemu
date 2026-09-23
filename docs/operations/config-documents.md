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
