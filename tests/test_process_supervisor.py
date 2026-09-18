import json
import sys
import time

from machineemu.engines import EngineRegistry
from machineemu.profiles import resolve_profile
from machineemu.runtime import ProcessSupervisor, SessionStore


def _record(tmp_path):
    bundle = tmp_path / "bundles" / "track"
    (bundle / "bin").mkdir(parents=True)
    (bundle / "bin/qemu").write_bytes(b"qemu")
    digest = "f" * 64
    (bundle / "engine-build.json").write_text(json.dumps({
        "schema_version": 1, "track_id": "track", "build_digest": digest,
        "source_revision": "commit", "targets": ["aarch64-softmmu"],
        "executables": {"aarch64-softmmu": "bin/qemu"}, "dirty_source": False,
    }), encoding="utf-8")
    release = tmp_path / "release.json"
    release.write_text(json.dumps({"schema_version": 1, "engines": {
        "track": {"manifest": "track/engine-build.json", "build_digest": digest}
    }}), encoding="utf-8")
    profile = tmp_path / "profile.json"
    profile.write_text(json.dumps({"schema_version": 1, "id": "debian",
        "engine": {"track": "track"}, "machine": "virt"}), encoding="utf-8")
    resolved = resolve_profile(profile, EngineRegistry.load(release, tmp_path / "bundles"), target="aarch64-softmmu")
    store = SessionStore(tmp_path / "run", tmp_path / "state", tmp_path / "artifacts")
    return store, store.create("instance", "session", resolved)


def test_supervisor_records_pid_and_bounded_stop(tmp_path):
    store, record = _record(tmp_path)
    child = ProcessSupervisor(store).start(record, [sys.executable, "-c", "import time; time.sleep(30)"])
    manifest = json.loads(record.manifest.read_text())
    assert manifest["state"] == "running" and manifest["pid"] == child.pid
    child.stop(timeout=1)
    manifest = json.loads(record.manifest.read_text())
    assert manifest["state"] in {"stopped", "failed"}
    assert child.poll() is not None


def test_supervisor_uses_explicit_environment(tmp_path):
    store, record = _record(tmp_path)
    marker = record.runtime_dir / "logs/env.txt"
    child = ProcessSupervisor(store).start(
        record,
        [sys.executable, "-c", f"open({str(marker)!r}, 'w').write(__import__('os').environ.get('ME_TEST', 'missing'))"],
        {"ME_TEST": "present"},
    )
    assert child.process.wait(timeout=3) == 0
    child.poll()
    assert marker.read_text() == "present"
