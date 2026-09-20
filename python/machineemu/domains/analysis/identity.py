"""Deterministic analysis identities derived from a non-secret operator seed."""
from __future__ import annotations

import hashlib
import re


def _digest(seed: str, label: str) -> bytes:
    return hashlib.sha256(b"unifi-qemu-analysis\0" + seed.encode() + b"\0" + label.encode()).digest()


def build_identity(seed: str, clone: str | None = None) -> dict[str, object]:
    if not isinstance(seed, str) or not seed or "\x00" in seed:
        raise ValueError("analysis identity seed must be a non-empty string")
    if clone is not None and (not isinstance(clone, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,63}", clone)):
        raise ValueError("analysis clone must be a safe identifier")
    effective = seed if clone is None else f"{seed}\0clone\0{clone}"
    raw_uuid = bytearray(_digest(effective, "uuid")[:16])
    raw_uuid[6] = (raw_uuid[6] & 0x0F) | 0x40
    raw_uuid[8] = (raw_uuid[8] & 0x3F) | 0x80
    uuid = "%02x%02x%02x%02x-%02x%02x-%02x%02x-%02x%02x-%s" % (
        *raw_uuid[:10], "".join(f"{value:02x}" for value in raw_uuid[10:]))
    mac = ":".join(f"{value:02x}" for value in bytes([2]) + _digest(effective, "mac")[:5])
    seed_hash = hashlib.sha256(seed.encode()).hexdigest()
    return {
        "uuid": uuid,
        "mac": mac,
        "serials": {kind: f"AN-{kind.upper()}-{seed_hash[:12]}"
                     for kind in ("bios", "system", "board", "chassis", "processor", "memory")},
        "identity_seed_sha256": seed_hash,
        "clone": clone,
    }
