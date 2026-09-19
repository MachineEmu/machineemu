"""Synthetic UDM-Pro SPI-NOR image.

The 8 MiB flash is not firmware. It is board identity plus guest-owned state,
so a donor dump carries whatever the last emulated boot happened to leave
behind. Observed in one: a stale ``system.cfg`` holding a root password hash,
and an ext4 filesystem written over the ``u-boot`` partition by the mtd
ordering bug that ``unifi_create_spi`` documents. None of that belongs in a
freshly prepared bundle.

Every byte that does matter is already synthetic. ``build_udmpro_eeprom`` in
crates/board-ffi/src/lib.rs derives the identity, this module reproduces it,
and ``udm_pro_lab.lab_factory`` refuses any seed that is not exactly this
record. So the image is generated rather than captured.

Partition offsets follow the vendor device tree, which ``unifi_create_spi``
re-emits for the guest.
"""
from __future__ import annotations

from pathlib import Path

from .models import FirmwareError

SIZE = 8 * 1024**2
ERASED = 0xff

# offset, size, label, read-only -- the vendor SPI-NOR map.
PARTITIONS: tuple[tuple[int, int, str, bool], ...] = (
    (0x000000, 0x1c0000, 'u-boot', False),
    (0x1c0000, 0x010000, 'u-boot env', False),
    (0x1d0000, 0x010000, 'u-boot env redundant', False),
    (0x1e0000, 0x010000, 'Factory', True),
    (0x1f0000, 0x010000, 'EEPROM', True),
    (0x200000, 0x600000, 'config', False),
)

EEPROM_SIZE = 0x10000
EEPROM_OFFSET = 0x1f0000
# ubnt-tools resolves the board from a record at offset 0x8000 of the first
# partition as well as from the EEPROM partition, so the same 64 KiB image is
# placed at both. The guest may later format the u-boot partition and destroy
# this copy; that is vendor behaviour, not a defect in the seed.
ALPINE_OFFSET = 0x000000
ALPINE_RECORD = 0x8000

MAC = bytes((0x52, 0x54, 0x00, 0x4d, 0x50, 0x01))
SECOND_MAC = bytes((0x52, 0x54, 0x00, 0x4d, 0x50, 0x02))
# The UDM-Pro's entry in ubnt-tools' board table. Another value retargets the
# model, but only to a console the firmware's own table already knows; an
# unlisted id misses the lookup and drops the guest onto the generic profile.
SYSTEM_ID = 0xea15
VENDOR_ID = 0x0777
BOM_REVISION = 10
SERIAL = b'QEMU01'
LEGACY_RECORD_LENGTH = 0x65


def legacy_crc32(payload: bytes) -> int:
    """The vendor's bit-reversed CRC-32, stored little-endian."""
    crc = 0
    for byte in payload:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0xedb88320 if crc & 1 else 0)
    return crc


def eeprom(system_id: int = SYSTEM_ID) -> bytes:
    """The deterministic identity region; see build_udmpro_eeprom."""
    if not 1 <= system_id <= 0xffff:
        raise FirmwareError('system id must be a non-zero 16-bit value')
    image = bytearray(b'\xff' * EEPROM_SIZE)
    image[0:6] = MAC
    image[6:12] = SECOND_MAC
    # ubnt-tools matches the bare system id against its board table; an erased
    # lookup falls back to the generic ARMv8 profile and unifi-core then exits
    # with `Unsupported console model`.
    image[0x0c:0x0e] = system_id.to_bytes(2, 'big')
    image[0x0e:0x10] = VENDOR_ID.to_bytes(2, 'big')
    # Erased bytes read as hwrev 0xffffffff, which overflows ULP's signed
    # revision parser and stops its host service starting.
    image[0x10:0x14] = BOM_REVISION.to_bytes(4, 'big')
    # The second identity record enables the version-5 UUID derivation. Without
    # it ubnt-tools falls back to a version-3 UUID, which the UniFi Network
    # application rejects outright.
    image[0xa000] = 0x12
    image[0xa001] = 0x02
    image[0xa020:0xa022] = VENDOR_ID.to_bytes(2, 'big')
    image[0xa022:0xa028] = MAC
    # Six ASCII characters; both erased and zeroed reads are rejected.
    image[0xa0bb:0xa0c1] = SERIAL
    image[0x8000:0x8004] = b'UBNT'
    image[0x8008:0x800b] = bytes(3)
    image[0x800b] = LEGACY_RECORD_LENGTH
    image[0x800c:0x800e] = (2).to_bytes(2, 'big')
    image[0x800e:0x8010] = (2).to_bytes(2, 'big')
    image[0x8010:0x8012] = VENDOR_ID.to_bytes(2, 'big')
    image[0x8012:0x8014] = system_id.to_bytes(2, 'big')
    image[0x8014:0x8018] = BOM_REVISION.to_bytes(4, 'big')
    image[0x8018:0x801e] = MAC
    image[0x801e:0x8020] = bytes((2, 2))
    image[0x8070] = 0x01
    # The record runs to and includes the marker at 0x8070.
    image[0x8004:0x8008] = legacy_crc32(image[0x800c:0x8071]).to_bytes(4, 'little')
    return bytes(image)


def write_template(path: Path, system_id: int = SYSTEM_ID) -> Path:
    """Write an erased 8 MiB flash carrying only the board identity.

    The machine reseeds this region when its identity differs from the one it
    was launched with, so a retargeted image must be paired with a matching
    `-M udm-pro,system-id=` or the guest will read the stock console back.
    """
    identity = eeprom(system_id)
    image = bytearray(bytes((ERASED,)) * SIZE)
    image[ALPINE_OFFSET:ALPINE_OFFSET + EEPROM_SIZE] = identity
    image[EEPROM_OFFSET:EEPROM_OFFSET + EEPROM_SIZE] = identity
    path.write_bytes(bytes(image))
    return path
