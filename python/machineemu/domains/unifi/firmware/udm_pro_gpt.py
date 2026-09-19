"""Synthetic UDM-Pro eMMC partition table.

The vendor update payload ships no partition table. It carries only U-Boot, a
kernel FIT, the rootfs SquashFS and the updater ELF; the table is written on
the appliance by the updater itself. Preparation therefore used to require a
donor disk image supplied with ``--boot-template``.

A donor contributes nothing beyond that table. Its boot and recovery
partitions are zero across all 96 MiB, and :func:`udm_pro.build_disk` formats
root, log, persistent and overlay itself, so the only bytes ever read out of a
template are the protective MBR, the primary GPT and the backup GPT. This
module writes exactly those into a sparse image and reproduces a captured
UDM-Pro table byte for byte.

Layout confirmed against hardware; see docs/udm-pro/qemu-emulation.md.
"""
from __future__ import annotations

import struct
import uuid
import zlib
from pathlib import Path

SECTOR = 512
SECTORS = 25364480
SIZE = SECTORS * SECTOR
FIRST_USABLE = 2048
LAST_USABLE = SECTORS - 34

# Stable identifiers: a generated template must hash identically on every host
# so that prepared bundles stay reproducible.
DISK_GUID = uuid.UUID('e1c5bb85-2469-4f58-aabe-413afa44c794')
LINUX_DATA = uuid.UUID('0fc63daf-8483-4772-8e79-3d69d8477de4')

# name, unique GUID, first LBA, last LBA (inclusive)
ENTRIES: tuple[tuple[str, str, int, int], ...] = (
    ('boot',       'd84d47d5-7e61-4866-988b-1ac7139cc6ab',     2048,   133119),
    ('recovery',   'ddc90376-c1eb-40d7-b0f5-8d8d2ac4c176',   133120,   198655),
    ('root',       'b0a4ef92-fbec-4661-95ce-c8bece66bdd0',   198656,  4392959),
    ('log',        'e9e77c3c-ff71-46e5-8ecf-2933424c82b6',  4392960,  6490111),
    ('persistent', '5f680517-1140-468d-94c5-1df8004f07c0',  6490112, 10684415),
    ('overlay',    'bf37f627-3f9e-4cfe-b61b-40a28b388b2a', 10684416, 25165790),
)

ENTRY_COUNT = 128
ENTRY_SIZE = 128


def entry_table() -> bytes:
    table = bytearray(ENTRY_COUNT * ENTRY_SIZE)
    for index, (name, unique, first, last) in enumerate(ENTRIES):
        entry = bytearray(ENTRY_SIZE)
        entry[0:16] = LINUX_DATA.bytes_le
        entry[16:32] = uuid.UUID(unique).bytes_le
        struct.pack_into('<QQQ', entry, 32, first, last, 0)
        entry[56:ENTRY_SIZE] = name.encode('utf-16-le').ljust(72, b'\0')
        table[index * ENTRY_SIZE:(index + 1) * ENTRY_SIZE] = entry
    return bytes(table)


def gpt_header(current: int, backup: int, entry_lba: int, table: bytes) -> bytes:
    header = bytearray(92)
    header[0:8] = b'EFI PART'
    header[8:12] = bytes((0, 0, 1, 0))
    struct.pack_into('<I', header, 12, 92)
    struct.pack_into('<QQQQ', header, 24, current, backup, FIRST_USABLE, LAST_USABLE)
    header[56:72] = DISK_GUID.bytes_le
    struct.pack_into('<QIII', header, 72, entry_lba, ENTRY_COUNT, ENTRY_SIZE, zlib.crc32(table))
    struct.pack_into('<I', header, 16, zlib.crc32(header))
    return bytes(header).ljust(SECTOR, b'\0')


def protective_mbr() -> bytes:
    mbr = bytearray(SECTOR)
    mbr[446:462] = (bytes((0, 0, 2, 0, 0xee, 0xff, 0xff, 0xff))
                    + struct.pack('<II', 1, min(SECTORS - 1, 0xffffffff)))
    mbr[510:512] = b'\x55\xaa'
    return bytes(mbr)


def write_template(path: Path) -> Path:
    """Write a sparse, partitioned, otherwise empty UDM-Pro eMMC image."""
    table = entry_table()
    with path.open('xb') as stream:
        stream.truncate(SIZE)
        stream.write(protective_mbr())
        stream.write(gpt_header(1, SECTORS - 1, 2, table))
        stream.write(table)
        stream.seek((SECTORS - 33) * SECTOR)
        stream.write(table)
        stream.write(gpt_header(SECTORS - 1, 1, SECTORS - 33, table))
    return path
