"""Small asynchronous QMP client used by the session supervisor."""

from __future__ import annotations

import asyncio
from collections import deque
import json
from pathlib import Path


class QMPError(RuntimeError):
    """Raised when QMP cannot negotiate or execute a command."""


class QMPClient:
    def __init__(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        self.reader = reader
        self.writer = writer
        self._id = 0
        self._lock = asyncio.Lock()
        self._events: deque[dict] = deque()

    @classmethod
    async def connect(cls, path: Path, timeout: float = 10.0) -> "QMPClient":
        deadline = asyncio.get_running_loop().time() + timeout
        while True:
            try:
                reader, writer = await asyncio.open_unix_connection(path)
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if asyncio.get_running_loop().time() >= deadline:
                    raise QMPError(f"QMP socket did not become available: {path}")
                await asyncio.sleep(0.05)
        client = cls(reader, writer)
        greeting = await client._read(timeout)
        if "QMP" not in greeting:
            await client.close()
            raise QMPError("QMP greeting is missing the QMP field")
        await client.execute("qmp_capabilities", timeout=timeout)
        return client

    async def execute(self, command: str, timeout: float = 10.0, **arguments):
        if not command or not isinstance(command, str):
            raise QMPError("QMP command must be a non-empty string")
        async with self._lock:
            self._id += 1
            request = {"execute": command, "id": self._id}
            if arguments:
                request["arguments"] = arguments
            self.writer.write((json.dumps(request) + "\r\n").encode())
            await self.writer.drain()
            while True:
                reply = await self._read(timeout)
                if "event" in reply:
                    self._events.append(reply)
                    continue
                if reply.get("id") != self._id:
                    continue
                if "error" in reply:
                    raise QMPError(str(reply["error"]))
                return reply.get("return")

    async def wait_event(self, name: str, timeout: float = 10.0) -> dict:
        async with self._lock:
            for event in tuple(self._events):
                if event.get("event") == name:
                    self._events.remove(event)
                    return event
            deadline = asyncio.get_running_loop().time() + timeout
            while True:
                remaining = deadline - asyncio.get_running_loop().time()
                if remaining <= 0:
                    raise QMPError(f"timed out waiting for QMP event {name}")
                reply = await self._read(remaining)
                if reply.get("event") == name:
                    return reply
                if "event" in reply:
                    self._events.append(reply)

    async def close(self) -> None:
        self.writer.close()
        await self.writer.wait_closed()

    async def _read(self, timeout: float) -> dict:
        try:
            line = await asyncio.wait_for(self.reader.readline(), timeout)
        except asyncio.TimeoutError as exc:
            raise QMPError("QMP response timed out") from exc
        if not line:
            raise QMPError("QMP connection closed")
        if len(line) > 1024 * 1024:
            raise QMPError("QMP response exceeds the maximum frame size")
        try:
            value = json.loads(line)
        except json.JSONDecodeError as exc:
            raise QMPError("QMP returned malformed JSON") from exc
        if not isinstance(value, dict):
            raise QMPError("QMP response must be an object")
        return value
