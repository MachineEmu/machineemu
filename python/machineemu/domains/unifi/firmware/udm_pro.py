"""Hash-pinned UDM-Pro firmware extraction and fresh-disk construction."""
from __future__ import annotations

import shutil
from pathlib import Path

from .formats import Section, fit
from .models import FirmwareError, FirmwareInfo, PreparedFirmware, PrepareOptions, StorageSpec, digest
from .patches import Entry, read_cpio, replace_file, set_passwords, write_cpio
from .squashfs import SquashFS
from .udm_pro_disk import ROOTFS_ALIGNMENT, build_disk, partitions
from .udm_pro_gpt import write_template as write_gpt_template
from .udm_pro_spi import write_template as write_spi_template

DEVICE = ADAPTER = "udm-pro"
REVISION = "hash-pinned-5.1.19-7"
SOURCE = "163cde709c66d596f2b1940445193aede82883bc443bca0972c94e4ea84e3cfa"
VERSION = "UDMPRO.al324.v5.1.19.3fbc1da.260613.0944"
ROOTFS_OFFSET = 16073698
ROOTFS_SIZE = 921078145

DIAGNOSTIC_HOOK = b'''#!/bin/sh
target="${rootmnt:-/root}"
mkdir -p "$target/etc/modules-load.d"
printf 'ubnthal\\n' > "$target/etc/modules-load.d/ubnthal.conf"
module_dir="$target/lib/modules/$(uname -r)/extra"
if [ -x /sbin/insmod ]; then
    [ ! -r "$module_dir/ubnt_common.ko" ] || /sbin/insmod "$module_dir/ubnt_common.ko"
    [ ! -r "$module_dir/ubnthal.ko" ] || /sbin/insmod "$module_dir/ubnthal.ko"
fi
profile="$target/usr/share/ubios-udapi-server/config-board/udm-pro-ea15.json"
if [ -r "$profile" ] && [ -x "$target/usr/bin/jq" ]; then
    mkdir -p "$target/tmp"
    if chroot "$target" /usr/bin/jq .switches=[] /usr/share/ubios-udapi-server/config-board/udm-pro-ea15.json > "$target/tmp/qemu-profile.json"; then
        mv "$target/tmp/qemu-profile.json" "$profile"
    fi
fi
for unit in usdbd uhwd ustated udapi-server; do
    mkdir -p "$target/etc/systemd/system/$unit.service.d"
    printf '[Service]\\nStandardOutput=journal+console\\nStandardError=journal+console\\n' > "$target/etc/systemd/system/$unit.service.d/qemu-diagnostics.conf"
done
'''


def _read(source: Path) -> tuple[FirmwareInfo, dict[str, bytes]]:
    if source.stat().st_size != 941806898 or digest(source) != SOURCE:
        raise FirmwareError("UDM-Pro currently requires hash-pinned firmware 5.1.19")
    selected, images = fit(Section("fit", "hash-pinned", 1499296, 14574338).read(source), ("fdt@1",), "udmpro@1")
    if "ramdisk" not in images:
        raise FirmwareError("UDM firmware lacks recovery ramdisk")
    return FirmwareInfo(
        DEVICE, VERSION, source.stat().st_size, SOURCE, selected,
        ("The eMMC partition table and SPI identity are generated when no templates are supplied.",
         "Stock vendor DTB and SquashFS can expose model/kernel boot blockers.",
         "Source hash checked; vendor signatures are not independently authenticated."),
    ), images


def inspect(source: Path) -> FirmwareInfo:
    return _read(source)[0]


def _rootfs(source: Path) -> bytes:
    with source.open("rb") as stream:
        stream.seek(ROOTFS_OFFSET)
        data = stream.read(ROOTFS_SIZE)
    if len(data) != ROOTFS_SIZE or data[:4] != b"hsqs":
        raise FirmwareError("missing or truncated vendor SquashFS")
    return data


def _diagnostic_initrd(initrd: bytes) -> bytes:
    entries, _ = read_cpio(initrd)
    order = "scripts/init-bottom/ORDER"
    existing = next((entry.data for entry in entries if entry.name == order), None)
    if existing is None:
        raise FirmwareError("UDM diagnostic initrd lacks init-bottom ORDER")
    hook = "scripts/init-bottom/99-qemu-diagnostic"
    entries = replace_file(entries, order, existing.rstrip(b"\n") + b"\n/" + hook.encode() + b"\n")
    entries.append(Entry(hook, (max(entry.fields[0] for entry in entries) + 1, 0o100755, 0, 0, 1,
                                0, 0, 0, 0, 0, 0, 0, 0), DIAGNOSTIC_HOOK))
    return write_cpio(entries)


def prepare(source: Path, output: Path, options: PrepareOptions) -> PreparedFirmware:
    """Prepare an output directory. Publication and manifesting are coordinated above this recipe."""
    if options.rootfs != "embedded":
        raise FirmwareError("UDM rootfs is disk-backed; --rootfs external is a U6+ option")
    if options.factory_lab_key is not None:
        raise FirmwareError("UDM factory lab-key preparation is not yet migrated")
    if options.system_id is not None and options.spi_template is not None:
        raise FirmwareError("--system-id retargets generated SPI identity; it cannot be combined with --spi-template")
    spi_template = options.spi_template or write_spi_template(
        output / ".spi-template.img", **({"system_id": options.system_id} if options.system_id is not None else {})
    )
    boot_template = options.boot_template or write_gpt_template(output / ".boot-template.img")
    try:
        partitions(boot_template)
        if spi_template.stat().st_size != 8 * 1024**2:
            raise FirmwareError("UDM SPI template must be an 8 MiB raw image")
        info, images = _read(source)
        rootfs = _rootfs(source)
        changes: list[dict[str, str]] = []
        with SquashFS(rootfs) as fs:
            replacement: dict[str, bytes] = {}
            if options.passwords:
                entries = []
                for path in ("etc/passwd", "etc/shadow"):
                    meta = fs.metadata(path)
                    entries.append(Entry(path, (0, meta.mode, meta.uid, meta.gid, 1, meta.mtime,
                                                0, 0, 0, 0, 0, 0, 0), fs.read(path)))
                entries, password_changes = set_passwords(entries, options.passwords, "etc/passwd", "etc/shadow", verified_scheme="5")
                replacement = {entry.name: entry.data for entry in entries}
                changes.extend(password_changes)
            if options.bypass_factory_auth:
                from .udm_pro_factory_auth import PATH, bypass_factory_auth
                patched, modification = bypass_factory_auth(fs.read(PATH))
                replacement[PATH] = patched
                changes.append(modification)
            if replacement or options.variant == "diagnostic":
                rootfs = fs.rebuild(replacement, block_size=131072 if options.variant == "diagnostic" else None)
                changes.append({"type": "squashfs-rebuild", "path": "rootfs.squashfs", "revision": "libsquashfs-1"})
        rootfs_path = output / "rootfs.squashfs"
        rootfs_path.write_bytes(rootfs)
        padding = -len(rootfs) % ROOTFS_ALIGNMENT
        if padding:
            with rootfs_path.open("ab") as stream:
                stream.write(bytes(padding))
        initrd = _diagnostic_initrd(images["ramdisk"]) if options.variant == "diagnostic" else images["ramdisk"]
        if options.variant == "diagnostic":
            changes.append({"type": "diagnostic-initrd", "path": "scripts/init-bottom/99-qemu-diagnostic", "revision": "1"})
        (output / "Image").write_bytes(images["kernel"])
        (output / "udmpro.dtb").write_bytes(images["fdt"])
        (output / "initramfs.cpio").write_bytes(initrd)
        scratch = output / "disk-build"
        scratch.mkdir()
        try:
            build_disk(boot_template, output / "boot.img", rootfs_path, scratch)
        finally:
            shutil.rmtree(scratch)
        with spi_template.open("rb") as stream:
            factory = stream.read(2 * 1024**2)
        (output / "spi.img").write_bytes(factory + b"\xff" * (6 * 1024**2))
        return PreparedFirmware(
            info, ADAPTER,
            {"kernel": "Image", "dtb": "udmpro.dtb", "initrd": "initramfs.cpio",
             "append": "earlycon=uart8250,mmio32,0xfd883000,115200 console=ttyS0,115200 root=rootfs rootfstype=ext4 rdinit=/init loglevel=5 ip=:::::eth1:bootp no_reboot"},
            {"machine": {"type": ADAPTER}, "cpu": None, "cpus": 1, "memory": "2G"},
            [StorageSpec("boot", "udm-boot", "copy", True, "boot.img"),
             StorageSpec("spi", "udm-config", "copy", True, "spi.img")],
            changes, "supplied-factory-partitions-and-model-identity",
        )
    finally:
        if options.boot_template is None and boot_template.exists():
            boot_template.unlink()
        if options.spi_template is None and spi_template.exists():
            spi_template.unlink()
