"""Synthetic US24Pro factory record signing for diagnostic lab bundles."""
from __future__ import annotations

import hashlib
import stat
from pathlib import Path

from .factory_signing import encode_modulus, load_lab_key, sign_factory_image
from .models import FirmwareError
from .patches import Entry, replace_file
from .us24pro_signature import BEFORE, OFFSET, UBNTBOX_SHA256

UUID = b"US24EMU000001"
MODULUS_OFFSET = 0x183688
IV = b"US24LAB!"


def identity_seed() -> bytearray:
    image = bytearray(b"\xff" * 0x10000)
    image[:20] = bytes.fromhex("525400555301525400555302eb36077700000001")
    image[0xA000:0xA002] = b"\x0e\x00"
    image[0xA01E:0xA022] = image[0x0C:0x10]
    image[0xA022:0xA028] = image[:6]
    image[0xA02E:0xA032] = bytes.fromhex("00c2201a")
    image[0xA032] = len(UUID)
    image[0xA033:0xA033 + len(UUID)] = UUID
    image[0xA0B3:0xA0B7] = bytes.fromhex("0000b166")
    image[0xA0B7:0xA1B7] = image[0x20:0x120]
    image[0xA1B7:0xA1BB] = image[0x10:0x14]
    return image


def lab_factory(entries: list[Entry], key_path: Path) -> tuple[list[Entry], bytes, bytes, dict[str, str]]:
    targets = [entry for entry in entries if entry.name == "bin/ubntbox"]
    if len(targets) != 1 or not stat.S_ISREG(targets[0].mode) or targets[0].fields[4] != 1:
        raise FirmwareError("lab signing requires one regular non-hardlinked bin/ubntbox")
    original = targets[0].data
    if hashlib.sha256(original).hexdigest() != UBNTBOX_SHA256:
        raise FirmwareError("lab signing requires stock hash-pinned US24PRO 7.5.15 ubntbox")
    if original[OFFSET:OFFSET + 4] != BEFORE:
        raise FirmwareError("lab signing refuses an already bypassed signature check")
    key = load_lab_key(key_path)
    modulus = encode_modulus(key.public_key().public_numbers().n)
    patched = original[:MODULUS_OFFSET] + modulus + original[MODULUS_OFFSET + len(modulus):]
    image, public_pem = sign_factory_image(bytes(identity_seed()), key, UUID, IV)
    return replace_file(entries, "bin/ubntbox", patched), image, public_pem, {
        "type": "factory-lab-key", "path": "bin/ubntbox", "revision": "2", "source_sha256": UBNTBOX_SHA256,
        "output_sha256": hashlib.sha256(patched).hexdigest(), "public_key_sha256": hashlib.sha256(public_pem).hexdigest(),
        "file_offset": hex(MODULUS_OFFSET), "virtual_address": "0x1a3688", "length": str(len(modulus)), "uuid": UUID.decode(),
        "changes": "DIAGNOSTIC ONLY: lab verification modulus and signed synthetic EEPROM; no check bypass",
    }
