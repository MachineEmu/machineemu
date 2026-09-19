"""Create isolated instance/session directories and immutable launch metadata."""

from __future__ import annotations

from dataclasses import dataclass
import json
from pathlib import Path
import re
import tempfile
from typing import Any

from machineemu.profiles import ResolvedProfile


class RuntimeStateError(ValueError):
    """Raised when runtime state cannot be safely created or inspected."""


_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")


def _validate_id(value: str, name: str) -> str:
    if not isinstance(value, str) or not _ID.fullmatch(value):
        raise RuntimeStateError(f"{name} must be an opaque runtime identifier")
    return value


@dataclass(frozen=True)
class SessionRecord:
    session_id: str
    instance_id: str
    runtime_dir: Path
    state_dir: Path
    artifact_dir: Path
    manifest: Path


class SessionStore:
    """Own per-session directories below configured, non-secret roots."""

    def __init__(self, runtime_root: Path, state_root: Path, artifact_root: Path):
        self.runtime_root = runtime_root.resolve()
        self.state_root = state_root.resolve()
        self.artifact_root = artifact_root.resolve()

    def create(self, instance_id: str, session_id: str, profile: ResolvedProfile) -> SessionRecord:
        instance_id = _validate_id(instance_id, "instance_id")
        session_id = _validate_id(session_id, "session_id")
        runtime_dir = self.runtime_root / "sessions" / session_id
        state_dir = self.state_root / "instances" / instance_id
        artifact_dir = self.artifact_root / "sessions" / session_id
        try:
            state_dir.mkdir(parents=True, exist_ok=True)
            runtime_dir.mkdir(parents=True, exist_ok=False)
            artifact_dir.mkdir(parents=True, exist_ok=False)
            for name in ("control", "sockets", "helpers", "logs"):
                (runtime_dir / name).mkdir()
        except FileExistsError as exc:
            raise RuntimeStateError(f"runtime or artifact already exists for {session_id}") from exc
        manifest = runtime_dir / "manifest.json"
        value: dict[str, Any] = {
            "schema_version": 1,
            "session_id": session_id,
            "instance_id": instance_id,
            "profile_id": profile.profile_id,
            "machine": profile.machine,
            "target": profile.target,
            "engine": {
                "track_id": profile.engine.track_id,
                "build_digest": profile.engine.build_digest,
                "source_revision": profile.engine.source_revision,
            },
            "configuration": profile.configuration,
            "assets": {name: str(path) for name, path in profile.assets.items()},
            "state": "created",
            "artifact_directory": str(artifact_dir),
        }
        self._atomic_json(manifest, value)
        return SessionRecord(session_id, instance_id, runtime_dir, state_dir, artifact_dir, manifest)

    def open(self, instance_id: str, session_id: str) -> SessionRecord:
        """Reopen an existing session after validating its owned paths and manifest."""
        instance_id = _validate_id(instance_id, "instance_id")
        session_id = _validate_id(session_id, "session_id")
        runtime_dir = self.runtime_root / "sessions" / session_id
        state_dir = self.state_root / "instances" / instance_id
        artifact_dir = self.artifact_root / "sessions" / session_id
        manifest = runtime_dir / "manifest.json"
        if not manifest.is_file() or manifest.is_symlink():
            raise RuntimeStateError(f"session manifest is unavailable: {manifest}")
        try:
            value = json.loads(manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise RuntimeStateError(f"cannot read session manifest {manifest}: {exc}") from exc
        if not isinstance(value, dict) or value.get("schema_version") != 1:
            raise RuntimeStateError("session manifest schema_version must be 1")
        if value.get("session_id") != session_id or value.get("instance_id") != instance_id:
            raise RuntimeStateError("session manifest identity does not match requested session")
        if not runtime_dir.is_dir() or not state_dir.is_dir() or not artifact_dir.is_dir():
            raise RuntimeStateError("session directory layout is incomplete")
        return SessionRecord(session_id, instance_id, runtime_dir, state_dir, artifact_dir, manifest)

    def list_records(self) -> list[SessionRecord]:
        """Return complete, owned session records without trusting directory names."""
        root = self.runtime_root / "sessions"
        if not root.is_dir() or root.is_symlink():
            return []
        records: list[SessionRecord] = []
        try:
            candidates = sorted(root.iterdir(), key=lambda path: path.name)
        except OSError:
            return []
        for runtime_dir in candidates:
            if runtime_dir.is_symlink() or not runtime_dir.is_dir():
                continue
            try:
                session_id = _validate_id(runtime_dir.name, "session_id")
                manifest = runtime_dir / "manifest.json"
                if manifest.is_symlink() or not manifest.is_file():
                    continue
                value = json.loads(manifest.read_text(encoding="utf-8"))
                instance_id = _validate_id(value.get("instance_id"), "instance_id")
                records.append(self.open(instance_id, session_id))
            except (OSError, json.JSONDecodeError, RuntimeStateError):
                # A stale or partial session must not make the directory unusable.
                continue
        return records

    def update(self, record: SessionRecord, state: str, *, pid: int | None = None,
               exit_code: int | None = None, metadata: dict[str, Any] | None = None) -> None:
        """Persist a controlled lifecycle transition in the session manifest."""
        if state not in {"created", "running", "stopping", "stopped", "failed"}:
            raise RuntimeStateError(f"unknown session state: {state}")
        try:
            value = json.loads(record.manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise RuntimeStateError(f"cannot read session manifest {record.manifest}: {exc}") from exc
        value["state"] = state
        if pid is not None:
            value["pid"] = pid
        if exit_code is not None:
            value["exit_code"] = exit_code
        if metadata:
            value.update(metadata)
        self._atomic_json(record.manifest, value)

    @staticmethod
    def _atomic_json(path: Path, value: dict[str, Any]) -> None:
        try:
            with tempfile.NamedTemporaryFile(
                mode="w", encoding="utf-8", dir=path.parent,
                prefix=f".{path.name}.", delete=False
            ) as stream:
                json.dump(value, stream, indent=2, sort_keys=True)
                stream.write("\n")
                temporary = Path(stream.name)
            temporary.replace(path)
        except OSError as exc:
            raise RuntimeStateError(f"cannot write session manifest {path}: {exc}") from exc
