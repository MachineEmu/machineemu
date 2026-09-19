"""Bounded firmware-format and prepared-bundle validation for UniFi devices."""

from .formats import Section, container, decompress, fdt, fit, uimage
from .models import Bundle, FirmwareError, PrepareOptions, load_bundle
from .patches import Entry, read_cpio, replace_file, set_passwords, write_cpio
from .udm_pro_gpt import write_template as write_udm_pro_template
from .udm_pro_disk import copy_region, partitions as udm_pro_partitions
from .udm_pro_spi import eeprom as udm_pro_eeprom
from .udm_pro_spi import write_template as write_udm_pro_spi_template

__all__ = [
    "Bundle",
    "FirmwareError",
    "Entry",
    "PrepareOptions",
    "Section",
    "container",
    "copy_region",
    "decompress",
    "fdt",
    "fit",
    "load_bundle",
    "read_cpio",
    "replace_file",
    "set_passwords",
    "uimage",
    "udm_pro_eeprom",
    "udm_pro_partitions",
    "write_cpio",
    "write_udm_pro_template",
    "write_udm_pro_spi_template",
]
