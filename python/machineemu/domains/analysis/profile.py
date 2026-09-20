"""Validate analysis profile options and derive non-secret launch metadata."""
from __future__ import annotations

import re
from typing import Any

from .identity import build_identity


def _ascii(value: object, name: str, limit: int) -> str:
    if not isinstance(value, str) or not value or len(value) > limit or any(ord(char) < 0x20 or ord(char) > 0x7E for char in value):
        raise ValueError(f"analysis.{name} must be printable ASCII of length 1..{limit}")
    return value


def _u16(value: object, name: str) -> int:
    if type(value) is not int or not 0 < value <= 0xFFFF:
        raise ValueError(f"analysis.{name} must be a u16 integer")
    return value


def _smbios(value: Any) -> dict[str, Any]:
    if value is None:
        value = {}
    if not isinstance(value, dict):
        raise ValueError("analysis.smbios must be a mapping")
    result = dict(value)
    limits = {"bios_vendor": 64, "bios_version": 64, "system_manufacturer": 64, "system_product": 64,
              "system_version": 64, "board_manufacturer": 64, "board_product": 64, "board_version": 64,
              "chassis_manufacturer": 64, "chassis_version": 64, "chassis_asset": 64, "chassis_sku": 64,
              "processor_manufacturer": 64, "processor_version": 64, "processor_asset": 64, "processor_part": 64,
              "memory_manufacturer": 64, "memory_bank": 64, "memory_asset": 64, "memory_part": 64}
    for key, limit in limits.items():
        if key in result:
            result[key] = _ascii(result[key], f"smbios.{key}", limit)
    for key in ("processor_max_speed", "processor_current_speed", "processor_family", "memory_speed"):
        if key in result and (type(result[key]) is not int or not 0 <= result[key] <= 0xFFFF):
            raise ValueError(f"analysis.smbios.{key} must be a u16 integer")
    if "processor_id" in result and (type(result["processor_id"]) is not int or not 0 <= result["processor_id"] <= 0xFFFFFFFFFFFFFFFF):
        raise ValueError("analysis.smbios.processor_id must be a u64 integer")
    for key in ("bios_vm",):
        if key in result and type(result[key]) is not bool:
            raise ValueError(f"analysis.smbios.{key} must be boolean")
    return result


def _acpi(value: Any) -> dict[str, Any]:
    if value is None:
        value = {}
    if not isinstance(value, dict):
        raise ValueError("analysis.acpi must be a mapping")
    result = dict(value)
    for key, limit in (("oem_id", 6), ("oem_table_id", 8), ("creator_id", 4)):
        if key in result:
            result[key] = _ascii(result[key], f"acpi.{key}", limit)
    for key in ("oem_revision", "creator_revision"):
        if key in result and (type(result[key]) is not int or not 0 <= result[key] <= 0xFFFFFFFF):
            raise ValueError(f"analysis.acpi.{key} must be a u32 integer")
    return result


def _descriptors(value: Any) -> dict[str, Any]:
    if value is None:
        value = {}
    if not isinstance(value, dict):
        raise ValueError("analysis.device_descriptors must be a mapping")
    storage = value.get("storage", {})
    display = value.get("display", {})
    usb = value.get("usb", {})
    if not all(isinstance(item, dict) for item in (storage, display, usb)):
        raise ValueError("analysis.device_descriptors storage, display, and usb must be mappings")
    storage_result = {"disk_vendor": storage.get("disk_vendor", "ATA"), "disk_product": storage.get("disk_product", "SATA SSD"),
                      "disk_serial_prefix": storage.get("disk_serial_prefix", "ANSSD"), "optical_vendor": storage.get("optical_vendor", "ATA"),
                      "optical_product": storage.get("optical_product", "DVD-ROM")}
    for key, limit in (("disk_vendor", 8), ("disk_product", 16), ("disk_serial_prefix", 12), ("optical_vendor", 8), ("optical_product", 16)):
        storage_result[key] = _ascii(storage_result[key], f"device_descriptors.storage.{key}", limit)
    display_result = {"vendor": display.get("vendor", "DEL"), "name": display.get("name", "DELL P2419H"),
                      "serial": display.get("serial", "10000001"), "xres": display.get("xres", 1920), "yres": display.get("yres", 1080),
                      "width_mm": display.get("width_mm", 527), "height_mm": display.get("height_mm", 296),
                      "refresh_rate": display.get("refresh_rate", 60000), "pci_identity": display.get("pci_identity", False),
                      "pci_identity_delay_ms": display.get("pci_identity_delay_ms", 15000),
                      "pci_vendor_id": display.get("pci_vendor_id", 0x8086), "pci_device_id": display.get("pci_device_id", 0x4680),
                      "pci_subsystem_vendor_id": display.get("pci_subsystem_vendor_id", 0x1028), "pci_subsystem_id": display.get("pci_subsystem_id", 0x0A56)}
    for key, low, high in (("xres", 640, 7680), ("yres", 480, 4320), ("width_mm", 100, 2000), ("height_mm", 100, 2000), ("refresh_rate", 24000, 240000), ("pci_identity_delay_ms", 0, 300000)):
        if type(display_result[key]) is not int or not low <= display_result[key] <= high:
            raise ValueError(f"analysis.device_descriptors.display.{key} is outside its supported range")
    display_result["vendor"] = _ascii(display_result["vendor"], "device_descriptors.display.vendor", 3)
    if not display_result["vendor"].isupper() or not display_result["vendor"].isalpha():
        raise ValueError("analysis.device_descriptors.display.vendor must be three uppercase ASCII letters")
    for key, limit in (("name", 12), ("serial", 12)):
        display_result[key] = _ascii(display_result[key], f"device_descriptors.display.{key}", limit)
    if type(display_result["pci_identity"]) is not bool:
        raise ValueError("analysis.device_descriptors.display.pci_identity must be boolean")
    for key in ("pci_vendor_id", "pci_device_id", "pci_subsystem_vendor_id", "pci_subsystem_id"):
        display_result[key] = _u16(display_result[key], f"device_descriptors.display.{key}")
    hid = usb.get("hid", {})
    usb_storage = usb.get("storage", {})
    if not isinstance(hid, dict) or not isinstance(usb_storage, dict):
        raise ValueError("analysis.device_descriptors.usb.hid and storage must be mappings")
    hid_result = {"manufacturer": hid.get("manufacturer", "Wacom Co.,Ltd."), "product": hid.get("product", "Wacom Tablet"),
                  "serial": hid.get("serial", "WTAB10000001"), "vendorid": hid.get("vendorid", 0x056A),
                  "productid": hid.get("productid", 0x00B9), "bcd_device": hid.get("bcd_device", 0x0100)}
    usb_storage_result = {"manufacturer": usb_storage.get("manufacturer", "SanDisk"), "product": usb_storage.get("product", "Ultra USB 3.0"),
                          "serial_prefix": usb_storage.get("serial_prefix", "USBSSD"), "vendorid": usb_storage.get("vendorid", 0x0781),
                          "productid": usb_storage.get("productid", 0x5581), "bcd_device": usb_storage.get("bcd_device", 0x0100)}
    for result, prefix in ((hid_result, "usb.hid"), (usb_storage_result, "usb.storage")):
        for key in ("manufacturer", "product", "serial" if prefix == "usb.hid" else "serial_prefix"):
            result[key] = _ascii(result[key], f"device_descriptors.{prefix}.{key}", 31)
        for key in ("vendorid", "productid", "bcd_device"):
            result[key] = _u16(result[key], f"device_descriptors.{prefix}.{key}")
    return {"storage": storage_result, "display": display_result, "usb": {"hid": hid_result, "storage": usb_storage_result}}


def _sensors(value: Any) -> dict[str, int]:
    if value is None:
        value = {}
    if not isinstance(value, dict):
        raise ValueError("analysis.sensors must be a mapping")
    result = {"temperature_celsius": value.get("temperature_celsius", 42), "passive_celsius": value.get("passive_celsius", 75),
              "critical_celsius": value.get("critical_celsius", 95), "fan_rpm": value.get("fan_rpm", 1200)}
    if any(type(item) is not int for item in result.values()):
        raise ValueError("analysis.sensors values must be integers")
    if not -20 <= result["temperature_celsius"] <= result["passive_celsius"] <= result["critical_celsius"] <= 127:
        raise ValueError("analysis.sensors thresholds are invalid")
    if not 0 <= result["fan_rpm"] <= 20000:
        raise ValueError("analysis.sensors.fan_rpm must be 0..20000")
    return result


def _pci(value: Any) -> dict[str, int]:
    if value is None:
        value = {}
    if not isinstance(value, dict):
        raise ValueError("analysis.pci must be a mapping")
    result = {"subsystem_vendor_id": value.get("subsystem_vendor_id", 0x1028), "subsystem_id": value.get("subsystem_id", 0x0A56)}
    return {key: _u16(item, f"pci.{key}") for key, item in result.items()}


def validate(value: Any) -> dict[str, Any] | None:
    if value is None:
        return None
    if not isinstance(value, dict):
        raise ValueError("analysis must be a mapping")
    allowed = {"enabled", "profile", "identity_seed", "clone", "collection", "overlay", "telemetry",
               "patch_revision", "smbios", "acpi", "device_descriptors", "sensors", "pci"}
    unknown = set(value) - allowed
    if unknown:
        raise ValueError(f"analysis: unknown key(s): {', '.join(sorted(unknown))}")
    if value.get("enabled") is not True:
        raise ValueError("analysis.enabled must be true")
    if value.get("profile") != "malware-analysis":
        raise ValueError("analysis.profile must be malware-analysis")
    seed = value.get("identity_seed")
    if not isinstance(seed, str) or not seed or "\x00" in seed:
        raise ValueError("analysis.identity_seed must be a non-empty string")
    clone = value.get("clone")
    if clone is not None and (not isinstance(clone, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,63}", clone)):
        raise ValueError("analysis.clone must be 1-64 safe characters")
    for key in ("collection", "overlay", "telemetry"):
        if key in value and not isinstance(value[key], bool):
            raise ValueError(f"analysis.{key} must be boolean")
    for key in ("smbios", "acpi", "device_descriptors", "sensors", "pci"):
        if key in value and not isinstance(value[key], dict):
            raise ValueError(f"analysis.{key} must be a mapping")
    revision = value.get("patch_revision", "machineemu-analysis-1")
    if not isinstance(revision, str) or not revision or "\x00" in revision:
        raise ValueError("analysis.patch_revision must be a non-empty string")
    identity = build_identity(seed, clone)
    smbios = _smbios(value.get("smbios"))
    acpi = _acpi(value.get("acpi"))
    descriptors = _descriptors(value.get("device_descriptors"))
    sensors = _sensors(value.get("sensors"))
    pci = _pci(value.get("pci"))
    return {
        "schema_version": 1,
        "profile": "malware-analysis",
        "identity_seed_sha256": identity["identity_seed_sha256"],
        "identity": identity,
        "collection": value.get("collection", False),
        "overlay": value.get("overlay", True),
        "telemetry": value.get("telemetry", True),
        "patch_revision": revision,
        "smbios": smbios,
        "acpi": acpi,
        "device_descriptors": descriptors,
        "sensors": sensors,
        "pci": pci,
    }
