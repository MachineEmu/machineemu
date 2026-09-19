"""Generate the U6+ model EEPROM through the QEMU-owned board-tools binary."""
from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

from .models import FirmwareError

SIZE = 65536
SEED_PREFIX = bytes.fromhex("020000798101020000798102a642077700030001")
ENV_OVERRIDE = "MACHINEEMU_BOARD_TOOLS"


def binary_path() -> str:
    override = os.environ.get(ENV_OVERRIDE)
    if override:
        if not Path(override).is_file():
            raise FirmwareError(f"{ENV_OVERRIDE} does not name a file: {override}")
        return override
    found = shutil.which("board-tools")
    if found:
        return found
    raise FirmwareError(
        "board-tools is required to generate the U6+ EEPROM seed; build the QEMU repository, "
        f"set {ENV_OVERRIDE}, or provide an EEPROM template"
    )


def validate(image: bytes) -> bytes:
    if len(image) != SIZE:
        raise FirmwareError(f"generated U6+ EEPROM must be {SIZE} bytes, got {len(image)}")
    if image[:len(SEED_PREFIX)] != SEED_PREFIX:
        raise FirmwareError("generated U6+ EEPROM head record is not the model seed")
    if image[0x8000:0x8004] != b"UBNT":
        raise FirmwareError("generated U6+ EEPROM lacks the SBD record")
    return image


def write_template(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run([binary_path(), "eeprom-gen", str(path)], check=True, capture_output=True)
    validate(path.read_bytes())
    return path
