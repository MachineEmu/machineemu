"""Explicit diagnostic-only UDM-Pro factory authentication bypass."""
from __future__ import annotations

import hashlib

from .models import FirmwareError

PATH = "usr/bin/ubios-udapi-server"
SOURCE_SHA256 = "10edf63c9a4740a0aa51fecd46b01d0abd57e73e709476cf1a0a3f459e5d91b5"
OFFSET = 0x392D8C
ADDRESS = 0x5A2D8C
BEFORE = bytes.fromhex("ffc301d1fd7b03a9")
AFTER = bytes.fromhex("20008052c0035fd6")


def bypass_factory_auth(data: bytes) -> tuple[bytes, dict[str, str]]:
    """Patch a uniquely pinned instruction sequence, or make no alteration."""
    checksum = hashlib.sha256(data).hexdigest()
    if checksum != SOURCE_SHA256:
        raise FirmwareError("factory auth bypass requires the verified UDM-Pro 5.1.19 binary hash")
    if data[OFFSET : OFFSET + len(BEFORE)] != BEFORE:
        raise FirmwareError("factory auth bypass instruction bytes do not match")
    result = data[:OFFSET] + AFTER + data[OFFSET + len(BEFORE) :]
    return result, {
        "type": "bypass-factory-auth",
        "path": PATH,
        "revision": "2",
        "source_sha256": checksum,
        "output_sha256": hashlib.sha256(result).hexdigest(),
        "file_offset": hex(OFFSET),
        "virtual_address": hex(ADDRESS),
        "before": BEFORE.hex(),
        "after": AFTER.hex(),
        "changes": "DIAGNOSTIC ONLY: skip combined factory identity/authentication; firmware-update verification unchanged",
    }
