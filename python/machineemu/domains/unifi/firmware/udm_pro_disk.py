"""Safe UDM-Pro disk-layout intake primitives.

These routines validate a supplied GPT before reading it and copy only explicit
regions.  They never mount, extract, or inherit guest-owned state.
"""

from __future__ import annotations

import struct
from pathlib import Path
import zlib

from .models import FirmwareError


PARTITIONS = ("boot", "recovery", "root", "log", "persistent", "overlay")
SECTOR = 512


def partitions(path: Path) -> list[tuple[str, int, int]]:
    """Return the verified UDM-Pro GPT partition layout in byte offsets."""
    size = path.stat().st_size
    with path.open("rb") as stream:
        stream.seek(SECTOR)
        header = bytearray(stream.read(SECTOR))
        if len(header) != SECTOR or header[:8] != b"EFI PART":
            raise FirmwareError("boot template requires GPT")
        header_size, checksum = struct.unpack_from("<II", header, 12)
        if not 92 <= header_size <= SECTOR:
            raise FirmwareError("invalid GPT header size")
        header[16:20] = bytes(4)
        if zlib.crc32(header[:header_size]) != checksum:
            raise FirmwareError("GPT header checksum mismatch")
        lba, count, stride, table_checksum = struct.unpack_from("<QIII", header, 72)
        if count > 128 or stride != 128 or lba * SECTOR + count * stride > size:
            raise FirmwareError("unsupported GPT partition table")
        stream.seek(lba * SECTOR)
        table = stream.read(count * stride)
    if zlib.crc32(table) != table_checksum:
        raise FirmwareError("GPT partition checksum mismatch")
    result = []
    for offset in range(0, len(table), stride):
        entry = table[offset : offset + stride]
        if not any(entry[:16]):
            continue
        start, end = struct.unpack_from("<QQ", entry, 32)
        name = entry[56:128].decode("utf-16-le").rstrip("\0")
        if start < 34 or start > end or (end + 1) * SECTOR > size - SECTOR * 33:
            raise FirmwareError("GPT partition exceeds image bounds")
        if result and start * SECTOR < result[-1][1] + result[-1][2]:
            raise FirmwareError("overlapping GPT partitions")
        result.append((name, start * SECTOR, (end - start + 1) * SECTOR))
    if tuple(name for name, _, _ in result) != PARTITIONS:
        raise FirmwareError("UDM boot template requires boot/recovery/root/log/persistent/overlay GPT partitions")
    return result


def copy_region(source: Path, target: Path, source_offset: int, target_offset: int, length: int) -> None:
    """Copy a bounded region without allocating holes full of zero bytes."""
    if min(source_offset, target_offset, length) < 0:
        raise FirmwareError("disk copy offsets and length must be non-negative")
    with source.open("rb") as src, target.open("r+b") as dst:
        src.seek(source_offset)
        dst.seek(target_offset)
        remaining = length
        while remaining:
            data = src.read(min(remaining, 1024 * 1024))
            if not data:
                raise FirmwareError("truncated disk input")
            if data.strip(b"\0"):
                dst.write(data)
            else:
                dst.seek(len(data), 1)
            remaining -= len(data)
