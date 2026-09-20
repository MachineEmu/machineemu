"""Bounded subprocess ownership for a MachineEmu session."""

from __future__ import annotations

from dataclasses import dataclass
import json
import os
import subprocess
from typing import Mapping, Sequence

from .state import RuntimeStateError, SessionRecord, SessionStore


@dataclass
class ManagedProcess:
    record: SessionRecord
    process: subprocess.Popen[bytes]
    store: SessionStore

    @property
    def pid(self) -> int:
        return self.process.pid

    def poll(self) -> int | None:
        code = self.process.poll()
        if code is not None:
            self.store.update(self.record, "stopped" if code == 0 else "failed", exit_code=code)
        return code

    def stop(self, timeout: float = 5.0) -> int:
        """Terminate the child, escalating to kill after the bounded timeout."""
        if self.process.poll() is not None:
            return self.process.returncode
        self.store.update(self.record, "stopping", pid=self.pid)
        self.process.terminate()
        try:
            code = self.process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.process.kill()
            code = self.process.wait(timeout=timeout)
        self.store.update(self.record, "stopped" if code == 0 else "failed", exit_code=code)
        return code


class ProcessSupervisor:
    """Own child processes and never inherit the caller's full environment."""

    def __init__(self, store: SessionStore):
        self.store = store

    def start(self, record: SessionRecord, command: Sequence[str],
              environment: Mapping[str, str] | None = None) -> ManagedProcess:
        if not command or any(not isinstance(arg, str) or not arg for arg in command):
            raise RuntimeStateError("process command must contain non-empty strings")
        try:
            state = json.loads(record.manifest.read_text(encoding="utf-8")).get("state")
        except (OSError, json.JSONDecodeError) as exc:
            raise RuntimeStateError(f"cannot read session manifest: {exc}") from exc
        if state not in {"created", "stopped", "failed"}:
            raise RuntimeStateError(f"session is not restartable from state: {state}")
        env = {"PATH": os.environ.get("PATH", "")}
        if environment is not None:
            env.update(environment)
        stdout = (record.runtime_dir / "logs/stdout.log").open("ab")
        stderr = (record.runtime_dir / "logs/stderr.log").open("ab")
        try:
            process = subprocess.Popen(
                list(command), cwd=record.runtime_dir, env=env,
                stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
                start_new_session=True,
            )
        except OSError:
            stdout.close()
            stderr.close()
            raise
        stdout.close()
        stderr.close()
        self.store.update(record, "running", pid=process.pid)
        return ManagedProcess(record, process, self.store)
