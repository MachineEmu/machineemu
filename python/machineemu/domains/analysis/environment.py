"""Write the non-secret environment report for an analysis session."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import tempfile
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from machineemu.profiles import LaunchPlan, ResolvedProfile
    from machineemu.runtime.state import SessionRecord


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _asset_report(assets: dict[str, Path]) -> dict[str, dict[str, object]]:
    result: dict[str, dict[str, object]] = {}
    for name, path in sorted(assets.items()):
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"analysis asset is unavailable: {name}")
        result[name] = {"sha256": _sha256(path), "size": path.stat().st_size}
    return result


def build_environment_report(profile: "ResolvedProfile", plan: "LaunchPlan") -> dict[str, object]:
    """Build a source-plan-compatible report without retaining secret paths."""
    analysis = profile.analysis
    if not isinstance(analysis, dict):
        raise ValueError("environment reports require an analysis profile")
    assets = _asset_report(profile.assets)
    qemu = profile.executable
    if not qemu.is_file() or qemu.is_symlink():
        raise ValueError("analysis QEMU executable is unavailable")
    configuration = profile.configuration
    resources = configuration.get("resources", {})
    network = configuration.get("network", {})
    if not isinstance(resources, dict):
        resources = {}
    network_mode = network.get("type", "user") if isinstance(network, dict) else "user"
    endpoints: dict[str, object] = {
        "qmp": {"name": "qmp.sock", "transport": "unix", "configured": True},
        "serial": {"name": "uart.sock", "transport": "unix", "configured": plan.manifest.get("uart_socket") is not None},
    }
    machine = {
        "name": profile.machine,
        "target": profile.target,
        "cpu": configuration.get("cpu", "host,kvm=off"),
        "vcpu": resources.get("vcpus", 1),
        "memory": resources.get("memory"),
        "qemu_cpu": configuration.get("cpu", "host,kvm=off"),
        "qemu_smp": str(resources.get("vcpus", 1)),
        "qemu_memory": str(resources.get("memory")),
    }
    report: dict[str, object] = {
        "schema_version": 1,
        "profile": analysis.get("profile", "malware-analysis"),
        "profile_id": profile.profile_id,
        "identity_seed_sha256": analysis.get("identity_seed_sha256"),
        "qemu": {
            "binary": qemu.name,
            "sha256": _sha256(qemu),
            "track_id": profile.engine.track_id,
            "build_digest": profile.engine.build_digest,
            "source_revision": profile.engine.source_revision,
        },
        "patch_revision": analysis.get("patch_revision"),
        "baseline": assets.get("disk"),
        "clone": analysis.get("clone"),
        "machine": machine,
        "identity": analysis.get("identity", {}),
        "smbios": analysis.get("smbios", {}),
        "acpi": analysis.get("acpi", {}),
        "device_descriptors": analysis.get("device_descriptors", {}),
        "sensors": analysis.get("sensors", {}),
        "network": {"type": network_mode},
        "assets": assets,
        "firmware": {name: value for name, value in assets.items() if "firmware" in name or "nvram" in name},
        "disk_sha256": assets.get("disk", {}).get("sha256") if isinstance(assets.get("disk"), dict) else None,
        "endpoints": endpoints,
        "guest_observed": None,
        "remaining_detectable_signals": ["hypervisor CPUID and device timing require guest verification"],
        "checks_version": "unverified",
    }
    return report


def write_environment_report(record: "SessionRecord", profile: "ResolvedProfile", plan: "LaunchPlan") -> Path:
    """Atomically write ``environment.json`` beside the session artifacts."""
    report = build_environment_report(profile, plan)
    destination = record.artifact_dir / "environment.json"
    try:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=record.artifact_dir,
                                         prefix=".environment.json.", delete=False) as stream:
            json.dump(report, stream, indent=2, sort_keys=True)
            stream.write("\n")
            temporary = Path(stream.name)
        temporary.replace(destination)
    except OSError as exc:
        raise ValueError(f"cannot write analysis environment report: {exc}") from exc
    return destination
