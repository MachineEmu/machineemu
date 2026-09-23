# Analysis firmware

`malware-analysis-x64` boots a rebuilt OVMF, not the stock one from the dev
shell. The flake builds it as `analysis-ovmf`, and the profile pins its code and
variables images by digest as `firmware_code` and `firmware_vars`.

```sh
nix build .#analysis-ovmf.fd
ls result-fd/FV   # OVMF_CODE.fd, OVMF_VARS.fd, OVMF_VARS.ms.fd, ...
```

It is Nixpkgs' `OVMF` with Secure Boot, TPM support and the Microsoft-enrolled
variables template, plus three changes QEMU cannot make at run time. The
derivation is `mkAnalysisOvmf` in [`flake.nix`](../flake.nix); it was carried
over from `unifi-qemu/nix/flake.nix`, where it was driven by
`QEMU_ANALYSIS_*` environment variables under `--impure`. Here it is pure and
reads the profile directly.

## What changes

**BGRT identity.** QEMU builds no BGRT. The table comes from OVMF's
`BootGraphicsResourceTableDxe`, which fills its ACPI header from build-time
PCDs (`PcdAcpiDefaultOemId`, `PcdAcpiDefaultOemTableId`,
`PcdAcpiDefaultOemRevision`, `PcdAcpiDefaultCreatorId`,
`PcdAcpiDefaultCreatorRevision`) that default to `INTEL`/`EDK2` with an empty
creator. The profile's `analysis.acpi` block reaches every table QEMU builds,
so a stock BGRT would be the one table that disagrees. The build appends a
`[PcdsFixedAtBuild]` section to `OvmfPkg/OvmfPkgX64.dsc` with the values from
`profiles/malware-analysis-x64.json`. The OEM ID is padded to six
bytes. The OEM table ID and creator ID are packed little-endian into eight and
four bytes.

**Boot logo.** The BGRT also publishes the logo bitmap, and EDK II's own
`Logo.bmp` is 193x58. The build replaces it with
[`assets/analysis/neutral-boot-logo.bmp`](../assets/analysis/neutral-boot-logo.bmp)
(320x200). The published bitmap is always re-encoded as 24bpp.

**Measured firmware volumes.** OVMF measures PEIFV and DXEFV into PCR 0 as
`EV_EFI_PLATFORM_FIRMWARE_BLOB` along with their base and length. With the stock
`MemFd.fdf.inc` those are 0x830000/0xD0000 and 0x900000/0xE80000. A guest can
read them back through `tbs.dll` and compare them against a known list;
VMAware's `MEASURED_BOOT` check does exactly that. Either pair matching is
enough, so both volumes move up by `fvShift` (0x10000 by default). DXEFV ends
exactly at the end of MEMFD, so MEMFD grows by the same amount.

## Keeping the profile and the firmware in step

The PCDs come from the same JSON QEMU gets its ACPI identity from, so editing
`analysis.acpi` changes both. The firmware digests do change, though, so after
editing the profile you have to rebuild, re-pin the digests and re-import the
blobs:

```sh
nix build .#analysis-ovmf.fd
sha256sum result-fd/FV/OVMF_CODE.fd result-fd/FV/OVMF_VARS.ms.fd
# update assets.firmware_code / assets.firmware_vars in the profile
```

Nix only sees files that git tracks, so the profile and the logo have to be
tracked (or at least `git add -N`) before `nix build` can read them.

## Importing into a workspace

Assets are read from the workspace's content-addressed store:

```sh
ws=machineemu-workspace
for f in OVMF_CODE.fd OVMF_VARS.ms.fd; do
  d=$(sha256sum "result-fd/FV/$f" | cut -d' ' -f1)
  install -Dm444 "result-fd/FV/$f" "$ws/blobs/sha256/$d"
done
```

An image selected with `--image` still supplies its own NVRAM seed. That seed
has to come from a guest installed on this firmware, because OVMF's variable
store layout is tied to the build. The pinned `firmware_vars` is the pristine,
Microsoft-enrolled store for a fresh install.

## Variants

`mkAnalysisOvmf` is overridable. For example, to use a logo captured from the
hardware the profile imitates (the old repository used a NUC11 capture,
`artifacts/reference/nuc11/bgrt-image.bmp`, which is 600x360 and kept out of git):

```nix
analysis-ovmf.override { bootLogo = /path/to/bgrt-image.bmp; fvShift = 131072; }
```

## Checking the result without a guest

Boot the firmware alone with a graphics device. Without one, the logo is never
drawn and no BGRT is published:

```sh
cp result-fd/FV/OVMF_VARS.ms.fd vars.fd && chmod u+w vars.fd
qemu-system-x86_64 -M q35,smm=on -m 1G -display none -nodefaults \
  -global driver=cfi.pflash01,property=secure,value=on \
  -drive if=pflash,format=raw,unit=0,readonly=on,file=result-fd/FV/OVMF_CODE.fd \
  -drive if=pflash,format=raw,unit=1,file=vars.fd \
  -device VGA -qmp unix:/tmp/bgrt.sock,server,nowait
```

After a few seconds, `pmemsave` the top of low RAM (`{"val": 1065353216, "size":
8388608}` for 1 GiB) and look for `BGRT` followed by a length of `0x38`. A Linux
guest exposes the same table under `/sys/firmware/acpi/bgrt/`.
