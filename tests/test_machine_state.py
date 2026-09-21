import json
import os
import socket
import subprocess
import stat
import textwrap
from pathlib import Path

import pytest

from machineemu.runtime.state import RuntimeStateError
from machineemu.runtime.machine_state import seed_disk_overlay, seed_nvram, start_tpm


def _stub_binary(directory, name, body):
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / name
    path.write_text("#!/usr/bin/env python3\n" + textwrap.dedent(body), encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


@pytest.fixture
def stub_swtpm(tmp_path, monkeypatch):
    """Stand in for swtpm/swtpm_setup: bind the control socket and idle."""
    binaries = tmp_path / "bin"
    _stub_binary(binaries, "swtpm_setup", """
        import sys
        state = sys.argv[sys.argv.index("--tpm-state") + 1]
        with open(state + "/tpm2-00.permall", "wb") as handle:
            handle.write(b"\\0" * 4096)
    """)
    _stub_binary(binaries, "swtpm", """
        import socket, sys, time
        control = [arg for arg in sys.argv if arg.startswith("type=unixio,")][0]
        path = control.split("path=", 1)[1].split(",", 1)[0]
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(path)
        server.listen(1)
        time.sleep(30)
    """)
    monkeypatch.setenv("PATH", str(binaries), prepend=os.pathsep)
    return binaries


def test_nvram_is_copied_out_of_the_asset_store_on_first_use(tmp_path):
    asset = tmp_path / "store" / "vars"
    asset.parent.mkdir()
    asset.write_bytes(b"pristine")
    target = tmp_path / "instances" / "one" / "OVMF_VARS.fd"

    assert seed_nvram({"nvram": {"path": str(target), "seed": str(asset)}}) == target
    assert target.read_bytes() == b"pristine"

    # A boot writes to the copy; the imported asset must be untouched.
    target.write_bytes(b"enrolled keys")
    assert seed_nvram({"nvram": {"path": str(target), "seed": str(asset)}}) == target
    assert target.read_bytes() == b"enrolled keys"
    assert asset.read_bytes() == b"pristine"


def test_nvram_seeding_is_skipped_when_a_profile_declares_no_firmware():
    assert seed_nvram(None) is None


def test_nvram_refuses_a_seed_asset_that_is_not_present(tmp_path):
    target = tmp_path / "instances" / "one" / "OVMF_VARS.fd"
    with pytest.raises(RuntimeStateError, match="seed asset is unavailable"):
        seed_nvram({"nvram": {"path": str(target), "seed": str(tmp_path / "missing")}})


def test_swtpm_manufactures_state_once_and_reuses_it(tmp_path, stub_swtpm):
    state = tmp_path / "instances" / "one" / "tpm"
    logs = tmp_path / "logs"
    spec = {"socket": str(tmp_path / "tpm.sock"), "state": str(state), "version": "2.0"}

    running = start_tpm(spec, logs)
    try:
        assert (state / "tpm2-00.permall").stat().st_size >= 3000
        assert (tmp_path / "tpm.sock").exists()
        assert running.pid > 0
    finally:
        running.stop(timeout=2)
        (tmp_path / "tpm.sock").unlink()

    # Manufactured state is preserved rather than rebuilt on the next session.
    (state / "tpm2-00.permall").write_bytes(b"\0" * 8192)
    running = start_tpm(spec, logs)
    try:
        assert (state / "tpm2-00.permall").stat().st_size == 8192
    finally:
        running.stop(timeout=2)


def test_swtpm_start_fails_clearly_when_the_emulator_is_missing(tmp_path, monkeypatch):
    monkeypatch.setenv("PATH", str(tmp_path / "empty"))
    spec = {"socket": str(tmp_path / "tpm.sock"), "state": str(tmp_path / "tpm"), "version": "2.0"}
    with pytest.raises(RuntimeStateError, match="swtpm is not available"):
        start_tpm(spec, tmp_path / "logs")


def test_swtpm_refuses_to_reuse_a_socket_another_session_still_holds(tmp_path, stub_swtpm):
    taken = tmp_path / "tpm.sock"
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(taken))
    try:
        spec = {"socket": str(taken), "state": str(tmp_path / "tpm"), "version": "2.0"}
        with pytest.raises(RuntimeStateError, match="already present"):
            start_tpm(spec, tmp_path / "logs")
    finally:
        server.close()


def _baseline(tmp_path, name="baseline.qcow2"):
    path = tmp_path / "store" / name
    path.parent.mkdir(exist_ok=True)
    subprocess.run(["qemu-img", "create", "-f", "qcow2", str(path), "64M"],
                   check=True, capture_output=True)
    return path


def test_disk_overlay_is_created_over_the_baseline_and_reused(tmp_path):
    baseline = _baseline(tmp_path)
    target = tmp_path / "instances" / "one" / "overlay.qcow2"
    storage = {"disk": {"path": str(target), "backing": str(baseline), "backing_format": "qcow2"}}

    assert seed_disk_overlay(storage) == target
    info = json.loads(subprocess.run(["qemu-img", "info", "--output=json", str(target)],
                                     check=True, capture_output=True, text=True).stdout)
    assert Path(info["full-backing-filename"]) == baseline

    # A second session keeps what the guest already wrote.
    before = target.stat().st_mtime_ns
    assert seed_disk_overlay(storage) == target
    assert target.stat().st_mtime_ns == before


def test_guest_writes_never_reach_the_shared_baseline(tmp_path):
    baseline = _baseline(tmp_path)
    pristine = baseline.read_bytes()
    target = tmp_path / "instances" / "one" / "overlay.qcow2"
    seed_disk_overlay({"disk": {"path": str(target), "backing": str(baseline),
                                "backing_format": "qcow2"}})

    subprocess.run(["qemu-img", "resize", str(target), "128M"], check=True, capture_output=True)
    assert baseline.read_bytes() == pristine


def test_disk_overlay_refuses_to_boot_on_a_changed_baseline(tmp_path):
    baseline = _baseline(tmp_path)
    target = tmp_path / "instances" / "one" / "overlay.qcow2"
    seed_disk_overlay({"disk": {"path": str(target), "backing": str(baseline),
                                "backing_format": "qcow2"}})

    replacement = _baseline(tmp_path, "other.qcow2")
    with pytest.raises(RuntimeStateError, match="baseline asset changed"):
        seed_disk_overlay({"disk": {"path": str(target), "backing": str(replacement),
                                    "backing_format": "qcow2"}})


def test_disk_overlay_refuses_a_baseline_that_is_not_present(tmp_path):
    with pytest.raises(RuntimeStateError, match="baseline is unavailable"):
        seed_disk_overlay({"disk": {"path": str(tmp_path / "overlay.qcow2"),
                                    "backing": str(tmp_path / "missing.qcow2"),
                                    "backing_format": "qcow2"}})
