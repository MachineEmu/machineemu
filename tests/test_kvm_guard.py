import json
import os

from machineemu.domains.analysis.kvm_guard import load_command, parse_stats, session_from_record, snapshot, status


def _session(tmp_path):
    runtime = tmp_path / "runtime"
    artifacts = tmp_path / "artifacts"
    runtime.mkdir()
    artifacts.mkdir()
    manifest = runtime / "manifest.json"
    manifest.write_text(json.dumps({
        "schema_version": 1, "analysis": {"profile": "malware-analysis"},
        "configuration": {"cpu": {"model": "hv_fast"}}, "qemu": {"tgid": os.getpid()},
    }), encoding="utf-8")
    return session_from_record("session-1", runtime, artifacts, manifest)


def test_kvm_guard_load_status_and_snapshot_are_session_scoped(tmp_path):
    session = _session(tmp_path)
    command = load_command(session, tmp_path / "guard.ko")
    assert command["qemu_tgid"] == os.getpid()
    assert command["hyperv_fast_mode"] is True
    stats = tmp_path / "stats"
    stats.write_text("target_tgid: %d\nlab_enable: 1\nexit_probe_hits: 2\nexit_timing_observed: 2\n" % os.getpid())
    assert parse_stats(stats.read_text())["exit_probe_hits"] == 2
    assert status(session, stats)["ok"] is True
    result = snapshot(session, stats)
    assert json.loads((tmp_path / "artifacts" / "kvm-guard-stats.latest.json").read_text())["qemu_tgid"] == os.getpid()
    assert result["path"].endswith(".json")
