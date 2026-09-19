import pytest

from machineemu.domains.unifi.firmware.models import FirmwareError
from machineemu.domains.unifi.firmware.u6plus_eeprom import SEED_PREFIX, SIZE, validate


def test_u6plus_eeprom_validation_requires_model_records():
    image = bytearray(SIZE)
    image[:len(SEED_PREFIX)] = SEED_PREFIX
    image[0x8000:0x8004] = b"UBNT"
    assert validate(bytes(image)) == bytes(image)
    with pytest.raises(FirmwareError, match="SBD"):
        validate(bytes(image[:0x8000] + b"FAIL" + image[0x8004:]))
