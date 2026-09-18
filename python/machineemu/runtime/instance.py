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

    def record_backing_chain(self, record: InstanceRecord, name: str,
                             backing_chain: list[str]) -> None:
        """Record a validated backing chain using only imported state files."""
        if not self._safe_name(name) or not isinstance(backing_chain, list):
            raise RuntimeStateError("state backing-chain input is invalid")
        if name in backing_chain or len(set(backing_chain)) != len(backing_chain):
            raise RuntimeStateError("state backing chain contains a cycle or duplicate")
        try:
            value = json.loads(record.manifest.read_text(encoding="utf-8"))
            files = value.get("state_files", {})
            if name not in files:
                raise RuntimeStateError(f"state file is not imported: {name}")
            for parent in backing_chain:
                if not self._safe_name(parent) or parent not in files:
                    raise RuntimeStateError(f"backing state file is not imported: {parent}")
            files[name]["backing_chain"] = list(backing_chain)
            self._atomic_json(record.manifest, value)
        except (OSError, json.JSONDecodeError) as exc:
            raise RuntimeStateError(f"cannot read instance manifest {record.manifest}: {exc}") from exc

    def snapshot(self, record: InstanceRecord, snapshot_id: str,
                 files: list[str] | None = None) -> Path:
        """Capture imported state files into an immutable, atomically published snapshot."""
        if not _ID.fullmatch(snapshot_id):
            raise RuntimeStateError("snapshot_id must be an opaque runtime identifier")
        try:
            value = json.loads(record.manifest.read_text(encoding="utf-8"))
            state_files = value.get("state_files", {})
        except (OSError, json.JSONDecodeError) as exc:
            raise RuntimeStateError(f"cannot read instance manifest {record.manifest}: {exc}") from exc
        selected = list(state_files) if files is None else files
        if not selected or len(set(selected)) != len(selected):
            raise RuntimeStateError("snapshot file set must be non-empty and unique")
        for name in selected:
            if not self._safe_name(name) or name not in state_files:
                raise RuntimeStateError(f"snapshot state file is not imported: {name}")
            if not (record.state_dir / name).is_file():
                raise RuntimeStateError(f"snapshot state file is unavailable: {name}")
        snapshots = record.state_dir / "snapshots"
        destination = snapshots / snapshot_id
        if destination.exists():
            raise RuntimeStateError(f"snapshot already exists: {snapshot_id}")
        snapshots.mkdir(exist_ok=True)
        staging = Path(tempfile.mkdtemp(prefix=f".{snapshot_id}.", dir=snapshots))
        snapshot_files = {}
        try:
            for name in selected:
                source = record.state_dir / name
                target = staging / name
                shutil.copy2(source, target)
                snapshot_files[name] = state_files[name]
            (staging / "snapshot.json").write_text(json.dumps({
                "schema_version": 1, "snapshot_id": snapshot_id,
                "instance_id": record.instance_id, "files": snapshot_files,
            }, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            staging.replace(destination)
        except RuntimeStateError:
            shutil.rmtree(staging, ignore_errors=True)
            raise
        except (OSError, TypeError) as exc:
            shutil.rmtree(staging, ignore_errors=True)
            raise RuntimeStateError(f"cannot publish snapshot {snapshot_id}: {exc}") from exc
        return destination

    def stage_snapshot_restore(self, record: InstanceRecord, snapshot_id: str) -> Path:
        """Verify a snapshot and stage a complete restore set without touching live state."""
        if not _ID.fullmatch(snapshot_id):
            raise RuntimeStateError("snapshot_id must be an opaque runtime identifier")
        snapshot = record.state_dir / "snapshots" / snapshot_id
        manifest = snapshot / "snapshot.json"
        try:
            value = json.loads(manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise RuntimeStateError(f"cannot read snapshot manifest {manifest}: {exc}") from exc
        if value.get("instance_id") != record.instance_id or value.get("snapshot_id") != snapshot_id:
            raise RuntimeStateError("snapshot identity does not match requested instance")
        files = value.get("files")
        if not isinstance(files, dict) or not files:
            raise RuntimeStateError("snapshot contains no state files")
        restore_root = record.state_dir / "restore-staging"
        restore_root.mkdir(exist_ok=True)
        destination = restore_root / snapshot_id
        if destination.exists():
            raise RuntimeStateError(f"restore staging already exists: {snapshot_id}")
        staging = Path(tempfile.mkdtemp(prefix=f".{snapshot_id}.", dir=restore_root))
        try:
            for name, metadata in files.items():
                if not self._safe_name(name) or not isinstance(metadata, dict):
                    raise RuntimeStateError("snapshot contains an invalid state file")
                source = snapshot / name
                if not source.is_file() or source.is_symlink():
                    raise RuntimeStateError(f"snapshot state file is unavailable: {name}")
                digest = hashlib.sha256(source.read_bytes()).hexdigest()
                if metadata.get("sha256") != f"sha256:{digest}":
                    raise RuntimeStateError(f"snapshot digest mismatch: {name}")
                shutil.copy2(source, staging / name)
            (staging / "restore.json").write_text(json.dumps({
                "schema_version": 1, "snapshot_id": snapshot_id,
                "instance_id": record.instance_id, "state": "staged",
            }, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            staging.replace(destination)
        except RuntimeStateError:
            shutil.rmtree(staging, ignore_errors=True)
            raise
        except (OSError, TypeError) as exc:
            shutil.rmtree(staging, ignore_errors=True)
            raise RuntimeStateError(f"cannot stage snapshot restore {snapshot_id}: {exc}") from exc
        return destination

    def apply_staged_restore(self, record: InstanceRecord, snapshot_id: str,
                             *, instance_state: str) -> None:
        """Apply a staged restore only after the caller proves the instance is stopped."""
        if instance_state != "stopped":
            raise RuntimeStateError("snapshot restore requires a stopped instance")
        staged = record.state_dir / "restore-staging" / snapshot_id
        restore_manifest = staged / "restore.json"
        snapshot_manifest = record.state_dir / "snapshots" / snapshot_id / "snapshot.json"
        try:
            restore = json.loads(restore_manifest.read_text(encoding="utf-8"))
            snapshot = json.loads(snapshot_manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise RuntimeStateError(f"cannot read staged restore {snapshot_id}: {exc}") from exc
        if restore.get("instance_id") != record.instance_id or restore.get("state") != "staged":
            raise RuntimeStateError("staged restore identity or state is invalid")
        files = snapshot.get("files")
        if not isinstance(files, dict) or not files:
            raise RuntimeStateError("snapshot contains no state files")
        backup = Path(tempfile.mkdtemp(prefix=f".restore-backup-{snapshot_id}.", dir=record.state_dir))
        replaced: list[str] = []
        try:
            for name in files:
                source = staged / name
                if not self._safe_name(name) or not source.is_file() or source.is_symlink():
                    raise RuntimeStateError(f"staged state file is unavailable: {name}")
                live = record.state_dir / name
                if live.exists():
                    shutil.copy2(live, backup / name)
                shutil.copy2(source, live)
                replaced.append(name)
            value = json.loads(record.manifest.read_text(encoding="utf-8"))
            value["state"] = "stopped"
            value["last_restore"] = {"snapshot_id": snapshot_id, "state": "applied"}
            self._atomic_json(record.manifest, value)
        except (OSError, json.JSONDecodeError, RuntimeStateError) as exc:
            for name in replaced:
                saved = backup / name
                live = record.state_dir / name
                if saved.is_file():
                    shutil.copy2(saved, live)
                else:
                    live.unlink(missing_ok=True)
            if isinstance(exc, RuntimeStateError):
                raise
            raise RuntimeStateError(f"cannot apply staged restore {snapshot_id}: {exc}") from exc
        finally:
            shutil.rmtree(backup, ignore_errors=True)

    @staticmethod
    def _safe_name(name: object) -> bool:
        return isinstance(name, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", name) is not None

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
