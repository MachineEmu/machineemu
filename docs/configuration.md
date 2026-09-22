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
  unifi-10.2:
    path: /home/rick/projects-caddy/machineemu/qemu/.cache/qemu-build-10.2.4-unifi
    version: 10.2.4
    target: aarch64-softmmu
  unifi-10.2-analysis:
    path: /home/rick/projects-caddy/machineemu/qemu/.cache/qemu-build-10.2.4-analysis
    version: 10.2.4
    target: x86_64-softmmu
```

`engines.<track>.path` may point directly to a QEMU executable or to a build
directory containing the executable for its `target`. When `machineemu run` is invoked
without an explicit `--qemu`, it selects the engine matching the profile's
`engine.track`, then falls back to `system`. `build_digest` is optional and
can be added after a QEMU rebuild when the build should be pinned.

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
the configured value for one run. A relative configured path is read against
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
