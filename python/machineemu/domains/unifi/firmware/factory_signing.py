"""Opt-in RSA factory-record signing primitives for diagnostic lab bundles."""
from __future__ import annotations

import hashlib
from dataclasses import dataclass
from pathlib import Path

from .models import FirmwareError


@dataclass(frozen=True)
class SignatureLayout:
    digest_start: int = 0x1000
    iv_offset: int = 0xBD40
    encrypted_digest_offset: int = 0xBDC0
    signature_offset: int = 0xBE00


DEFAULT_LAYOUT = SignatureLayout()


def double_sha512(data: bytes) -> bytes:
    return hashlib.sha512(hashlib.sha512(data).digest()).digest()


def encode_modulus(number: int) -> bytes:
    if number.bit_length() != 4096 or not number & 1:
        raise FirmwareError("lab modulus must be an odd 4096-bit integer")
    return b"".join(((number >> (28 * index)) & 0xFFFFFFF).to_bytes(4, "little") for index in range(147))


def load_lab_key(path: Path):
    try:
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.primitives.asymmetric import rsa
    except ImportError as exc:
        raise FirmwareError("lab signing requires cryptography; install machineemu[lab]") from exc
    if path.stat().st_size > 16384:
        raise FirmwareError("lab key PEM is unexpectedly large")
    try:
        key = serialization.load_pem_private_key(path.read_bytes(), password=None)
    except (ValueError, TypeError) as exc:
        raise FirmwareError("expected an unencrypted PEM lab RSA private key") from exc
    if not isinstance(key, rsa.RSAPrivateKey) or key.key_size != 4096:
        raise FirmwareError("lab key must be RSA-4096")
    if key.public_key().public_numbers().e != 65537:
        raise FirmwareError("firmware requires public exponent 65537")
    return key


def sign_factory_image(image: bytes, key, serial: bytes, iv: bytes,
                       layout: SignatureLayout = DEFAULT_LAYOUT) -> tuple[bytes, bytes]:
    try:
        from cryptography.hazmat.decrepit.ciphers.algorithms import Blowfish
        from cryptography.hazmat.primitives import hashes, serialization
        from cryptography.hazmat.primitives.asymmetric import padding, utils
        from cryptography.hazmat.primitives.ciphers import Cipher, modes
    except ImportError as exc:
        raise FirmwareError("lab signing requires cryptography; install machineemu[lab]") from exc
    if not (0 <= layout.digest_start < layout.iv_offset
            and layout.iv_offset + 8 <= layout.encrypted_digest_offset
            and layout.encrypted_digest_offset + 64 <= layout.signature_offset
            and layout.signature_offset + 512 <= len(image)):
        raise FirmwareError("invalid factory signature layout")
    if len(iv) != 8 or not 7 <= len(serial) <= 128:
        raise FirmwareError("factory signing requires an 8-byte IV and a 7–128 byte serial")
    if key.key_size != 4096 or key.public_key().public_numbers().e != 65537:
        raise FirmwareError("factory signing requires RSA-4096 with exponent 65537")
    result = bytearray(image)
    result[layout.iv_offset : layout.iv_offset + 8] = iv
    digest = double_sha512(bytes(result[layout.digest_start : layout.iv_offset]))
    encryptor = Cipher(Blowfish(serial[-56:]), modes.CBC(iv)).encryptor()
    result[layout.encrypted_digest_offset : layout.encrypted_digest_offset + 64] = encryptor.update(digest) + encryptor.finalize()
    signed_digest = double_sha512(bytes(result[layout.digest_start : layout.signature_offset]))
    signature = key.sign(signed_digest, padding.PKCS1v15(), utils.Prehashed(hashes.SHA512()))
    public = key.public_key()
    public.verify(signature, signed_digest, padding.PKCS1v15(), utils.Prehashed(hashes.SHA512()))
    result[layout.signature_offset : layout.signature_offset + 512] = signature
    return bytes(result), public.public_bytes(serialization.Encoding.PEM, serialization.PublicFormat.SubjectPublicKeyInfo)
