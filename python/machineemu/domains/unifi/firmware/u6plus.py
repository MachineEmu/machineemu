"""MT7981 U6+ container/FIT preparation recipe."""
from __future__ import annotations

import struct
import zlib
from pathlib import Path

from .formats import container, fit
from .models import FirmwareError, FirmwareInfo, PreparedFirmware, PrepareOptions, StorageSpec, digest
from .patches import password_hash, read_cpio, replace_file, write_cpio
from .u6plus_eeprom import write_template as write_eeprom_template

DEVICE = "u6plus"
ADAPTER = "mt7981"
REVISION = "1"
PATCH_SOURCE = "7211a694fa8c23998a551b99dc073e729b3067d94295de6728f7019178b7d560"
CPIO_START = 0x8F9A20
CPIO_END = 0x2E19820


def build_emmc(output: Path) -> None:
    partitions = [("bl2", 1024), ("u-boot-env", 1024), ("Factory", 4096),
                  ("u-boot", 4096), ("EEPROM", 1024), ("kernel0", 65536),
                  ("kernel1", 65536), ("bs", 2048), ("cfg", 32768), ("log", 32768)]
    blocks = 1024**3 // 512
    guid = bytes.fromhex("af3dc60f838472478e793d69d8477de4")
    entries = bytearray(128 * 128)
    start = 2048
    for index, (name, count) in enumerate(partitions):
        offset = index * 128
        entries[offset:offset + 16] = guid
        entries[offset + 16:offset + 32] = bytes([index + 1]) + guid[1:]
        struct.pack_into("<QQ", entries, offset + 32, start, start + count - 1)
        encoded = name.encode("utf-16-le")
        entries[offset + 56:offset + 56 + len(encoded)] = encoded
        start += count

    def header(current: int, backup: int, table: int) -> bytes:
        data = bytearray(512)
        data[:8] = b"EFI PART"
        struct.pack_into("<II", data, 8, 0x10000, 92)
        struct.pack_into("<QQQQ", data, 24, current, backup, 34, blocks - 34)
        data[56:72] = b"\x06" + guid[1:]
        struct.pack_into("<QIII", data, 72, table, 128, 128, zlib.crc32(entries))
        struct.pack_into("<I", data, 16, zlib.crc32(data[:92]))
        return data

    mbr = bytearray(512)
    mbr[447:454] = bytes.fromhex("000200eeffffff")
    struct.pack_into("<II", mbr, 454, 1, blocks - 1)
    mbr[510:512] = b"\x55\xaa"
    with output.open("xb") as stream:
        stream.truncate(blocks * 512)
        stream.write(mbr + header(1, blocks - 1, 2) + entries)
        stream.seek((blocks - 33) * 512)
        stream.write(entries + header(blocks - 1, 1, blocks - 33))


def _read(source: Path):
    version, sections = container(source)
    if not version.startswith("BZ.MT7981."):
        raise FirmwareError("firmware is not MT7981")
    kernels = [section for section in sections if section.name == "kernel0" and section.kind == "EMMC"]
    if len(kernels) != 1:
        raise FirmwareError("missing EMMC kernel0")
    configuration, images = fit(kernels[0].read(source), ("fdt-u6-plus",))
    info = FirmwareInfo(DEVICE, version, source.stat().st_size, digest(source), configuration,
                        ("Container CRC and FIT hashes checked; vendor signatures are not authenticated.",
                         "SPI/EEPROM are model-seeded in memory; only eMMC persists."))
    return info, images


def inspect(source: Path) -> FirmwareInfo:
    return _read(source)[0]


def prepare(source: Path, output: Path, options: PrepareOptions) -> PreparedFirmware:
    options.public()
    lab = options.factory_lab_key is not None
    if options.boot_template or (not lab and (options.spi_template or options.variant != "stock")):
        raise FirmwareError("U6+ disk templates and diagnostic mode require lab signing")
    if lab and options.rootfs != "external":
        raise FirmwareError("U6+ lab signing requires --rootfs external")
    spi_template = options.spi_template
    if lab:
        if spi_template is None:
            spi_template = write_eeprom_template(output / ".eeprom-template.bin")
        if spi_template.stat().st_size != 65536:
            raise FirmwareError("U6+ lab signing requires a 64 KiB EEPROM template")
    info, images = _read(source)
    modifications: list[dict[str, str]] = []
    boot = {"kernel": "Image", "dtb": "u6plus.dtb", "initrd": None,
            "append": "console=ttyS0,115200n8 earlycon=uart8250,mmio32,0x11002000 loglevel=8"}
    if options.passwords and options.rootfs != "external":
        raise FirmwareError("U6+ password patches require explicit --rootfs external")
    if options.rootfs == "external":
        if info.source_sha256 != PATCH_SOURCE:
            raise FirmwareError("external U6+ rootfs requires the hash-pinned 6.7.54 recipe")
        entries, end = read_cpio(images["kernel"], CPIO_START)
        if end != CPIO_END:
            raise FirmwareError("U6+ embedded archive bounds changed")
        accounts = next(entry.data for entry in entries if entry.name == "usr/etc/passwd")
        names = {row.split(b":")[0].decode() for row in accounts.splitlines()}
        for user, password in options.passwords:
            if user not in names:
                raise FirmwareError("account is absent from the effective vendor passwd file")
            hashed = password_hash(password, "6")
            for path in ("usr/etc/default_mt7981.cfg", "usr/etc/default_nossid_mt7981.cfg"):
                original = next(entry.data for entry in entries if entry.name == path).decode()
                lines = original.splitlines(keepends=True)
                keys = [line.split("=", 1)[0].removesuffix(".name")
                        for line in lines if line.strip().endswith(".name=" + user)]
                if len(keys) != 1:
                    raise FirmwareError("unsupported vendor account configuration")
                key = keys[0] + ".password="
                matches = [index for index, line in enumerate(lines) if line.startswith(key)]
                if len(matches) != 1 or not lines[matches[0]].startswith(key + "$6$"):
                    raise FirmwareError("unverified vendor password scheme")
                index = matches[0]
                lines[index] = key + hashed + ("\n" if lines[index].endswith("\n") else "")
                entries = replace_file(entries, path, "".join(lines).encode())
                modifications.append({"type": "set-password", "account": user, "path": path, "revision": "1"})
        if lab:
            from .u6plus_lab import lab_factory
            entries, eeprom, public_pem, change = lab_factory(entries, spi_template.read_bytes(), options.factory_lab_key)
            (output / "eeprom.bin").write_bytes(eeprom)
            (output / "factory-lab-public.pem").write_bytes(public_pem)
            modifications.append(change)
            if options.spi_template is None:
                spi_template.unlink()
        (output / "initramfs.cpio").write_bytes(write_cpio(entries))
        boot["initrd"] = "initramfs.cpio"
        modifications.append({"type": "external-rootfs", "path": "initramfs.cpio", "revision": "hash-pinned-6.7.54-1"})
    (output / "Image").write_bytes(images["kernel"])
    (output / "u6plus.dtb").write_bytes(images["fdt"])
    build_emmc(output / "emmc.img")
    return PreparedFirmware(
        info, ADAPTER, boot,
        {"machine": {"type": ADAPTER, "properties": {"secure": True, "gic-version": 3}},
         "cpu": "cortex-a53", "cpus": 2, "memory": "512M"},
        [StorageSpec("emmc", "mt7981-emmc", "copy", True, "emmc.img"),
         StorageSpec("spi", "model-memory", "board-seeded", False)],
        modifications, "emulated-gpt-erased-cfg", "mt7981-gpt-1")
