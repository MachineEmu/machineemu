"""Describe a QEMU launch without creating state or starting a process."""

from __future__ import annotations

from dataclasses import dataclass
import os
from pathlib import Path
from typing import Mapping

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
    arguments = ["-uuid", identity["uuid"], "-cpu", "max,kvm=off"]
    smbios = analysis.get("smbios", {})
    if not isinstance(smbios, dict):
        raise ProfileError("analysis SMBIOS metadata is invalid")
    fields = {
        0: (("vendor", "bios_vendor"), ("version", "bios_version")),
        1: (("manufacturer", "system_manufacturer"), ("product", "system_product"), ("version", "system_version")),
        2: (("manufacturer", "board_manufacturer"), ("product", "board_product"), ("version", "board_version")),
        3: (("manufacturer", "chassis_manufacturer"), ("version", "chassis_version"), ("asset", "chassis_asset"), ("sku", "chassis_sku")),
        4: (("manufacturer", "processor_manufacturer"), ("version", "processor_version"), ("asset", "processor_asset"), ("part", "processor_part")),
        17: (("manufacturer", "memory_manufacturer"), ("part", "memory_part"), ("asset", "memory_asset"), ("bank", "memory_bank")),
    }
    serial_names = {1: "system", 2: "board", 3: "chassis", 4: "processor", 17: "memory"}
    for smbios_type, options in fields.items():
        values = [f"type={smbios_type}"]
        for qemu_key, profile_key in options:
            value = smbios.get(profile_key)
            if isinstance(value, str):
                values.append(f"{qemu_key}={value.replace(',', ',,')}")
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


def build_launch_plan(profile: ResolvedProfile, runtime_dir: Path) -> LaunchPlan:
    """Build deterministic QEMU argv from an already-resolved profile."""
    runtime_dir = runtime_dir.resolve()
    resources = profile.configuration.get("resources", {})
    if not isinstance(resources, dict):
        raise ProfileError("profile.resources must be a mapping")
    memory = resources.get("memory")
    if not isinstance(memory, (str, int)) or isinstance(memory, bool) or not memory:
        raise ProfileError("profile.resources.memory must be a non-empty size")
    vcpus = resources.get("vcpus", 1)
    if not isinstance(vcpus, int) or isinstance(vcpus, bool) or vcpus < 1:
        raise ProfileError("profile.resources.vcpus must be a positive integer")

    qmp_socket = runtime_dir / "sockets" / "qmp.sock"
    pidfile = runtime_dir / "control" / "qemu.pid"
    command: list[str] = [
        str(profile.executable),
        "-machine", profile.machine,
        "-m", str(memory),
        "-smp", str(vcpus),
        "-qmp", f"unix:{qmp_socket},server=on,wait=off",
        "-pidfile", str(pidfile),
    ]
    console = profile.configuration.get("console", {})
    if not isinstance(console, dict):
        raise ProfileError("profile.console must be a mapping")
    uart_socket: Path | None = None
    if console.get("uart") is True:
        uart_socket = runtime_dir / "sockets" / "uart.sock"
        if len(os.fsencode(uart_socket)) > 100:
            raise ProfileError("runtime directory is too long for a UART socket")
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
    for name, asset in sorted(profile.assets.items()):
        if not name:
            raise ProfileError("profile asset names must be non-empty")
        command.extend(("-drive", f"file={asset},if=virtio,format=raw"))
    analysis_arguments: list[str] = []
    if profile.analysis is not None:
        analysis_arguments = _analysis_arguments(profile.analysis)
        command.extend(analysis_arguments)
    manifest = {
        "schema_version": 1,
        "profile_id": profile.profile_id,
        "target": profile.target,
        "machine": profile.machine,
        "engine": {
            "track_id": profile.engine.track_id,
            "build_digest": profile.engine.build_digest,
        },
        "resources": {"memory": memory, "vcpus": vcpus},
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
