"""Read-only host inventory used to seed an analysis profile."""
from __future__ import annotations

import os
import platform
from pathlib import Path
from typing import Any


def _clean(value: object, limit: int = 64) -> str | None:
    if value is None:
        return None
    text = " ".join("".join(ch for ch in str(value).strip().strip("\x00") if ch == " " or ch.isprintable()).split())
    return text[:limit] or None


def _read(path: Path, limit: int = 64) -> str | None:
    try:
        return _clean(path.read_text(encoding="utf-8", errors="ignore"), limit)
    except OSError:
        return None


def _linux_cpu(root: Path) -> str | None:
    for line in _read(root / "proc/cpuinfo", 4096).splitlines() if _read(root / "proc/cpuinfo", 4096) else []:
        if ":" in line and line.split(":", 1)[0].strip() in {"model name", "Hardware", "Processor", "cpu model"}:
            return _clean(line.split(":", 1)[1])
    return None


def _linux_memory(root: Path) -> str:
    text = _read(root / "proc/meminfo", 4096) or ""
    for line in text.splitlines():
        if line.startswith("MemTotal:"):
            try:
                kib = int(line.split()[1])
                return f"{max(1, (kib + 1024 * 1024 - 1) // (1024 * 1024))}G"
            except (IndexError, ValueError):
                break
    return "8G"


def _linux_smbios(root: Path, privileged: bool) -> dict[str, str]:
    base = root / "sys/class/dmi/id"
    fields = {"bios_vendor": "bios_vendor", "bios_version": "bios_version", "system_manufacturer": "sys_vendor",
              "system_product": "product_name", "system_version": "product_version", "board_manufacturer": "board_vendor",
              "board_product": "board_name", "board_version": "board_version", "chassis_manufacturer": "chassis_vendor",
              "chassis_version": "chassis_version", "chassis_asset": "chassis_asset_tag"}
    result = {key: value for key, filename in fields.items() if (value := _read(base / filename))}
    if privileged:
        for key, filename in {"system": "product_serial", "board": "board_serial", "chassis": "chassis_serial"}.items():
            value = _read(base / filename)
            if value and value.lower() not in {"none", "to be filled by o.e.m.", "default string"}:
                result[key] = value
    return result


def _linux_storage(root: Path) -> dict[str, str]:
    try:
        candidates = sorted((root / "sys/block").iterdir(), key=lambda path: path.name)
    except OSError:
        candidates = []
    for item in candidates:
        if item.name.startswith(("loop", "ram", "dm-")):
            continue
        return {"disk_vendor": _read(item / "device/vendor", 8) or "ATA",
                "disk_product": _read(item / "device/model", 16) or "SATA SSD"}
    return {"disk_vendor": "ATA", "disk_product": "SATA SSD"}


def _linux_sensors(root: Path) -> dict[str, int]:
    temperature = None
    for pattern in ("sys/class/hwmon/hwmon*/temp*_input", "sys/class/thermal/thermal_zone*/temp"):
        for path in sorted(root.glob(pattern)):
            try:
                raw = int(path.read_text(encoding="utf-8").strip())
            except (OSError, ValueError):
                continue
            value = raw // 1000 if raw > 1000 else raw
            if -20 <= value <= 120:
                temperature = value
                break
        if temperature is not None:
            break
    fan = None
    for path in sorted(root.glob("sys/class/hwmon/hwmon*/fan*_input")):
        try:
            value = int(path.read_text(encoding="utf-8").strip())
        except (OSError, ValueError):
            continue
        if 0 <= value <= 20000:
            fan = value
            break
    current = temperature if temperature is not None else 42
    return {"temperature_celsius": current, "passive_celsius": max(current, 75),
            "critical_celsius": max(current, 95), "fan_rpm": fan if fan is not None else 1200}


def dump_host_profile(identity_seed: str = "host-profile", *, root: Path = Path("/"), privileged: bool = False) -> dict[str, Any]:
    system = platform.system() or "Unknown"
    cpu = (_linux_cpu(root) if system == "Linux" else None) or _clean(platform.processor() or platform.machine()) or "Host CPU"
    smbios = {"bios_vendor": "Host Firmware", "bios_version": platform.version()[:64] or "Host BIOS",
              "bios_vm": False, "system_manufacturer": "Host System", "system_product": platform.node()[:64] or f"{system} PC",
              "system_version": platform.release()[:64] or "1.0", "board_manufacturer": "Host Board",
              "board_product": "Host Board", "board_version": "1.0", "chassis_manufacturer": "Host Chassis",
              "chassis_version": "1.0", "processor_manufacturer": "Host CPU", "processor_version": cpu,
              "processor_socket_prefix": "CPU", "memory_manufacturer": "Host Memory", "memory_locator_prefix": "DIMM",
              "memory_bank": "BANK 0"}
    if system == "Linux":
        smbios.update(_linux_smbios(root, privileged))
    storage = _linux_storage(root) if system == "Linux" else {"disk_vendor": "ATA", "disk_product": "SATA SSD"}
    storage.update({"disk_serial_prefix": "ANSSD", "optical_vendor": "ATA", "optical_product": "DVD-ROM"})
    return {"analysis": {"enabled": True, "profile": "malware-analysis", "identity_seed": identity_seed,
                          "telemetry": True, "patch_revision": "machineemu-analysis-1", "smbios": smbios,
                          "acpi": {"oem_id": "ANALY", "oem_table_id": "ANALYSIS", "oem_revision": 1,
                                   "creator_id": "ALAB", "creator_revision": 1},
                          "device_descriptors": {"storage": storage,
                              "display": {"vendor": "DEL", "name": "DELL P2419H", "serial": "10000001",
                                           "xres": 1920, "yres": 1080, "width_mm": 527, "height_mm": 296,
                                           "refresh_rate": 60000},
                              "usb": {"hid": {"manufacturer": "Wacom Co.,Ltd.", "product": "Wacom Tablet",
                                              "serial": "WTAB10000001", "vendorid": 0x056A, "productid": 0x00B9, "bcd_device": 0x0100},
                                      "storage": {"manufacturer": "SanDisk", "product": "Ultra USB 3.0",
                                                  "serial_prefix": "USBSSD", "vendorid": 0x0781, "productid": 0x5581, "bcd_device": 0x0100}}},
                          "sensors": _linux_sensors(root) if system == "Linux" else {"temperature_celsius": 42, "passive_celsius": 75, "critical_celsius": 95, "fan_rpm": 1200}},
            "adapter": "pc", "network": {"type": "disabled"}, "os": {"type": {"arch": "x86_64", "machine": "q35"}, "smm": True},
            "cpu": {"model": "host,kvm=off,+kvm_pv_unhalt,+kvm_pv_eoi"},
            "vcpu": {"placement": "static", "current": min(os.cpu_count() or 4, 32)},
            "memory": {"value": int(_linux_memory(root)[:-1]) if system == "Linux" else 8, "unit": "GiB"},
            "host_inventory": {"source": "host-privileged" if privileged else "host-unprivileged", "os": system,
                               "release": platform.release(), "machine": platform.machine(), "cpu_model": cpu}}
