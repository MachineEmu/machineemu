"""Hash-pinned US24Pro extraction with explicit model limitations."""
from __future__ import annotations

from pathlib import Path

from .formats import container, decompress, uimage
from .models import FirmwareError, FirmwareInfo, PreparedFirmware, PrepareOptions, StorageSpec, digest
from .patches import read_cpio, write_cpio
from .us24pro_signature import bypass_signature

DEVICE = ADAPTER = "us24pro"
REVISION = "4"
SOURCE = "08f05a425f5d9bfa21098f581c2fd524101e5401378135c051bf1435af66d3f0"
APPEND = ("console=ttyS0,115200n8 maxcpus=1 mem=256M "
          "mtdparts=spi1.0:1920k(u-boot),64k(u-boot-env),64k(shmoo),"
          "31168k(kernel0),31232k(kernel1),1024k(cfg),64k(EEPROM)")


def _read(source: Path) -> tuple[FirmwareInfo, bytes, bytes]:
    version, sections = container(source)
    checksum = digest(source)
    if not version.startswith("US.bcm5616x.") or checksum != SOURCE:
        raise FirmwareError("US24PRO currently requires hash-pinned firmware 7.5.15")
    kernels = [section for section in sections if section.name == "kernel0" and section.kind == "PART"]
    if len(kernels) != 1:
        raise FirmwareError("missing PART kernel0")
    kernel = kernels[0].read(source)
    rootfs = decompress(uimage(kernel)[0x372D90 : 0x372D90 + 0x1111DE3], "lzma")
    _, end = read_cpio(rootfs, allow_root_clamped_links=True)
    if any(rootfs[end:]):
        raise FirmwareError("unexpected data after US24PRO initramfs")
    return FirmwareInfo(DEVICE, version, source.stat().st_size, checksum, limitations=(
        "Current model lacks persistent flash and guest program/erase support; reuse is unavailable.",
        "Current model unconditionally seeds diagnostic cfg; stock prepared launches are unavailable.",
        "Vendor source is hash-pinned; RSA signatures are not independently authenticated.",
    )), kernel, rootfs


def inspect(source: Path) -> FirmwareInfo:
    return _read(source)[0]


def prepare(source: Path, output: Path, options: PrepareOptions) -> PreparedFirmware:
    options.public()
    if options.passwords:
        raise FirmwareError("US24PRO password patches require verified cfg and persistent flash support")
    if options.boot_template or options.spi_template:
        raise FirmwareError("current US24PRO adapter has no file-backed flash contract")
    if options.factory_lab_key is not None:
        raise FirmwareError("US24PRO lab signing is not yet migrated")
    info, kernel, rootfs = _read(source)
    changes: list[dict[str, str]] = []
    if options.bypass_factory_signature:
        entries, _ = read_cpio(rootfs, allow_root_clamped_links=True)
        entries, modification = bypass_signature(entries)
        rootfs = write_cpio(entries)
        changes.append(modification)
    (output / "uImage").write_bytes(kernel)
    (output / "initramfs.cpio").write_bytes(rootfs)
    if options.variant == "diagnostic":
        changes.append({"type": "configuration-seed", "path": "cfg", "revision": "bcm5616x-cfg-1"})
    return PreparedFirmware(
        info, ADAPTER, {"kernel": "uImage", "initrd": "initramfs.cpio", "append": APPEND},
        {"machine": {"type": ADAPTER}, "cpu": "cortex-a9", "cpus": 1, "memory": "256M"},
        [StorageSpec("flash", "model-memory", "board-seeded", False)], changes,
        "diagnostic" if options.variant == "diagnostic" else "stock-erased-cfg", "bcm5616x-cfg-1",
    )
