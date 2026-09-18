"""Durable instance identity separate from ephemeral execution sessions."""

from __future__ import annotations

from dataclasses import dataclass
import json
import hashlib
from pathlib import Path
import re
import shutil
import tempfile

from machineemu.profiles import ResolvedProfile

from .state import RuntimeStateError

_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")


@dataclass(frozen=True)
class InstanceRecord:
    instance_id: str
    state_dir: Path
    manifest: Path


class InstanceStore:
    def __init__(self, root: Path):
        self.root = root.resolve()

    def ensure(self, instance_id: str, profile: ResolvedProfile) -> InstanceRecord:
        if not isinstance(instance_id, str) or not _ID.fullmatch(instance_id):
            raise RuntimeStateError("instance_id must be an opaque runtime identifier")
        state_dir = self.root / "instances" / instance_id
        manifest = state_dir / "instance.json"
        state_dir.mkdir(parents=True, exist_ok=True)
        if manifest.exists():
            try:
                value = json.loads(manifest.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError) as exc:
                raise RuntimeStateError(f"cannot read instance manifest {manifest}: {exc}") from exc
            if value.get("instance_id") != instance_id:
                raise RuntimeStateError("instance manifest identity does not match requested instance")
            return InstanceRecord(instance_id, state_dir, manifest)
        value = {
            "schema_version": 1,
            "instance_id": instance_id,
            "profile_id": profile.profile_id,
            "machine": profile.machine,
            "engine": {
                "track_id": profile.engine.track_id,
                "build_digest": profile.engine.build_digest,
            },
            "state": "created",
        }
        try:
            with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=state_dir, delete=False) as stream:
                json.dump(value, stream, indent=2, sort_keys=True)
                stream.write("\n")
                temporary = Path(stream.name)
            temporary.replace(manifest)
        except OSError as exc:
            raise RuntimeStateError(f"cannot write instance manifest {manifest}: {exc}") from exc
        return InstanceRecord(instance_id, state_dir, manifest)

    def import_state_file(self, record: InstanceRecord, source: Path, name: str,
                          expected: str | None = None) -> tuple[str, Path]:
        """Copy one mutable state file atomically and record its digest."""
        if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", name):
            raise RuntimeStateError("state file name is not safe")
        if not source.is_file() or source.is_symlink():
            raise RuntimeStateError("state source must be a regular file")
        hasher = hashlib.sha256()
        with source.open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                hasher.update(block)
        digest = f"sha256:{hasher.hexdigest()}"
        if expected is not None and expected != digest:
            raise RuntimeStateError(f"state digest mismatch: expected {expected}, got {digest}")
        destination = record.state_dir / name
        if destination.exists():
            raise RuntimeStateError(f"state file already exists: {name}")
        try:
            with tempfile.NamedTemporaryFile(dir=record.state_dir, prefix=f".{name}.", delete=False) as temporary:
                temporary_path = Path(temporary.name)
                with source.open("rb") as stream:
                    shutil.copyfileobj(stream, temporary)
                temporary.flush()
            temporary_path.replace(destination)
            value = json.loads(record.manifest.read_text(encoding="utf-8"))
            files = value.setdefault("state_files", {})
            files[name] = {"sha256": digest, "size": destination.stat().st_size}
            self._atomic_json(record.manifest, value)
        except (OSError, json.JSONDecodeError) as exc:
            if destination.exists():
                destination.unlink(missing_ok=True)
            raise RuntimeStateError(f"cannot publish state file {name}: {exc}") from exc
        return digest, destination

    @staticmethod
    def _atomic_json(path: Path, value: dict) -> None:
        try:
            with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as stream:
                json.dump(value, stream, indent=2, sort_keys=True)
                stream.write("\n")
                temporary = Path(stream.name)
            temporary.replace(path)
        except OSError as exc:
            raise RuntimeStateError(f"cannot write instance manifest {path}: {exc}") from exc
