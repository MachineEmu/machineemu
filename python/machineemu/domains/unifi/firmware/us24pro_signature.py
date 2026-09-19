"""Hash-pinned, signature-only diagnostic patch for US24Pro 7.5.15."""
from __future__ import annotations

import hashlib
import stat

from .models import FirmwareError
from .patches import Entry, replace_file

UBNTBOX_SHA256 = "112f4ce84ccf3b4b6f5cfe8a685d1c500315168b2c273158619a751d1368483f"
OFFSET = 0x45364
BEFORE = bytes.fromhex("1e00000a")
AFTER = bytes.fromhex("1e0000ea")


def bypass_signature(entries: list[Entry]) -> tuple[list[Entry], dict[str, str]]:
    targets = [entry for entry in entries if entry.name == "bin/ubntbox"]
    if len(targets) != 1 or not stat.S_ISREG(targets[0].mode) or targets[0].fields[4] != 1:
        raise FirmwareError("signature bypass requires one regular, non-hardlinked bin/ubntbox")
    data = targets[0].data
    checksum = hashlib.sha256(data).hexdigest()
    if checksum != UBNTBOX_SHA256:
        raise FirmwareError("signature bypass requires the verified US24PRO 7.5.15 ubntbox hash")
    if data[OFFSET : OFFSET + 4] != BEFORE:
        raise FirmwareError("signature bypass instruction bytes do not match")
    patched = data[:OFFSET] + AFTER + data[OFFSET + 4 :]
    return replace_file(entries, "bin/ubntbox", patched), {
        "type": "bypass-factory-signature", "path": "bin/ubntbox", "revision": "1",
        "source_sha256": checksum, "output_sha256": hashlib.sha256(patched).hexdigest(),
        "file_offset": hex(OFFSET), "virtual_address": "0x55364", "before": BEFORE.hex(), "after": AFTER.hex(),
        "changes": "DIAGNOSTIC ONLY: skip factory RSA signature rejection; retain hardware identity checks",
    }
