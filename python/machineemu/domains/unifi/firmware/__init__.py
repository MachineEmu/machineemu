"""Bounded firmware-format and prepared-bundle validation for UniFi devices."""

from .formats import Section, container, decompress, fdt, fit, uimage
from .models import Bundle, FirmwareError, PrepareOptions, load_bundle
from .patches import Entry, read_cpio, replace_file, set_passwords, write_cpio
from .udm_pro_gpt import write_template as write_udm_pro_template
from .udm_pro_spi import eeprom as udm_pro_eeprom
from .udm_pro_spi import write_template as write_udm_pro_spi_template

__all__ = [
    "Bundle",
    "FirmwareError",
    "Entry",
    "PrepareOptions",
    "Section",
    "container",
    "decompress",
    "fdt",
    "fit",
    "load_bundle",
    "read_cpio",
    "replace_file",
    "set_passwords",
    "uimage",
    "udm_pro_eeprom",
    "write_cpio",
    "write_udm_pro_template",
    "write_udm_pro_spi_template",
]
