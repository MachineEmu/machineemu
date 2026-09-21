"""Describe a QEMU launch without creating state or starting a process."""

from __future__ import annotations

from dataclasses import dataclass
import base64
import json
import os
from pathlib import Path
from typing import Mapping

from .machine import (accelerator_arguments, disk_plan, firmware_arguments, memory_argument,
                      nvram_plan, smp_argument, storage_arguments, tpm_arguments)
from .resolve import ProfileError, ResolvedProfile


@dataclass(frozen=True)
class LaunchPlan:
    command: tuple[str, ...]
    environment: Mapping[str, str]
    manifest: dict[str, object]


def _analysis_arguments(analysis: dict[str, object]) -> list[str]:
    identity = analysis.get("identity")
    if not isinstance(identity, dict) or not isinstance(identity.get("uuid"), str):
        raise ProfileError("analysis identity metadata is invalid")
    serials = identity.get("serials")
    if not isinstance(serials, dict):
        raise ProfileError("analysis identity serials are invalid")
    arguments = ["-uuid", identity["uuid"]]
    smbios = analysis.get("smbios", {})
    if not isinstance(smbios, dict):
        raise ProfileError("analysis SMBIOS metadata is invalid")
    fields = {
        0: (("vendor", "bios_vendor"), ("version", "bios_version")),
        1: (("manufacturer", "system_manufacturer"), ("product", "system_product"), ("version", "system_version")),
        2: (("manufacturer", "board_manufacturer"), ("product", "board_product"), ("version", "board_version")),
        3: (("manufacturer", "chassis_manufacturer"), ("version", "chassis_version"), ("asset", "chassis_asset"), ("sku", "chassis_sku")),
        4: (("manufacturer", "processor_manufacturer"), ("version", "processor_version"), ("asset", "processor_asset"),
            ("part", "processor_part"), ("sock_pfx", "processor_socket_prefix")),
        17: (("manufacturer", "memory_manufacturer"), ("part", "memory_part"), ("asset", "memory_asset"),
             ("bank", "memory_bank"), ("loc_pfx", "memory_locator_prefix")),
    }
    numeric = {
        4: (("max-speed", "processor_max_speed"), ("current-speed", "processor_current_speed"),
            ("processor-family", "processor_family"), ("processor-id", "processor_id")),
        17: (("speed", "memory_speed"),),
    }
    serial_names = {1: "system", 2: "board", 3: "chassis", 4: "processor", 17: "memory"}
    for smbios_type, options in fields.items():
        values = [f"type={smbios_type}"]
        for qemu_key, profile_key in options:
            value = smbios.get(profile_key)
            if isinstance(value, str):
                values.append(f"{qemu_key}={value.replace(',', ',,')}")
        for qemu_key, profile_key in numeric.get(smbios_type, ()):
            if smbios.get(profile_key) is not None:
                values.append(f"{qemu_key}={smbios[profile_key]}")
        if smbios_type == 0 and smbios.get("bios_vm") is not None:
            values.append(f'vm={str(smbios["bios_vm"]).lower()}')
        if smbios_type == 1:
            values.append(f"uuid={identity['uuid']}")
        if smbios_type in serial_names:
            serial = serials.get(serial_names[smbios_type])
            if not isinstance(serial, str) or not serial:
                raise ProfileError(f"analysis identity is missing {serial_names[smbios_type]} serial")
            values.append(f"serial={serial.replace(',', ',,')}")
        if len(values) > 1:
            arguments.extend(("-smbios", ",".join(values)))
    return arguments


def _asset_drive(kind: str | None, path: Path) -> str:
    """Map a declared external asset kind onto its QEMU -drive wiring."""
    if kind is None:
        return f"file={path},if=virtio,format=raw"
    wiring = {
        "raw-disk": f"file={path},if=virtio,format=raw",
        "qcow2-disk": f"file={path},if=virtio,format=qcow2",
        "qcow2-analysis-baseline": f"file={path},if=virtio,format=qcow2",
        "ovmf-code": f"file={path},if=pflash,format=raw,unit=0,readonly=on",
        "ovmf-vars": f"file={path},if=pflash,format=raw,unit=1",
        "iso": f"file={path},media=cdrom,readonly=on",
    }.get(kind)
    if wiring is None:
        raise ProfileError(f"asset kind {kind!r} has no launch wiring")
    return wiring


def _check_socket_length(path: Path, what: str) -> None:
    """Unix socket paths are capped near 108 bytes by the kernel."""
    if len(os.fsencode(path)) > 100:
        raise ProfileError(f"runtime directory is too long for {what}")


def _claimed_assets(firmware: object, storage: object) -> set[str]:
    """Asset names a structured block already attached, so they are not re-added."""
    claimed: set[str] = set()
    if isinstance(firmware, dict):
        for half in ("loader", "nvram"):
            value = firmware.get(half)
            if isinstance(value, dict) and isinstance(value.get("asset"), str):
                claimed.add(value["asset"])
    if isinstance(storage, dict):
        disk = storage.get("disk")
        if isinstance(disk, dict) and isinstance(disk.get("asset"), str):
            claimed.add(disk["asset"])
    return claimed


def _qemu_machine_value(value: object) -> str:
    return str(value).replace(",", ",,")


def _analysis_machine_suffix(profile: ResolvedProfile) -> str:
    if profile.analysis is None:
        return ""
    if profile.analysis_payload is None:
        raise ProfileError("analysis profile payload is unavailable")
    encoded = base64.b64encode(json.dumps(
        profile.analysis_payload, sort_keys=True, separators=(",", ":")
    ).encode()).decode("ascii")
    suffix = ["analysis-profile=on", f"x-analysis-profile-json-base64={encoded}"]
    fields = (("acpi", {
        "oem_id": "x-oem-id", "oem_table_id": "x-oem-table-id", "oem_revision": "x-oem-revision",
        "creator_id": "x-creator-id", "creator_revision": "x-creator-revision",
    }), ("sensors", {
        "temperature_celsius": "x-analysis-temp-c", "passive_celsius": "x-analysis-passive-temp-c",
        "critical_celsius": "x-analysis-critical-temp-c", "fan_rpm": "x-analysis-fan-rpm",
    }), ("pci", {
        "subsystem_vendor_id": "x-analysis-pci-subsystem-vendor-id", "subsystem_id": "x-analysis-pci-subsystem-id",
    }))
    for source, names in fields:
        values = profile.analysis.get(source, {})
        if isinstance(values, dict):
            suffix.extend(f"{name}={_qemu_machine_value(values[key])}" for key, name in names.items() if key in values)
    return ",".join(suffix)


def build_launch_plan(profile: ResolvedProfile, runtime_dir: Path,
                      state_dir: Path | None = None) -> LaunchPlan:
    """Build deterministic QEMU argv from an already-resolved profile.

    `state_dir` is the instance's durable directory. Writable machine state --
    UEFI variables and TPM state -- lives there so it survives across sessions
    and is never written back into the shared asset store. Without one (a dry
    run or preview) that state is session-scoped and thrown away.
    """
    runtime_dir = runtime_dir.resolve()
    machine_state = (state_dir or runtime_dir).resolve()
    resources = profile.configuration.get("resources", {})
    if not isinstance(resources, dict):
        raise ProfileError("profile.resources must be a mapping")
    memory = memory_argument(resources.get("memory"))
    vcpus = resources.get("vcpus", 1)
    if not isinstance(vcpus, int) or isinstance(vcpus, bool) or vcpus < 1:
        raise ProfileError("profile.resources.vcpus must be a positive integer")
    network = profile.configuration.get("network", {"type": "user"})
    if not isinstance(network, dict) or network.get("type") not in {"disabled", "user", "bridge"}:
        raise ProfileError("profile.network is invalid")
    cpu = profile.configuration.get("cpu", "max")
    if not isinstance(cpu, str) or not cpu:
        raise ProfileError("profile.cpu is invalid")

    smm = profile.configuration.get("smm", False)
    if not isinstance(smm, bool):
        raise ProfileError("profile.smm must be a boolean")

    qmp_socket = runtime_dir / "sockets" / "qmp.sock"
    pidfile = runtime_dir / "control" / "qemu.pid"
    machine_argument = profile.machine
    if smm:
        machine_argument += ",smm=on"
    if profile.analysis is not None:
        machine_argument += "," + _analysis_machine_suffix(profile)
    command: list[str] = [
        str(profile.executable),
        "-machine", machine_argument,
        *accelerator_arguments(resources),
        "-cpu", cpu,
        "-m", memory,
        "-smp", smp_argument(resources, vcpus),
        "-qmp", f"unix:{qmp_socket},server=on,wait=off",
        "-pidfile", str(pidfile),
    ]
    network_type = network["type"]
    if network_type == "disabled":
        command.extend(("-nic", "none"))
    elif network_type == "user":
        command.extend(("-nic", "user"))
    else:
        bridge = network.get("bridge")
        if not isinstance(bridge, str) or not bridge:
            raise ProfileError("profile.network.bridge is required for bridge networking")
        command.extend(("-nic", f"bridge,br={bridge}"))
    console = profile.configuration.get("console", {})
    if not isinstance(console, dict):
        raise ProfileError("profile.console must be a mapping")
    uart_socket: Path | None = None
    if console.get("uart") is True:
        uart_socket = runtime_dir / "sockets" / "uart.sock"
        _check_socket_length(uart_socket, "a UART socket")
        command.extend((
            "-chardev", f"socket,id=machineemu-uart,path={uart_socket},server=on,wait=off",
            "-serial", "chardev:machineemu-uart",
        ))
    devices = profile.configuration.get("devices", {})
    if not isinstance(devices, dict):
        raise ProfileError("profile.devices must be a mapping")
    display_sockets: dict[str, Path] = {}
    for name in ("vnc", "video"):
        if devices.get(name) is True:
            display_sockets[name] = runtime_dir / "sockets" / f"{name}.sock"
    if "vnc" in display_sockets:
        command.extend(("-vnc", f"unix:{display_sockets['vnc']}"))
    debug = profile.configuration.get("debug", {})
    if not isinstance(debug, dict):
        raise ProfileError("profile.debug must be a mapping")
    gdb_endpoint: dict[str, object] | None = None
    if debug.get("enabled") is True:
        transport = debug.get("transport", "unix")
        if transport == "tcp":
            host = debug.get("listen", "127.0.0.1")
            port = debug.get("port")
            if not isinstance(host, str) or not host or type(port) is not int or not 1 <= port <= 65535:
                raise ProfileError("profile.debug TCP endpoint is invalid")
            command.extend(("-gdb", f"tcp:{host}:{port}"))
            gdb_endpoint = {"transport": "tcp", "host": host, "port": port}
        elif transport == "unix":
            endpoint = runtime_dir / "sockets" / "gdb.sock"
            command.extend(("-gdb", f"unix:{endpoint}"))
            gdb_endpoint = {"transport": "unix", "path": str(endpoint)}
        else:
            raise ProfileError("profile.debug.transport must be tcp or unix")
    firmware = profile.configuration.get("firmware")
    storage = profile.configuration.get("storage")
    command.extend(firmware_arguments(firmware, profile.assets, machine_state))
    command.extend(storage_arguments(storage, profile.assets, profile.machine, profile.analysis,
                                     machine_state))
    nvram = nvram_plan(firmware, profile.assets, machine_state)
    disk = disk_plan(storage, profile.assets, machine_state)
    tpm_socket: Path | None = None
    tpm = profile.configuration.get("tpm")
    if tpm is not None:
        tpm_socket = runtime_dir / "sockets" / "tpm.sock"
        _check_socket_length(tpm_socket, "a TPM control socket")
        command.extend(tpm_arguments(tpm, tpm_socket))
    claimed = _claimed_assets(firmware, storage)
    for name, asset in sorted(profile.assets.items()):
        if not name:
            raise ProfileError("profile asset names must be non-empty")
        if name in claimed:
            continue
        command.extend(("-drive", _asset_drive(profile.asset_kinds.get(name), asset)))
    analysis_arguments: list[str] = []
    if profile.analysis is not None:
        analysis_arguments = _analysis_arguments(profile.analysis)
        command.extend(analysis_arguments)
    manifest = {
        "schema_version": 1,
        "profile_id": profile.profile_id,
        "target": profile.target,
        "machine": profile.machine,
        "machine_argument": machine_argument,
        "engine": {
            "track_id": profile.engine.track_id,
            "build_digest": profile.engine.build_digest,
        },
        "resources": {"memory": memory, "vcpus": vcpus},
        "network": dict(network),
        "cpu": cpu,
        "assets": {name: str(path) for name, path in sorted(profile.assets.items())},
        "qmp_socket": str(qmp_socket),
        "pidfile": str(pidfile),
        "argv": command,
    }
    if profile.analysis is not None:
        manifest["analysis"] = profile.analysis
        manifest["analysis_argv"] = analysis_arguments
    if uart_socket is not None:
        manifest["uart_socket"] = str(uart_socket)
    if nvram is not None:
        manifest["firmware"] = {"nvram": {"path": str(nvram["path"]), "seed": str(nvram["seed"])}}
    if disk is not None:
        manifest["storage"] = {"disk": {"path": str(disk["path"]), "backing": str(disk["backing"]),
                                       "backing_format": disk["backing_format"]}}
    if tpm_socket is not None:
        backend = tpm.get("backend", {}) if isinstance(tpm, dict) else {}
        manifest["tpm"] = {
            "socket": str(tpm_socket),
            "state": str(machine_state / "tpm"),
            "version": backend.get("version", "2.0") if isinstance(backend, dict) else "2.0",
        }
    for name, endpoint in display_sockets.items():
        manifest[f"{name}_socket"] = str(endpoint)
    if gdb_endpoint is not None:
        manifest["gdb"] = gdb_endpoint
    helper_sockets = {
        "wifi_hwsim": "hwsim_control_socket",
        "bluetooth_control": "bluetooth_control_socket",
    }
    for device_name, manifest_name in helper_sockets.items():
        if devices.get(device_name) is True:
            manifest[manifest_name] = str(runtime_dir / "sockets" / f"{device_name}.sock")
    return LaunchPlan(tuple(command), {}, manifest)
