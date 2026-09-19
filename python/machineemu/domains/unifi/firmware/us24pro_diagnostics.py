"""Opt-in hash-pinned US24Pro derived-initramfs diagnostics."""
from __future__ import annotations

import hashlib
import os
import struct
import tempfile
from pathlib import Path

from .formats import MAX_PAYLOAD
from .models import FirmwareError
from .patches import read_cpio, replace_file, write_cpio

REVISION = "us24pro-sdk-delay-calibration-1"
SWITCHDRVR_PATH = "bin/switchdrvr"
SWITCHDRVR_SHA256 = "c25e78df5dd4aac7fa33d3f146e470cfcd3fd260755313cd8659219831ddd8d1"
ROUTINE_VA = 0x1766268
FACTOR_VA = 0x378ABF0
BEFORE = bytes.fromhex("f0472de9ec4b0ae3784340e30070a0e1043094e5")
AFTER = bytes.fromhex("08109fe50120a0e3002081e51eff2fe1f0ab7803")


def _elf32_file_offset(data: bytes, virtual_address: int) -> int:
    if len(data) < 52 or data[:6] != b"\x7fELF\x01\x01":
        raise FirmwareError("SDK calibration target is not little-endian ELF32")
    header = struct.unpack_from("<16sHHIIIIIHHHHHH", data)
    machine, phoff, phentsize, phnum = header[2], header[5], header[9], header[10]
    if machine != 40 or phentsize != 32 or phnum > 128 or phoff + phentsize * phnum > len(data):
        raise FirmwareError("SDK calibration target has unsupported ARM ELF headers")
    matches = []
    for index in range(phnum):
        kind, offset, vaddr, _, filesz, _, _, _ = struct.unpack_from("<IIIIIIII", data, phoff + index * phentsize)
        if kind == 1 and vaddr <= virtual_address < vaddr + filesz:
            candidate = offset + virtual_address - vaddr
            if candidate + len(BEFORE) <= len(data):
                matches.append(candidate)
    if len(matches) != 1:
        raise FirmwareError("SDK calibration address is not in one file-backed ELF segment")
    return matches[0]


def patch_switchdrvr(data: bytes) -> tuple[bytes, dict[str, object]]:
    before = hashlib.sha256(data).hexdigest()
    if before != SWITCHDRVR_SHA256:
        raise FirmwareError("SDK calibration patch requires the hash-pinned US24PRO switchdrvr")
    offset = _elf32_file_offset(data, ROUTINE_VA)
    if data[offset:offset + len(BEFORE)] != BEFORE:
        raise FirmwareError("SDK calibration routine does not contain the verified instruction bytes")
    result = data[:offset] + AFTER + data[offset + len(AFTER):]
    return result, {"type": "sdk-delay-calibration-fast-path", "revision": REVISION, "target": SWITCHDRVR_PATH,
                    "virtual_address": f"0x{ROUTINE_VA:x}", "file_offset": f"0x{offset:x}",
                    "calibration_factor_address": f"0x{FACTOR_VA:x}", "calibration_factor": 1,
                    "preserves_r0": True, "before": BEFORE.hex(), "after": AFTER.hex(),
                    "target_sha256_before": before, "target_sha256_after": hashlib.sha256(result).hexdigest()}


def materialize_fast_sdk_delay_calibration(source: Path, output: Path) -> dict[str, object]:
    try:
        if source.stat().st_size > MAX_PAYLOAD:
            raise FirmwareError("US24PRO initramfs exceeds the diagnostic patch limit")
        archive = source.read_bytes()
    except OSError as exc:
        raise FirmwareError(f"cannot read US24PRO diagnostic initramfs: {exc}") from exc
    entries, end = read_cpio(archive, allow_root_clamped_links=True)
    if any(archive[end:]):
        raise FirmwareError("unexpected data after US24PRO diagnostic initramfs")
    matches = [entry for entry in entries if entry.name == SWITCHDRVR_PATH]
    if len(matches) != 1:
        raise FirmwareError("US24PRO diagnostic initramfs has no unique switchdrvr")
    patched, record = patch_switchdrvr(matches[0].data)
    generated = write_cpio(replace_file(entries, SWITCHDRVR_PATH, patched))
    output.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".us24pro-initramfs-", dir=output.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(generated); stream.flush(); os.fsync(stream.fileno())
        os.replace(temporary, output)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    return {**record, "source_initramfs": str(source), "source_initramfs_sha256": hashlib.sha256(archive).hexdigest(),
            "generated_initramfs": str(output), "generated_initramfs_sha256": hashlib.sha256(generated).hexdigest()}
