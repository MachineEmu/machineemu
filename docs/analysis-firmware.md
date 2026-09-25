# Analysis firmware

`malware-analysis-x64` uses the host-provided libvirt OVMF links instead of a
checkout-local Nix store path. Keep the firmware code path stable in the
profile and let the selected image provide the NVRAM seed.

Use these paths for x86 UEFI profiles:

```text
/run/libvirt/nix-ovmf/edk2-x86_64-secure-code.fd
/run/libvirt/nix-ovmf/edk2-x86_64-code.fd
/run/libvirt/nix-ovmf/edk2-i386-vars.fd
```

Windows and analysis profiles use `edk2-x86_64-secure-code.fd` with secure
pflash enabled. General Linux UEFI profiles use `edk2-x86_64-code.fd`. When a
profile needs a fresh variables template rather than an image component, seed it
from `edk2-i386-vars.fd`.

The analysis image still owns its installed firmware variables. That component
comes from the imported base image as `firmware`, so the profile only names the
code FD directly:

```json
"firmware": {
  "loader": {
    "source": {
      "path": "/run/libvirt/nix-ovmf/edk2-x86_64-secure-code.fd"
    },
    "readonly": true,
    "secure": true
  },
  "nvram": {
    "source": {
      "image_component": "firmware"
    }
  }
}
```

After changing firmware paths, validate the profile against the packaged
analysis QEMU:

```sh
cargo run -p machineemu -- validate-profile \
  --profile profiles/malware-analysis-x64.json \
  --qemu ../qemu/.cache/packages/analysis-10.2/bin/qemu-system-x86_64
```
