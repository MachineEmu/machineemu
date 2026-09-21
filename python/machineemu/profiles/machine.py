"""Render machine, firmware, TPM, and storage arguments for x86 PC profiles.

Ported from the unifi-qemu console board renderer. Host paths in the source
configuration are replaced by names into the profile's content-addressed asset
map, so a catalog profile stays redistributable.
"""

from __future__ import annotations

from pathlib import Path
import re
from typing import Any

from .resolve import ProfileError

DISK_BUSES = {"ide", "sata", "virtio", "nvme"}
_MEMORY = re.compile(r"^(\d+)\s*(B|K|KiB|M|MiB|G|GiB|T|TiB)?$")
_MEMORY_SUFFIX = {None: "M", "B": "B", "K": "K", "KiB": "K", "M": "M", "MiB": "M",
                  "G": "G", "GiB": "G", "T": "T", "TiB": "T"}


def qemu_value(value: Any) -> str:
    return str(value).replace(",", ",,")


def memory_argument(value: Any) -> str:
    """Normalise a profile memory size into a suffix QEMU's -m accepts."""
    if isinstance(value, bool) or not isinstance(value, (str, int)):
        raise ProfileError("profile.resources.memory must be a non-empty size")
    match = _MEMORY.match(str(value).strip())
    if match is None or int(match.group(1)) <= 0:
        raise ProfileError(f"profile.resources.memory is not a supported size: {value!r}")
    return match.group(1) + _MEMORY_SUFFIX[match.group(2)]


def smp_argument(resources: dict[str, Any], vcpus: int) -> str:
    topology = resources.get("topology")
    if topology is None:
        return str(vcpus)
    if not isinstance(topology, dict):
        raise ProfileError("profile.resources.topology must be a mapping")
    values = [str(vcpus)]
    for key in ("sockets", "dies", "clusters", "cores", "threads"):
        if key not in topology:
            continue
        value = topology[key]
        if not isinstance(value, int) or isinstance(value, bool) or value < 1:
            raise ProfileError(f"profile.resources.topology.{key} must be a positive integer")
        values.append(f"{key}={value}")
    return ",".join(values)


def accelerator_arguments(resources: dict[str, Any]) -> list[str]:
    accelerator = resources.get("accelerator")
    if accelerator is None:
        return []
    if accelerator not in {"kvm", "tcg"}:
        raise ProfileError("profile.resources.accelerator must be kvm or tcg")
    if accelerator == "kvm":
        return ["-accel", "kvm"]
    thread = resources.get("accelerator_thread", "multi")
    if thread not in {"single", "multi"}:
        raise ProfileError("profile.resources.accelerator_thread must be single or multi")
    return ["-accel", f"tcg,thread={thread}"]


def _asset_path(assets: dict[str, Path], reference: Any, where: str) -> Path:
    if not isinstance(reference, str) or not reference:
        raise ProfileError(f"{where} must name a profile asset")
    if reference not in assets:
        raise ProfileError(f"{where} names an asset that is not imported: {reference}")
    return assets[reference]


def _asset(assets: dict[str, Path], reference: Any, where: str) -> str:
    return qemu_value(_asset_path(assets, reference, where))


def nvram_plan(firmware: Any, assets: dict[str, Path], state_dir: Path) -> dict[str, Path] | None:
    """Locate the machine's writable NVRAM copy and the asset that seeds it.

    UEFI variables are mutable, so QEMU must never be pointed at the imported
    asset itself: that file is content-addressed and shared by every instance
    that imported the same image.
    """
    if firmware is None:
        return None
    if not isinstance(firmware, dict):
        raise ProfileError("profile.firmware must be a mapping")
    nvram = firmware.get("nvram")
    if not isinstance(firmware.get("loader"), dict) or not isinstance(nvram, dict):
        raise ProfileError("profile.firmware requires both loader and nvram mappings")
    seed = _asset_path(assets, nvram.get("asset"), "profile.firmware.nvram.asset")
    name = nvram.get("name", "OVMF_VARS.fd")
    if not isinstance(name, str) or not name or "/" in name or name in {".", ".."}:
        raise ProfileError("profile.firmware.nvram.name must be a plain file name")
    return {"path": state_dir / name, "seed": seed}


def firmware_arguments(firmware: Any, assets: dict[str, Path], state_dir: Path) -> list[str]:
    """Render the pflash loader/nvram pair; both halves are required together."""
    plan = nvram_plan(firmware, assets, state_dir)
    if plan is None:
        return []
    loader = firmware["loader"]
    code = _asset(assets, loader.get("asset"), "profile.firmware.loader.asset")
    readonly = loader.get("readonly", True)
    secure = loader.get("secure", False)
    if not isinstance(readonly, bool) or not isinstance(secure, bool):
        raise ProfileError("profile.firmware.loader readonly and secure must be booleans")
    arguments = [
        "-drive", f'if=pflash,format=raw,unit=0,readonly={"on" if readonly else "off"},file={code}',
        "-drive", f"if=pflash,format=raw,unit=1,file={qemu_value(plan['path'])}",
    ]
    if secure:
        arguments.extend(("-global", "driver=cfi.pflash01,property=secure,value=on"))
    return arguments


def tpm_arguments(tpm: Any, socket: Path) -> list[str]:
    if tpm is None:
        return []
    if not isinstance(tpm, dict):
        raise ProfileError("profile.tpm must be a mapping")
    model = tpm.get("model", "tpm-tis")
    if model not in {"tpm-tis", "tpm-crb"}:
        raise ProfileError("profile.tpm.model must be tpm-tis or tpm-crb")
    backend = tpm.get("backend", {})
    if not isinstance(backend, dict):
        raise ProfileError("profile.tpm.backend must be a mapping")
    if backend.get("type", "emulator") != "emulator":
        raise ProfileError("profile.tpm.backend.type must be emulator")
    if backend.get("version", "2.0") not in {"1.2", "2.0"}:
        raise ProfileError('profile.tpm.backend.version must be "1.2" or "2.0"')
    device = f"{model},tpmdev=tpm0"
    if "ppi" in tpm:
        if not isinstance(tpm["ppi"], bool):
            raise ProfileError("profile.tpm.ppi must be a boolean")
        device += ",ppi=" + ("on" if tpm["ppi"] else "off")
    return [
        "-chardev", f"socket,id=chrtpm,path={qemu_value(socket)}",
        "-tpmdev", "emulator,id=tpm0,chardev=chrtpm",
        "-device", device,
    ]


def _storage_descriptors(analysis: dict[str, Any] | None) -> dict[str, Any]:
    if analysis is None:
        return {}
    descriptors = analysis.get("device_descriptors", {})
    storage = descriptors.get("storage", {}) if isinstance(descriptors, dict) else {}
    return storage if isinstance(storage, dict) else {}


def _disk_device(base: str, disk: dict[str, Any], serial: str | None, *, model: str | None = None) -> str:
    values: list[str] = []
    if model:
        values.append("model=" + qemu_value(model))
    if serial:
        values.append("serial=" + qemu_value(serial))
    wwn = disk.get("wwn")
    if wwn:
        values.append("wwn=" + qemu_value(wwn))
    boot_order = disk.get("boot_order")
    if boot_order is not None:
        if not isinstance(boot_order, int) or isinstance(boot_order, bool) or boot_order < 0:
            raise ProfileError("profile.storage.disk.boot_order must be a non-negative integer")
        values.append(f"bootindex={boot_order}")
    return base + ("," + ",".join(values) if values else "")


def _disk_serial(disk: dict[str, Any], descriptors: dict[str, Any], fallback: str | None = None) -> str | None:
    serial = disk.get("serial")
    if serial:
        return str(serial)
    prefix = descriptors.get("disk_serial_prefix") or fallback
    return f"{prefix}-0" if prefix else None


def disk_plan(storage: Any, assets: dict[str, Path], state_dir: Path) -> dict[str, Any] | None:
    """Locate the machine's writable disk overlay and the baseline behind it.

    The imported baseline is content-addressed and shared by every instance, so
    QEMU is never pointed at it directly: each machine boots a qcow2 overlay
    backed by it.
    """
    if storage is None:
        return None
    if not isinstance(storage, dict):
        raise ProfileError("profile.storage must be a mapping")
    disk = storage.get("disk")
    if disk is None:
        return None
    if not isinstance(disk, dict):
        raise ProfileError("profile.storage.disk must be a mapping")
    backing = _asset_path(assets, disk.get("asset"), "profile.storage.disk.asset")
    backing_format = disk.get("format", "qcow2")
    if backing_format not in {"raw", "qcow2"}:
        raise ProfileError("profile.storage.disk.format must be raw or qcow2")
    return {"path": state_dir / "overlay.qcow2", "backing": backing, "backing_format": backing_format}


def storage_arguments(storage: Any, assets: dict[str, Path], machine: str,
                      analysis: dict[str, Any] | None, state_dir: Path) -> list[str]:
    """Render the profile disk onto the bus its machine actually provides."""
    plan = disk_plan(storage, assets, state_dir)
    if plan is None:
        return []
    disk = storage["disk"]
    path = qemu_value(plan["path"])
    bus = disk.get("bus", "sata")
    if bus not in DISK_BUSES:
        raise ProfileError("profile.storage.disk.bus must be ide, sata, virtio, or nvme")
    options = f"if=none,id=pc-disk,file={path},format=qcow2"
    for field, name in (("cache", "cache"), ("aio", "aio"), ("discard", "discard"),
                        ("detect_zeroes", "detect-zeroes")):
        value = disk.get(field)
        if value is None:
            continue
        if not isinstance(value, str) or not value:
            raise ProfileError(f"profile.storage.disk.{field} must be a non-empty string")
        options += f",{name}={value}"
    if disk.get("readonly") is True:
        options += ",readonly=on"

    # i440fx exposes legacy IDE directly; q35 needs an explicit AHCI controller.
    legacy = machine == "pc" or machine.startswith("pc-i440fx")
    if legacy and bus == "sata":
        raise ProfileError("the pc machine uses bus ide; sata requires q35")
    if not legacy and bus == "ide":
        raise ProfileError("q35 machines use bus sata; ide requires the pc machine")
    descriptors = _storage_descriptors(analysis)
    arguments: list[str] = []
    if bus == "sata":
        arguments.extend(("-device", "ich9-ahci,id=pc-sata"))
    arguments.extend(("-drive", options))
    if bus in {"ide", "sata"}:
        # Only the ATA path carries the analysis storage descriptors; virtio-blk
        # and nvme have no model property to answer them with.
        attachment = "ide.0" if legacy else "pc-sata.0"
        arguments.extend(("-device", _disk_device(
            f"ide-hd,bus={attachment},drive=pc-disk", disk, _disk_serial(disk, descriptors),
            model=descriptors.get("disk_product"))))
    elif bus == "virtio":
        arguments.extend(("-device", _disk_device(
            "virtio-blk-pci,drive=pc-disk,id=pc-disk-device", disk, disk.get("serial"))))
    else:
        arguments.extend(("-device", _disk_device(
            "nvme,drive=pc-disk,id=pc-disk-device", disk, _disk_serial(disk, descriptors, "PCDISK"))))
    return arguments
