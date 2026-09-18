"""Coordinate process ownership and QMP attachment for one session."""

from __future__ import annotations

from dataclasses import dataclass
import asyncio
import json
import os
import signal
import time
from pathlib import Path
from typing import Mapping, Sequence

from .process import ManagedProcess, ProcessSupervisor
from .qmp import QMPClient
from .state import RuntimeStateError, SessionRecord, SessionStore


@dataclass
class RunningSession:
    process: ManagedProcess
    qmp: QMPClient

    async def stop(self, timeout: float = 5.0) -> int:
        await self.qmp.close()
        return self.process.stop(timeout)


class SessionSupervisor:
    """Start a process and require a working QMP control channel."""

    def __init__(self, store: SessionStore):
        self.store = store
        self.processes = ProcessSupervisor(store)

    async def start(self, record: SessionRecord, command: Sequence[str], qmp_socket: Path,
                    environment: Mapping[str, str] | None = None,
                    qmp_timeout: float = 10.0) -> RunningSession:
        process = self.processes.start(record, command, environment)
        self.store.update(record, "running", pid=process.pid,
                          metadata={"qmp_socket": str(qmp_socket)})
        try:
            qmp = await QMPClient.connect(qmp_socket, timeout=qmp_timeout)
        except Exception as exc:
            process.stop(timeout=1.0)
            self.store.update(record, "failed", metadata={"failure": f"qmp: {exc}"})
            raise RuntimeStateError(f"QMP attachment failed: {exc}") from exc
        return RunningSession(process, qmp)

    def recover(self, record: SessionRecord) -> str:
        """Reconcile a manifest after supervisor restart using its recorded PID."""
        try:
            manifest = json.loads(record.manifest.read_text(encoding="utf-8"))
            pid = manifest.get("pid")
        except (OSError, json.JSONDecodeError) as exc:
            raise RuntimeStateError(f"cannot read session manifest: {exc}") from exc
        if not isinstance(pid, int) or pid <= 0:
            self.store.update(record, "failed", metadata={"failure": "missing process PID"})
            return "failed"
        try:
            os.kill(pid, 0)
        except (ProcessLookupError, PermissionError):
            self.store.update(record, "failed", metadata={"failure": "process is not alive"})
            return "failed"
        return "running"

    def stop_recovered(self, record: SessionRecord, timeout: float = 5.0) -> int:
        """Stop a session after a supervisor restart using its recorded PID."""
        if timeout < 0:
            raise RuntimeStateError("stop timeout must be non-negative")
        try:
            manifest = json.loads(record.manifest.read_text(encoding="utf-8"))
            pid = manifest.get("pid")
            state = manifest.get("state")
        except (OSError, json.JSONDecodeError) as exc:
            raise RuntimeStateError(f"cannot read session manifest: {exc}") from exc
        if state != "running":
            raise RuntimeStateError(f"session is not running: {state}")
        if not isinstance(pid, int) or pid <= 0:
            raise RuntimeStateError("session manifest has no valid process PID")
        self.store.update(record, "stopping", pid=pid)
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            self.store.update(record, "stopped", exit_code=0)
            return 0
        except PermissionError as exc:
            self.store.update(record, "failed", metadata={"failure": f"stop: {exc}"})
            raise RuntimeStateError(f"cannot stop process {pid}: {exc}") from exc
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                self.store.update(record, "stopped", exit_code=0)
                return 0
            time.sleep(0.05)
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        self.store.update(record, "stopped", exit_code=-signal.SIGKILL)
        return -signal.SIGKILL
