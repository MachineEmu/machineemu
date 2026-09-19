from __future__ import annotations

import hashlib
import json
import stat
import struct
import zlib
from pathlib import Path

import pytest

from machineemu.domains.unifi.firmware import (
    Entry,
    FirmwareError,
    PrepareOptions,
    container,
    fit,
    load_bundle,
    read_cpio,
    replace_file,
    set_passwords,
    write_cpio,
)
from machineemu.domains.unifi.firmware.udm_pro_gpt import (
    ENTRIES,
    ENTRY_SIZE,
    LAST_USABLE,
    SECTOR,
    SECTORS,
    entry_table,
    gpt_header,
    protective_mbr,
)
from machineemu.domains.unifi.firmware.udm_pro_spi import (
    EEPROM_OFFSET,
    EEPROM_SIZE,
    PARTITIONS,
    SIZE,
    SYSTEM_ID,
    eeprom,
    legacy_crc32,
    write_template,
)
from machineemu.domains.unifi.firmware.udm_pro_disk import build_disk, copy_region, partitions
from machineemu.domains.unifi.firmware import squashfs
from machineemu.domains.unifi.firmware.squashfs import SquashFS, library_path
from machineemu.domains.unifi.firmware import udm_pro, udm_pro_factory_auth


def pack_fdt(node: tuple[str, dict[str, bytes], list[object]]) -> bytes:
    strings = bytearray()
    offsets: dict[str, int] = {}
    body = bytearray()

    def emit(current: tuple[str, dict[str, bytes], list[object]]) -> None:
        name, properties, children = current
        body.extend(struct.pack(">I", 1) + name.encode() + b"\0")
        body.extend(bytes(-len(body) % 4))
        for key, value in properties.items():
            if key not in offsets:
                offsets[key] = len(strings)
                strings.extend(key.encode() + b"\0")
            body.extend(struct.pack(">III", 3, len(value), offsets[key]) + value)
            body.extend(bytes(-len(body) % 4))
        for child in children:
            emit(child)  # type: ignore[arg-type]
        body.extend(struct.pack(">I", 2))

    emit(node)
    body.extend(struct.pack(">I", 9))
    header = struct.pack(
        ">10I", 0xD00DFEED, 56 + len(body) + len(strings), 56, 56 + len(body),
        40, 17, 16, 0, len(strings), len(body),
    )
    return header + bytes(16) + body + strings


def fit_fixture() -> bytes:
    kernel = bytearray(256)
    kernel[56:60] = b"ARM\x64"
    dtb = pack_fdt(("", {"model": b"U6+\0"}, []))
    images = []
    for name, kind, content in (("kernel-1", "kernel", bytes(kernel)), ("fdt-u6-plus", "flat_dt", dtb)):
        images.append((name, {"data": content, "type": kind.encode() + b"\0", "arch": b"arm64\0", "compression": b"none\0"}, [("hash", {"algo": b"sha256\0", "value": hashlib.sha256(content).digest()}, [])]))
    return pack_fdt(("", {}, [("images", {}, images), ("configurations", {}, [("config-a642", {"kernel": b"kernel-1\0", "fdt": b"fdt-u6-plus\0"}, [])])]))


def container_fixture(payload: bytes) -> bytes:
    header = b"UBNTBZ.MT7981.test".ljust(0x104, b"\0")
    header += struct.pack(">I", zlib.crc32(header)) + bytes(4)
    record = b"EMMC" + b"kernel0".ljust(16, b"\0") + bytes(12) + struct.pack(">6I", 0, 1, 0, 0, len(payload), len(payload))
    return header + record + payload + struct.pack(">II", zlib.crc32(record + payload), 0) + b"ENDS" + bytes(260)


def test_container_and_fit_are_content_checked(tmp_path: Path) -> None:
    source = tmp_path / "firmware.bin"
    source.write_bytes(container_fixture(fit_fixture()))
    version, sections = container(source)
    assert version == "BZ.MT7981.test"
    selected, parts = fit(sections[0].read(source), ("fdt-u6-plus",))
    assert selected == "config-a642"
    assert parts["kernel"][56:60] == b"ARM\x64"

    changed = bytearray(source.read_bytes())
    changed[sections[0].offset] ^= 1
    source.write_bytes(changed)
    with pytest.raises(FirmwareError, match="CRC mismatch"):
        container(source)


def test_prepared_bundle_rejects_changed_artifacts(tmp_path: Path) -> None:
    image = tmp_path / "Image"
    dtb = tmp_path / "board.dtb"
    image.write_bytes(b"kernel")
    dtb.write_bytes(b"dtb")
    outputs = {
        path.name: {"path": path.name, "size": path.stat().st_size, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
        for path in (image, dtb)
    }
    manifest = {
        "version": 1,
        "info": {"device": "u6plus"},
        "adapter": "mt7981",
        "outputs": outputs,
        "boot": {"kernel": "Image", "dtb": "board.dtb", "initrd": None},
        "settings": {"machine": {"type": "mt7981", "properties": {"secure": True, "gic-version": 3}}},
        "storage": [
            {"role": "emmc", "backend": "mt7981-emmc", "initialization": "copy", "persistent": True, "template": "Image"},
            {"role": "spi", "backend": "model-memory", "initialization": "board-seeded", "persistent": False},
        ],
        "options": {"variant": "stock"},
    }
    (tmp_path / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
    bundle = load_bundle(tmp_path)
    assert bundle.artifact("Image") == image
    image.write_bytes(b"changed")
    with pytest.raises(FirmwareError, match="changed"):
        load_bundle(tmp_path)


def test_public_prepare_options_do_not_expose_passwords() -> None:
    options = PrepareOptions(passwords=(("ubnt", "private:password"),))
    assert options.public()["password_accounts"] == ["ubnt"]
    assert "private:password" not in repr(options)


def test_squashfs_native_library_contract_is_explicit_and_bounded(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("MACHINEEMU_SQUASHFS_LIBRARY", "/opt/test/libsquashfs.so.1")
    assert library_path() == "/opt/test/libsquashfs.so.1"
    monkeypatch.setattr(squashfs, "MAX_IMAGE", 64)
    with pytest.raises(FirmwareError, match="exceeds bounds"):
        SquashFS(bytes(65))
    with pytest.raises(FirmwareError, match="exceeds bounds"):
        SquashFS(b"too short")


def test_udm_diagnostic_initrd_adds_an_explicit_ordered_hook() -> None:
    initrd = write_cpio([
        archive_entry("scripts", mode=stat.S_IFDIR | 0o755),
        archive_entry("scripts/init-bottom", mode=stat.S_IFDIR | 0o755),
        archive_entry("scripts/init-bottom/ORDER", b"/scripts/init-bottom/10-base\n"),
    ])
    entries, _ = read_cpio(udm_pro._diagnostic_initrd(initrd))
    files = {entry.name: entry for entry in entries}
    assert files["scripts/init-bottom/ORDER"].data.endswith(b"/scripts/init-bottom/99-qemu-diagnostic\n")
    assert files["scripts/init-bottom/99-qemu-diagnostic"].data == udm_pro.DIAGNOSTIC_HOOK


def test_udm_factory_auth_bypass_is_diagnostic_hash_pinned(monkeypatch: pytest.MonkeyPatch) -> None:
    data = bytearray(udm_pro_factory_auth.OFFSET + len(udm_pro_factory_auth.BEFORE))
    data[udm_pro_factory_auth.OFFSET : udm_pro_factory_auth.OFFSET + len(udm_pro_factory_auth.BEFORE)] = udm_pro_factory_auth.BEFORE
    monkeypatch.setattr(udm_pro_factory_auth, "SOURCE_SHA256", hashlib.sha256(data).hexdigest())
    patched, change = udm_pro_factory_auth.bypass_factory_auth(bytes(data))
    assert patched[udm_pro_factory_auth.OFFSET : udm_pro_factory_auth.OFFSET + len(udm_pro_factory_auth.AFTER)] == udm_pro_factory_auth.AFTER
    assert change["type"] == "bypass-factory-auth"
    with pytest.raises(FirmwareError, match="hash"):
        udm_pro_factory_auth.bypass_factory_auth(b"wrong")


def archive_entry(name: str, data: bytes = b"", mode: int = stat.S_IFREG | 0o640, links: int = 1) -> Entry:
    return Entry(name, (1, mode, 0, 0, links, 0, len(data), 0, 0, 0, 0, 0, 0), data)


def test_cpio_edits_preserve_metadata_and_reject_escape() -> None:
    entries = [
        archive_entry("etc", mode=stat.S_IFDIR | 0o755),
        archive_entry("etc/config", b"old"),
        archive_entry("bin/find", b"../../bin/busybox", stat.S_IFLNK | 0o777),
    ]
    parsed, _ = read_cpio(write_cpio(entries), allow_root_clamped_links=True)
    changed = replace_file(parsed, "etc/config", b"new")
    round_trip, _ = read_cpio(write_cpio(changed), allow_root_clamped_links=True)
    assert round_trip[1].data == b"new"
    assert round_trip[1].fields[1:6] == parsed[1].fields[1:6]
    with pytest.raises(FirmwareError, match="unsafe archive path"):
        read_cpio(write_cpio([archive_entry("../escape")]))
    with pytest.raises(FirmwareError, match="hardlinks"):
        replace_file([archive_entry("etc/config", b"old", links=2)], "etc/config", b"new")


def test_password_patch_never_returns_secret_metadata(monkeypatch: pytest.MonkeyPatch) -> None:
    entries = [
        archive_entry("etc/passwd", b"root:x:0:0:root:/:/bin/sh\n"),
        archive_entry("etc/shadow", b"root:$6$old:1:2:3:4:5:6\n"),
    ]
    monkeypatch.setattr(
        "machineemu.domains.unifi.firmware.patches.password_hash",
        lambda password, scheme: "$6$replacement",
    )
    changed, metadata = set_passwords(entries, (("root", "private:password"),), "etc/passwd", "etc/shadow")
    assert changed[1].data.startswith(b"root:$6$replacement:")
    assert metadata == [{"type": "set-password", "account": "root", "path": "etc/shadow", "revision": "1"}]
    assert "private:password" not in json.dumps(metadata)


def test_udm_pro_gpt_template_has_deterministic_valid_layout() -> None:
    table = entry_table()
    assert len(table) == 128 * ENTRY_SIZE
    decoded = []
    for index in range(len(ENTRIES)):
        entry = table[index * ENTRY_SIZE : (index + 1) * ENTRY_SIZE]
        first, last = struct.unpack_from("<QQ", entry, 32)
        decoded.append((entry[56:ENTRY_SIZE].decode("utf-16-le").rstrip("\0"), first, last))
    assert decoded == [(name, first, last) for name, _, first, last in ENTRIES]
    assert decoded[0][1] == 2048
    assert decoded[-1][2] < LAST_USABLE

    primary = bytearray(gpt_header(1, SECTORS - 1, 2, table))
    checksum = struct.unpack_from("<I", primary, 16)[0]
    primary[16:20] = bytes(4)
    assert primary[:8] == b"EFI PART"
    assert zlib.crc32(primary[:92]) == checksum
    assert struct.unpack_from("<I", primary, 88)[0] == zlib.crc32(table)

    mbr = protective_mbr()
    assert len(mbr) == SECTOR
    assert mbr[450] == 0xEE
    assert mbr[510:] == b"\x55\xaa"


def test_udm_pro_spi_identity_is_deterministic_and_retargetable(tmp_path: Path) -> None:
    identity = eeprom()
    assert len(identity) == EEPROM_SIZE
    assert identity[:16].hex() == "5254004d50015254004d5002ea150777"
    assert int.from_bytes(identity[0x8004:0x8008], "little") == legacy_crc32(identity[0x800C:0x8071])
    assert [(offset, size, label) for offset, size, label, _ in PARTITIONS][-1] == (0x200000, 0x600000, "config")

    retargeted = eeprom(0xEA2A)
    assert int.from_bytes(retargeted[0x0C:0x0E], "big") == 0xEA2A
    assert int.from_bytes(retargeted[0x8012:0x8014], "big") == 0xEA2A
    assert retargeted[0x8004:0x8008] != identity[0x8004:0x8008]
    with pytest.raises(FirmwareError, match="non-zero"):
        eeprom(0)

    path = write_template(tmp_path / "spi.img", system_id=0xEA2A)
    data = path.read_bytes()
    assert len(data) == SIZE
    for offset in (0, EEPROM_OFFSET):
        assert data[offset:offset + EEPROM_SIZE] == retargeted
    assert set(data[EEPROM_SIZE:EEPROM_OFFSET]) == {0xFF}
    assert SYSTEM_ID == 0xEA15


def test_udm_pro_disk_layout_and_region_copy_are_bounded(tmp_path: Path) -> None:
    template = tmp_path / "template.img"
    table = entry_table()
    with template.open("xb") as stream:
        stream.truncate(SECTORS * SECTOR)
        stream.seek(SECTOR)
        stream.write(gpt_header(1, SECTORS - 1, 2, table))
        stream.write(table)
    layout = partitions(template)
    assert [name for name, _, _ in layout] == [name for name, _, _, _ in ENTRIES]

    source = tmp_path / "source.bin"
    target = tmp_path / "target.bin"
    source.write_bytes(b"A" * 4 + bytes(4) + b"B" * 4)
    target.write_bytes(b"x" * 12)
    copy_region(source, target, 0, 2, 12)
    assert target.read_bytes() == b"xxAAAA" + bytes(4) + b"BBBB"
    with pytest.raises(FirmwareError, match="truncated"):
        copy_region(source, target, 0, 0, 13)


def test_udm_pro_disk_builder_copies_only_boot_regions(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import machineemu.domains.unifi.firmware.udm_pro_disk as disk

    layout = [
        ("boot", 4096, 4096),
        ("recovery", 8192, 4096),
        ("root", 12288, 4096),
        ("log", 16384, 4096),
        ("persistent", 20480, 4096),
        ("overlay", 24576, 4096),
    ]
    template = tmp_path / "template.img"
    template.write_bytes(b"G" * 4096 + b"BOOT" * 1024 + b"RECV" * 1024 + b"ROOT" * 1024 + b"L" * 16384)
    rootfs = tmp_path / "rootfs.img"
    rootfs.write_bytes(b"R" * 4096)
    scratch = tmp_path / "scratch"
    scratch.mkdir()
    commands: list[list[str]] = []

    monkeypatch.setattr(disk, "partitions", lambda value: layout)
    monkeypatch.setattr(disk.shutil, "which", lambda value: "/usr/sbin/mke2fs")
    monkeypatch.setattr(disk.subprocess, "run", lambda command, **kwargs: commands.append(command))
    output = tmp_path / "output.img"
    build_disk(template, output, rootfs, scratch)
    data = output.read_bytes()
    assert data[4096:8192] == b"BOOT" * 1024
    assert data[8192:12288] == b"RECV" * 1024
    assert data[12288:16384] != b"ROOT" * 1024
    assert [command[command.index("-L") + 1] for command in commands] == ["root", "log", "persistent", "overlay"]
    assert all(command[-1].endswith(".ext4") for command in commands)
    assert not list(scratch.glob("*.ext4"))

    unaligned = tmp_path / "unaligned.img"
    unaligned.write_bytes(b"x")
    with pytest.raises(FirmwareError, match="loop-device sector"):
        build_disk(template, tmp_path / "bad.img", unaligned, scratch)
