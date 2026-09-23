#!/usr/bin/env python3
"""Exercise real QEMU ownership across daemon restarts in a private workspace.

Build first with: cargo build -p machineemu --bins --locked
Run with: python3 scripts/test_daemon_recovery.py
Requires qemu-system-x86_64; uses TCG and no guest disks or network.
"""
import argparse
import http.client
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=15)
        self.path = str(path)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def alive(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
        return fields[0] not in ("Z", "X")
    except FileNotFoundError:
        return False


def exercise(binary, qemu):
    with tempfile.TemporaryDirectory(prefix="me-recovery-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        endpoint = root / "api.sock"
        config = root / "config.yaml"
        config.write_text("{}\n")
        daemon = None
        children = set()

        def request(method, path, body=None):
            client = UnixHTTP(endpoint)
            try:
                client.request(method, "/api/v2/" + path,
                               None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
                response = client.getresponse()
                data = response.read()
                assert response.status < 300, (response.status, data.decode())
                return json.loads(data) if data else None
            finally:
                client.close()

        def start_daemon():
            nonlocal daemon
            daemon = subprocess.Popen(
                [str(binary), "--config", str(config), "--workspace", str(workspace),
                 "--unix-socket", str(endpoint)], stdout=log, stderr=log)
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                if daemon.poll() is not None:
                    raise AssertionError("daemon exited: " + (root / "daemon.log").read_text())
                try:
                    request("GET", "health")
                    return
                except (OSError, http.client.HTTPException):
                    time.sleep(0.05)
            raise AssertionError("daemon did not become ready")

        def stop_daemon(crash=False):
            nonlocal daemon
            daemon.send_signal(signal.SIGKILL if crash else signal.SIGTERM)
            daemon.wait(timeout=15)
            daemon = None

        with (root / "daemon.log").open("w") as log:
            try:
                start_daemon()
                request("POST", "images", {
                    "image_id": "fixture", "engine_track": "fixture", "target": "x86_64-softmmu",
                    "disk_sha256": "a" * 64, "firmware_sha256": None, "tpm_state_sha256": None})
                qmp = workspace / "instances/vm/qmp.sock"
                request("POST", "instances", {
                    "instance_id": "vm", "image_id": "fixture", "profile_id": "fixture",
                    "launch_plan": {
                        "argv": [qemu, "-machine", "q35,accel=tcg", "-m", "64",
                                 "-nodefaults", "-display", "none", "-nic", "none", "-S",
                                 "-qmp", f"unix:{qmp},server=on,wait=off"],
                        "qmp_socket": "instances/vm/qmp.sock", "stderr": "instances/vm/qemu.stderr",
                        "helpers": [{"name": "fixture", "argv": ["sleep", "120"], "after_qemu": False}]}})
                request("POST", "instances/vm/start", {})
                assert request("GET", "instances/vm")["state"] == "paused"
                with sqlite3.connect(workspace / "metadata.sqlite3") as db:
                    run_id, pid = db.execute("SELECT run_id, pid FROM runs WHERE status='running'").fetchone()
                    helper_pid = db.execute("SELECT pid FROM run_helpers WHERE run_id=?", (run_id,)).fetchone()[0]
                children.update((pid, helper_pid))
                # Give the event watcher time to acquire the shared QMP connection.
                time.sleep(0.4)
                request("POST", "instances/vm/resume", {})
                assert request("GET", "instances/vm")["state"] == "running"
                stop_daemon()
                assert alive(pid) and alive(helper_pid), "graceful daemon shutdown killed a VM or helper"
                # Reproduce persisted error state while the same VM is still alive.
                with sqlite3.connect(workspace / "metadata.sqlite3") as db:
                    db.execute("UPDATE instances SET lifecycle='error' WHERE instance_id='vm'")
                    db.execute("UPDATE runs SET status='uncertain' WHERE run_id=?", (run_id,))
                start_daemon()
                assert request("GET", "instances/vm")["state"] == "running"
                request("POST", "instances/vm/pause", {})
                assert request("GET", "instances/vm")["state"] == "paused"
                stop_daemon(crash=True)
                assert alive(pid) and alive(helper_pid), "daemon crash killed a VM or helper"
                start_daemon()
                assert request("GET", "instances/vm")["state"] == "paused"
                request("POST", "instances/vm/resume", {})
                request("POST", "instances/vm/stop", {})
                assert request("GET", "instances/vm")["state"] == "stopped"
                assert not alive(pid) and not alive(helper_pid), "stop leaked recovered processes"
                with sqlite3.connect(workspace / "metadata.sqlite3") as db:
                    assert db.execute("SELECT run_id, pid FROM runs").fetchall() == [(run_id, pid)]
                print("PASS: paused startup, shared QMP, graceful/crash restart, live error recovery, same VM PID, recovered helper cleanup")
            except BaseException:
                print((root / "daemon.log").read_text())
                raise
            finally:
                if daemon is not None and daemon.poll() is None:
                    daemon.terminate()
                    daemon.wait(timeout=15)
                for pid in children:
                    if alive(pid):
                        os.kill(pid, signal.SIGKILL)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--daemon", type=Path, default=Path("target/debug/machineemu-daemon"))
    args = parser.parse_args()
    qemu = shutil.which("qemu-system-x86_64")
    if qemu is None:
        parser.error("qemu-system-x86_64 is required")
    exercise(args.daemon.resolve(), qemu)
