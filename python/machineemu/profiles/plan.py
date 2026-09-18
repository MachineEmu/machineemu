"""Describe a QEMU launch without creating state or starting a process."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Mapping

from .resolve import ProfileError, ResolvedProfile


@dataclass(frozen=True)
class LaunchPlan:
    command: tuple[str, ...]
    environment: Mapping[str, str]
    manifest: dict[str, object]


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
    for name, asset in sorted(profile.assets.items()):
        if not name:
            raise ProfileError("profile asset names must be non-empty")
        command.extend(("-drive", f"file={asset},if=virtio,format=raw"))
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
    return LaunchPlan(tuple(command), {}, manifest)
