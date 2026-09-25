# MachineEmu configuration

The client and server can share one YAML file. MachineEmu looks for
`./machineemu.yaml` first, then `$XDG_CONFIG_HOME/machineemu/config.yaml`, or
`~/.config/machineemu/config.yaml` when `XDG_CONFIG_HOME` is unset. An explicit
`--config` path takes precedence for the daemon.

The server section may be omitted on a host that only connects to a remote
daemon:

```yaml
server:
  workspace: ./machineemu-workspace
  unix_socket: ./machineemu-workspace/control.sock
  # Use listen/bearer_token instead when exposing TCP.
  # listen: 127.0.0.1:8787
  # bearer_token: replace-me

client:
  unix_socket: ./machineemu-workspace/control.sock
  # endpoint: 192.0.2.20:8787
  # token: replace-me

engines:
  system:
    path: /run/current-system/sw/bin
    version: 10.2.4
  qemu-10.2-unifi:
    path: /home/rick/projects-caddy/machineemu/qemu/.cache/packages/unifi-10.2
    version: 10.2.4
    target: aarch64-softmmu
    build_digest: sha256:d569f3e0e61079430cdc02d944a3aa252ff378bd9eb965adeb95b31af82fd6e1
  qemu-10.2-analysis:
    path: /home/rick/projects-caddy/machineemu/qemu/.cache/packages/analysis-10.2
    version: 10.2.4
    target: x86_64-softmmu
    build_digest: sha256:a01e24d1cc568e842bea126f3b516cec8b20885d350370190904177b21fb31c5
```

`engines.<track>.path` may point directly to a QEMU executable or to a packaged
engine root containing `bin/qemu-system-<architecture>`. When `machineemu run`
is invoked without an explicit `--qemu`, it selects the first configured engine
listed by the profile's `engine` array, then falls back to `qemu-system` only
when the profile does not name an engine. This choice is saved in the new
instance's launch plan; changing an engine setting does not replan existing
instances. `build_digest` is optional and can be added after a QEMU rebuild
when the build should be pinned.

An engine without `target` is a multi-target engine. The system entry above
selects `/run/current-system/sw/bin/qemu-system-<architecture>` from the
profile target.

## Helpers

A profile with a `tpm` section is launched alongside `swtpm`, which is a host
program rather than part of the engine bundle. It is resolved through `PATH`
unless a path is configured:

```yaml
helpers:
  swtpm: /run/current-system/sw/bin/swtpm
  qemu_bridge_helper: /run/wrappers/bin/qemu-bridge-helper
```

`machineemu run --swtpm`, or `MACHINEEMU_SWTPM` in its environment, overrides
the configured value when creating the instance. Its resolved helper command
is saved with that instance. A relative configured path is read against
the configuration file, as engine paths are. When the helper cannot be started
the run is rejected with the executable that was not found, so a host that
never installed swtpm says so instead of reporting a missing file.

`qemu_bridge_helper` is named in the `-netdev bridge` argument, and
`--bridge-helper` or `MACHINEEMU_BRIDGE_HELPER` overrides it. Only a
privileged copy can open a tap device: QEMU otherwise runs the helper beside
its own executable, which for an engine built from source holds neither the
setuid bit nor `cap_net_admin`, and the run fails with "failed to create tun
device: Operation not permitted". The helper also reads `/etc/qemu/bridge.acl`,
which must carry an `allow <bridge>` line for the bridge the profile names.

When the server uses `unix_socket`, the daemon creates the socket with mode
`0600` and requests received on that socket do not require bearer auth. TCP
listeners always require `bearer_token`. Relative paths are resolved relative
to the configuration file.

## Verifying local engine packages

Import packaged engines through the daemon, using the same workspace and
progress stream workflow as base-image imports:

```sh
machineemu install-engines --workspace ./machineemu-workspace --source \
  /absolute/path/analysis-10.2-x86_64-linux.tar.gz \
  /absolute/path/unifi-10.2-x86_64-linux.tar.gz
```

`import-engine` is an alias for `install-engines`. `--daemon` and `--token`
select the daemon as with `import-vmmanager-base`. Source paths refer to files
on the daemon host; archives are not uploaded. Prefer absolute source paths.
The daemon verifies `SHA256SUMS` and the manifest's executable hashes before
registering the bundle under `engines/<track>/<payload-digest>/` in its workspace.
The archive must contain one directory, regular files and directories only,
with a clean `engine-build.json` manifest and checksums for every payload file.

The workspace's `engines/registry.json` records imported tracks. New instances
can select them through the profile's `engine` array; explicitly configured
engines take precedence over imported tracks with the same name. Re-importing
an unchanged bundle reuses its verified installation. A different bundle gets
a separate directory and updates the registry without changing existing instance
launch plans. No host configuration file is rewritten.

Local engines should point at the packaged roots emitted by the sibling QEMU
build, such as `../qemu/.cache/packages/analysis-10.2`. The package carries its
runtime libraries and firmware, so `LD_LIBRARY_PATH` is not required.
Do not keep stale wrapper launchers under `machineemu-workspace/engines`; the
saved instance launch plan should record the actual executable selected from the
configured engine path.

Verify the configured analysis engine after rebuilding QEMU:

```sh
cargo run -p machineemu -- qemu-options \
  --qemu ../qemu/.cache/packages/analysis-10.2/bin/qemu-system-x86_64
```
