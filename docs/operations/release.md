# Operator release checklist

MachineEmu releases are assembled from two independently versioned artifacts:

1. an operator package built from this repository; and
2. an immutable QEMU engine bundle built by `MachineEmu/qemu`.

Build and test the operator package with:

```sh
python -m pip install --upgrade build
python -m build
python -m pip install --no-deps dist/*.whl
machineemu --help
```

Before publishing a release, verify that `release-set.json` names the exact
engine manifest and `build_digest` that operators will install. The engine
manifest must have `dirty_source: false`, the release entry must set
`require_executable_hashes: true`, and the bundle executable must exist with a
matching SHA-256 digest for every advertised target. Run both checks from the
two repositories:

```sh
python scripts/check_release_set.py release-set.json
python ../qemu/scripts/validate_engine_bundle.py /path/to/engine-build.json
```

The package does not bundle QEMU binaries. Install the engine bundle separately,
then use `profile-check` or `session-create` with the matching release set.
This keeps the operator package small and makes engine updates independently
auditable.
