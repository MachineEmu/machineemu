# UDM Pro firmware image

This is the portable UDM Pro base image, prepared from firmware
`UDMPRO.al324.v5.1.19.3fbc1da.260613.0944`.

```text
udm-pro/
├── manifest.json          # Component hashes and firmware identity
├── profile.json           # Default lab profile template, without asset hashes
└── components/            # Local firmware files; ignored by Git
    ├── Image              # Linux kernel
    ├── initramfs.cpio     # Prepared initramfs, including the H4 Bluetooth hook
    ├── boot.img           # Pristine raw boot disk, with embedded rootfs
    └── spi.img            # Pristine raw SPI image
```

The base uses the existing diagnostic lab firmware preparation, including its
lab identity and factory-authentication modifications. It is not an untouched
vendor update. `manifest.json` records the original firmware's SHA-256 and the
exact prepared components' SHA-256 values. Private signing keys are not part
of the bundle.

The template names `udm-pro` as its base image and `udm-pro-lab` as its
profile ID, so launch settings and firmware identity stay separate.

The default profile has four CPUs, 2 GiB RAM, WAN `eth8` on `br0`, SFP+ LAN
`eth10` on `br10`, an interactive serial socket with logging, LCD, and the H4
Bluetooth UART. QEMU discards boot/SPI writes when the process exits.

Import this directory and bind its template to your workspace:

```sh
python3 scripts/import-udm-pro.py images/udm-pro
```

This verifies all four components and writes
`machineemu-workspace/profiles/udm-pro-lab.json`. The repository remains a source
of unbound templates; a named `run` selects an imported workspace profile first:

```sh
target/debug/machineemu run udm-pro-lab udmlab \
  --bridge-helper /run/wrappers/bin/qemu-bridge-helper
```

Set the engine in `machineemu.yaml`, or use `--qemu` for an explicit executable.
On the current host the existing wrapper can be selected with
`--qemu "$PWD/machineemu-workspace/udm-qemu"`.

Edit `profile.json` to change defaults for subsequent imports, or edit the
workspace profile for local changes. No component hashes need to be copied
into either template by hand. Importing again regenerates the workspace
profile; it does not change running guests. To choose the offline bundled
template instead, import with `--profile udm-pro`.

Copy the whole directory, including `components/`, to move this image to
another host. A Git checkout includes only the metadata and template.
Recreate a bundle from a **prepared firmware directory** with:

```sh
python3 scripts/import-udm-pro.py /path/to/prepared-firmware \
  --profile udm-pro-lab --export-bundle images/udm-pro
```

Export requires a new destination and refuses to overwrite an existing base.
The input must already contain `Image`, `initramfs.cpio`, `boot.img`, and
`spi.img`; raw vendor update extraction is a separate preparation step.

Use the UDM importer above for this four-component image; the generic PC
image importer expects a `disk`/OVMF/TPM bundle. See
[UDM Pro operations](../../docs/udm-pro.md) for Bluetooth and LCD commands.
