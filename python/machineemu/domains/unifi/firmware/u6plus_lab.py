"""Hash-pinned U6+ factory trust-anchor replacement for diagnostics only."""
from __future__ import annotations

import hashlib
import stat
from pathlib import Path

from .factory_signing import encode_modulus, load_lab_key, sign_factory_image
from .models import FirmwareError
from .patches import Entry, replace_file
from .u6plus_eeprom import SEED_PREFIX

SOURCE_SHA256 = "15ddaba4d5057e72b5ad1e536e1d38061a4e7ac458f28d42d5b97d107a207073"
PATH = "sbin/ubntbox"
MODULUS_OFFSETS = (0x1877E0, 0x187BF0, 0x187EB0)
MODULUS_SIZE = 588
MODULUS_SHA256 = "64c08b23a1462bcf6d20375d4ea28b7ef2827f724b90afaf2918d2e386906537"
SERIAL = b"QEMU-U6PLUS-0"
IV = b"U6PLSLAB"


def lab_factory(entries: list[Entry], seed: bytes, key_path: Path):
    targets = [entry for entry in entries if entry.name == PATH]
    if len(targets) != 1 or not stat.S_ISREG(targets[0].mode) or targets[0].fields[4] != 1:
        raise FirmwareError("U6+ lab signing requires one regular non-hardlinked sbin/ubntbox")
    original = targets[0].data
    if hashlib.sha256(original).hexdigest() != SOURCE_SHA256:
        raise FirmwareError("U6+ lab signing requires stock hash-pinned 6.7.54 ubntbox")
    if len(seed) != 65536 or seed[:len(SEED_PREFIX)] != SEED_PREFIX:
        raise FirmwareError("U6+ lab signing requires a 64 KiB model-seeded EEPROM template")
    key = load_lab_key(key_path)
    modulus = encode_modulus(key.public_key().public_numbers().n)
    patched = bytearray(original)
    for offset in MODULUS_OFFSETS:
        if hashlib.sha256(original[offset:offset + MODULUS_SIZE]).hexdigest() != MODULUS_SHA256:
            raise FirmwareError("U6+ factory modulus differs from the pinned firmware")
        patched[offset:offset + MODULUS_SIZE] = modulus
    image = bytearray(seed)
    image[0xA000:0xA002] = b"\x12\x03"
    image[0xA01E:0xA022] = image[0x0C:0x10]
    image[0xA022:0xA028] = image[:6]
    image[0xA02E:0xA032] = bytes.fromhex("00ef4018")
    image[0xA032] = len(SERIAL)
    image[0xA033:0xA033 + len(SERIAL)] = SERIAL
    image[0xA0B3:0xA0B7] = (0x7981).to_bytes(4, "big")
    image[0xA0B7:0xA0BB] = image[0x10:0x14]
    image, public_pem = sign_factory_image(bytes(image), key, SERIAL, IV)
    record = {
        "type": "factory-lab-key", "path": PATH, "revision": "1",
        "source_sha256": SOURCE_SHA256,
        "output_sha256": hashlib.sha256(patched).hexdigest(),
        "public_key_sha256": hashlib.sha256(public_pem).hexdigest(),
        "file_offsets": ",".join(hex(offset) for offset in MODULUS_OFFSETS),
        "length_each": str(MODULUS_SIZE),
        "changes": "DIAGNOSTIC ONLY: shared lab RSA modulus in all three factory validators; signed synthetic EEPROM; verification instructions retained",
    }
    return replace_file(entries, PATH, bytes(patched)), image, public_pem, record
