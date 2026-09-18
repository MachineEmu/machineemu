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
            "state": "created",
            "artifact_directory": str(artifact_dir),
        }
        self._atomic_json(manifest, value)
        return SessionRecord(session_id, instance_id, runtime_dir, state_dir, artifact_dir, manifest)

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
