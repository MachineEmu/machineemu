"""Read-only host USB inventory for the authenticated hotplug picker."""

from __future__ import annotations

from pathlib import Path

SYSFS_ROOT = Path("/sys/bus/usb/devices")
DEV_ROOT = Path("/dev/bus/usb")
ROOT_HUB_VENDOR = "1d6b"


def _read(path: Path) -> str | None:
    try:
        value = path.read_text(encoding="utf-8", errors="replace").strip()
    except OSError:
        return None
    return value or None


def _read_int(path: Path) -> int | None:
    value = _read(path)
    if value is None:
        return None
    try:
        return int(value)
    except ValueError:
        return None


def list_host_usb(*, sysfs_root: Path = SYSFS_ROOT, dev_root: Path = DEV_ROOT) -> list[dict[str, object]]:
    """List attachable USB devices without opening or changing host devices."""
    if not sysfs_root.is_dir():
        return []
    devices: list[dict[str, object]] = []
    for entry in sorted(sysfs_root.iterdir()):
        if ":" in entry.name:
            continue
        bus = _read_int(entry / "busnum")
        address = _read_int(entry / "devnum")
        vendor_id = _read(entry / "idVendor")
        if bus is None or address is None or vendor_id is None or vendor_id.lower() == ROOT_HUB_VENDOR:
            continue
        node = dev_root / f"{bus:03d}" / f"{address:03d}"
        if node.is_symlink() or not node.is_char_device():
            continue
        devices.append({
            "id": f"usb-{bus}-{address}", "kind": "host",
            "hostbus": bus, "hostaddr": address,
            "vendor_id": vendor_id,
            "product_id": _read(entry / "idProduct"),
            "manufacturer": _read(entry / "manufacturer"),
            "product": _read(entry / "product"),
            "serial": _read(entry / "serial"),
        })
    devices.sort(key=lambda device: (int(device["hostbus"]), int(device["hostaddr"])))
    return devices
