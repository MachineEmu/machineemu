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
