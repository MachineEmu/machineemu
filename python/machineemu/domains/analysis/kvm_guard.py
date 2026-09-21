"""Lab-only helpers for the opt-in analysis KVM guard module."""

from __future__ import annotations

from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import time
from typing import Any

DEFAULT_MODULE = Path("qemu/kernel/analysis-kvm/kvm_analysis_guard.ko")
DEFAULT_STATS = Path("/sys/kernel/debug/kvm_analysis_guard/stats")


@dataclass(frozen=True)
class GuardSession:
    session_id: str
    runtime: Path
    manifest: dict[str, Any]
    artifact_directory: Path
    qemu_pid: int | None


def _alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def session_from_record(session_id: str, runtime: Path, artifact_directory: Path,
                        manifest_path: Path) -> GuardSession:
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise ValueError(f"cannot read session manifest: {exc}") from exc
    if not isinstance(manifest, dict):
        raise ValueError("session manifest must be an object")
    analysis = manifest.get("analysis")
    if not isinstance(analysis, dict) or analysis.get("profile") != "malware-analysis":
        raise ValueError("KVM guard commands require a malware-analysis session")
    recorded = manifest.get("qemu", {})
    pid = recorded.get("tgid") if isinstance(recorded, dict) else None
    if not isinstance(pid, int):
        try:
            pid = int((runtime / "qemu.pid").read_text(encoding="ascii").strip())
        except (OSError, ValueError):
            pid = None
    return GuardSession(session_id, runtime, manifest, artifact_directory, pid)


def parse_stats(text: str) -> dict[str, int | str]:
    result: dict[str, int | str] = {}
    for raw in text.splitlines():
        if ":" not in raw:
            continue
        key, value = (part.strip() for part in raw.split(":", 1))
        if re.fullmatch(r"-?[0-9]+", value):
            try:
                result[key] = int(value)
                continue
            except ValueError:
                pass
        result[key] = value
    return result


def read_stats(stats_path: Path = DEFAULT_STATS) -> tuple[dict[str, int | str] | None, str | None]:
    try:
        return parse_stats(stats_path.read_text(encoding="utf-8")), None
    except OSError as exc:
        return None, str(exc)


def load_command(session: GuardSession, module_path: Path = DEFAULT_MODULE,
                 *, hook_exits: bool = True, hook_tsc: bool = True,
                 hyperv_fast_mode: bool | None = None) -> dict[str, Any]:
    if session.qemu_pid is None:
        raise ValueError("session does not record a QEMU PID")
    if not _alive(session.qemu_pid):
        raise ValueError(f"recorded QEMU PID is not alive: {session.qemu_pid}")
    if hyperv_fast_mode is None:
        cpu = session.manifest.get("configuration", {}).get("cpu", {})
        model = cpu.get("model", "") if isinstance(cpu, dict) else ""
        hyperv_fast_mode = "hv_" in model or "kvm_pv_" in model
    command = ["insmod", str(module_path), "lab_enable=1",
               f"target_tgid={session.qemu_pid}",
               f"hyperv_fast_mode={int(hyperv_fast_mode)}",
               f"hook_exits={int(hook_exits)}", f"hook_tsc={int(hook_tsc)}"]
    return {"session_id": session.session_id, "qemu_tgid": session.qemu_pid,
            "qemu_pid_alive": True, "module": str(module_path),
            "hyperv_fast_mode": hyperv_fast_mode, "hook_exits": hook_exits,
            "hook_tsc": hook_tsc, "command": command, "shell": " ".join(command)}


def status(session: GuardSession, stats_path: Path = DEFAULT_STATS) -> dict[str, Any]:
    stats, error = read_stats(stats_path)
    warnings: list[str] = []
    pid_alive = session.qemu_pid is not None and _alive(session.qemu_pid)
    if session.qemu_pid is None:
        warnings.append("session does not record a QEMU PID")
    elif not pid_alive:
        warnings.append(f"recorded QEMU PID is not alive: {session.qemu_pid}")
    if stats is None:
        warnings.append(f"KVM guard stats unavailable: {error}")
    else:
        if stats.get("target_tgid") != session.qemu_pid:
            warnings.append(f"KVM guard target_tgid={stats.get('target_tgid')} does not match session qemu_tgid={session.qemu_pid}")
        if stats.get("lab_enable") != 1:
            warnings.append("KVM guard lab_enable is not armed")
        if stats.get("exit_probe_hits", 0) == 0 or stats.get("exit_timing_observed", 0) == 0:
            warnings.append("KVM guard exit timing counters are still zero")
    return {"session_id": session.session_id, "qemu_tgid": session.qemu_pid,
            "qemu_pid_alive": pid_alive, "stats_path": str(stats_path),
            "stats": stats, "warnings": warnings, "ok": not warnings}


def snapshot(session: GuardSession, stats_path: Path = DEFAULT_STATS) -> dict[str, Any]:
    stats, error = read_stats(stats_path)
    if stats is None:
        raise ValueError(f"KVM guard stats unavailable: {error}")
    session.artifact_directory.mkdir(parents=True, exist_ok=True)
    payload = {"schema_version": 1, "session_id": session.session_id,
               "captured_at": time.time(), "stats_path": str(stats_path),
               "qemu_tgid": session.qemu_pid, "stats": stats,
               "warnings": status(session, stats_path)["warnings"]}
    stamp = time.strftime("%Y%m%d-%H%M%S", time.gmtime())
    path = session.artifact_directory / f"kvm-guard-stats-{stamp}.json"
    encoded = json.dumps(payload, indent=2, sort_keys=True) + "\n"
    path.write_text(encoded, encoding="utf-8")
    latest = session.artifact_directory / "kvm-guard-stats.latest.json"
    latest.write_text(encoded, encoding="utf-8")
    return {**payload, "path": str(path), "latest": str(latest)}
