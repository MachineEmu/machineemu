"""Bounded firmware-format and prepared-bundle validation for UniFi devices."""

from .formats import Section, container, decompress, fdt, fit, uimage
from .models import Bundle, FirmwareError, PrepareOptions, load_bundle
from .patches import Entry, read_cpio, replace_file, set_passwords, write_cpio
from .squashfs import SquashFS, library_path as squashfs_library_path
from .service import inspect as inspect_firmware
from .service import prepare as prepare_firmware
from .udm_pro_gpt import write_template as write_udm_pro_template
from .udm_pro import inspect as inspect_udm_pro
from .udm_pro import prepare as prepare_udm_pro
from .us24pro import inspect as inspect_us24pro
from .us24pro import prepare as prepare_us24pro
from .us24pro_diagnostics import materialize_fast_sdk_delay_calibration
from .udm_pro_disk import build_disk as build_udm_pro_disk
from .udm_pro_disk import copy_region, partitions as udm_pro_partitions
from .udm_pro_spi import eeprom as udm_pro_eeprom
from .udm_pro_spi import write_template as write_udm_pro_spi_template
from .u6plus_eeprom import write_template as write_u6plus_eeprom_template

__all__ = [
    "Bundle",
    "build_udm_pro_disk",
    "FirmwareError",
    "Entry",
    "PrepareOptions",
    "Section",
    "SquashFS",
    "container",
    "copy_region",
    "decompress",
    "fdt",
    "fit",
    "inspect_udm_pro",
    "inspect_us24pro",
    "inspect_firmware",
    "load_bundle",
    "read_cpio",
    "prepare_udm_pro",
    "prepare_us24pro",
    "materialize_fast_sdk_delay_calibration",
    "prepare_firmware",
    "replace_file",
    "set_passwords",
    "squashfs_library_path",
    "uimage",
    "udm_pro_eeprom",
    "udm_pro_partitions",
    "write_cpio",
    "write_udm_pro_template",
    "write_udm_pro_spi_template",
    "write_u6plus_eeprom_template",
]
