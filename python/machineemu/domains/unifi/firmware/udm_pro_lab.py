"""Diagnostic lab signing for the synthetic UDM-Pro identity record."""
from __future__ import annotations

import hashlib
from pathlib import Path

from .factory_signing import encode_modulus, load_lab_key, sign_factory_image
from .models import FirmwareError
from .udm_pro_factory_auth import BEFORE, OFFSET, PATH, SOURCE_SHA256

MODULUS_OFFSET = 0x8D4038
MODULUS_ADDRESS = 0xB14038
MODULUS_SIZE = 588
OTP_SERIAL = bytes(13)
IV = b"UDMPLAB!"


def lab_factory(original: bytes, eeprom: bytes, key_path: Path) -> tuple[bytes, bytes, bytes, dict[str, str]]:
    if hashlib.sha256(original).hexdigest() != SOURCE_SHA256:
        raise FirmwareError("lab signing requires stock hash-pinned UDM-Pro 5.1.19 udapi-server")
    if original[OFFSET : OFFSET + len(BEFORE)] != BEFORE:
        raise FirmwareError("lab signing refuses a bypassed factory check")
    if (len(eeprom) != 65536 or eeprom[:12] != bytes.fromhex("5254004d50015254004d5002")
            or eeprom[0x0E : 0x10] != bytes.fromhex("0777")):
        raise FirmwareError("lab signing requires the synthetic UDM-Pro EEPROM identity")
    key = load_lab_key(key_path)
    modulus = encode_modulus(key.public_key().public_numbers().n)
    patched = original[:MODULUS_OFFSET] + modulus + original[MODULUS_OFFSET + MODULUS_SIZE :]
    image = bytearray(eeprom)
    image[0x10 : 0x14] = (10).to_bytes(4, "big")
    image[0xA000 : 0xA002] = b"\x12\x02"
    image[0xA01E : 0xA022] = image[0x0C : 0x10]
    image[0xA022 : 0xA028] = image[:6]
    image[0xA02E : 0xA032] = bytes.fromhex("00ef4017")
    image[0xA032] = len(OTP_SERIAL)
    image[0xA033 : 0xA040] = OTP_SERIAL
    image[0xA0B3 : 0xA0B7] = bytes.fromhex("411fd073")
    image[0xA0B7 : 0xA0BB] = image[0x10 : 0x14]
    image, public_pem = sign_factory_image(bytes(image), key, OTP_SERIAL, IV)
    return patched, image, public_pem, {
        "type": "factory-lab-key", "path": PATH, "revision": "1", "source_sha256": SOURCE_SHA256,
        "output_sha256": hashlib.sha256(patched).hexdigest(), "public_key_sha256": hashlib.sha256(public_pem).hexdigest(),
        "file_offset": hex(MODULUS_OFFSET), "virtual_address": hex(MODULUS_ADDRESS), "length": str(MODULUS_SIZE),
        "changes": "DIAGNOSTIC ONLY: lab RSA modulus and signed synthetic EEPROM; verification instructions retained",
    }
