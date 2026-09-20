"""A shared GDB/MI console attached to one session's QEMU guest stub.

QEMU exposes a single guest debug stub per session, so only one debugger can own
it at a time. This module runs that one debugger as a server-side process and
multiplexes it: every viewer of a session sees the same transcript, and any
viewer may type. Commands are echoed to everyone with the originating viewer, so
two people working on the same session can follow each other's steps.
"""
from __future__ import annotations

import asyncio
from collections import deque
from collections.abc import Callable
import os
import signal

DEFAULT_EXECUTABLE = "gdb-multiarch"
HISTORY_FRAMES = 500
IDLE_SECONDS = 300
MAX_COMMAND_LENGTH = 4096
MAX_LINE = 1024 * 1024
QUEUE_FRAMES = 256

_STREAM_KINDS = {"~": "console", "@": "target", "&": "log"}
_ASYNC_KINDS = {"*": "exec", "=": "notify", "+": "status"}
_ESCAPES = {"n": "\n", "t": "\t", "r": "\r", "f": "\f", "b": "\b", "a": "\a", "v": "\v"}


class GdbUnavailable(RuntimeError):
    """The shared console cannot serve a request because GDB is not running."""


def gdb_target(endpoint: object) -> str:
    """Return the `target remote` argument for a session's GDB endpoint."""
    if not isinstance(endpoint, dict) or not endpoint:
        raise ValueError("this session was not launched with the QEMU GDB stub enabled")
    if endpoint.get("transport") == "tcp":
        host, port = endpoint.get("host"), endpoint.get("port")
        if not isinstance(host, str) or not host or type(port) is not int:
            raise ValueError("session GDB TCP endpoint metadata is invalid")
        return f"{host}:{port}"
    if endpoint.get("transport") == "unix":
        path = endpoint.get("path")
        if not isinstance(path, str) or not path:
            raise ValueError("session GDB socket metadata is invalid")
        return path
    raise ValueError("session GDB transport is unsupported")


def _unescape(raw: str) -> str:
    """Decode an MI C-string body, including the octal escapes GDB emits for
    every non-ASCII byte, so UTF-8 text survives the round trip."""
    out = bytearray()
    index = 0
    while index < len(raw):
        char = raw[index]
        if char != "\\" or index + 1 >= len(raw):
            out += char.encode("utf-8")
            index += 1
            continue
        following = raw[index + 1]
        if following in "01234567":
            digits = ""
            while index + 1 < len(raw) and len(digits) < 3 and raw[index + 1] in "01234567":
                digits += raw[index + 1]
                index += 1
            out.append(int(digits, 8) & 0xFF)
            index += 1
            continue
        out += _ESCAPES.get(following, following).encode("utf-8")
        index += 2
    return out.decode("utf-8", errors="replace")


def _cstring(raw: str) -> str:
    raw = raw.strip()
    if len(raw) >= 2 and raw.startswith('"') and raw.endswith('"'):
        raw = raw[1:-1]
    return _unescape(raw)


def _field(payload: str, name: str) -> str | None:
    """Read one `name="..."` field out of an MI result payload."""
    marker = f'{name}="'
    start = payload.find(marker)
    if start < 0:
        return None
    index = start + len(marker)
    collected: list[str] = []
    while index < len(payload):
        char = payload[index]
        if char == "\\" and index + 1 < len(payload):
            collected.append(payload[index:index + 2])
            index += 2
            continue
        if char == '"':
            break
        collected.append(char)
        index += 1
    return _unescape("".join(collected))


def parse_mi(line: str) -> dict | None:
    """Translate one GDB/MI output line into a wire frame, or None to drop it."""
    line = line.rstrip("\r\n")
    # Real GDB writes the MI prompt with a trailing space; it carries no output.
    if not line.strip() or line.strip() == "(gdb)":
        return None
    index = 0
    while index < len(line) and line[index].isdigit():
        index += 1
    token, body = line[:index] or None, line[index:]
    if not body:
        return None
    marker, rest = body[0], body[1:]
    if marker in _STREAM_KINDS:
        return {"type": "gdb.output", "stream": _STREAM_KINDS[marker], "text": _cstring(rest)}
    if marker == "^":
        status, _, payload = rest.partition(",")
        return {"type": "gdb.result", "token": token, "status": status,
                "message": _field(payload, "msg"), "payload": payload or None}
    if marker in _ASYNC_KINDS:
        event, _, payload = rest.partition(",")
        return {"type": "gdb.event", "kind": _ASYNC_KINDS[marker], "event": event,
                "payload": payload or None}
    return {"type": "gdb.output", "stream": "raw", "text": body}


class GdbConsole:
    """One GDB process per session, fanned out to every connected viewer."""

    def __init__(self, session_id: str, target: str, *, executable: str = DEFAULT_EXECUTABLE,
                 history: int = HISTORY_FRAMES, idle_timeout: float = IDLE_SECONDS,
                 on_idle: Callable[[str], None] | None = None) -> None:
        self.session_id = session_id
        self.target = target
        self.executable = executable
        self.idle_timeout = idle_timeout
        self._on_idle = on_idle
        self._history: deque[dict] = deque(maxlen=history)
        self._subscribers: set[asyncio.Queue[dict]] = set()
        self._process: asyncio.subprocess.Process | None = None
        self._pump_task: asyncio.Task | None = None
        self._idle_task: asyncio.Task | None = None
        self._token = 0
        self._sequence = 0

    @property
    def running(self) -> bool:
        return self._process is not None and self._process.returncode is None

    @property
    def viewers(self) -> int:
        return len(self._subscribers)

    async def start(self) -> None:
        """Spawn GDB in MI mode and attach it to the session's guest stub."""
        self._process = await asyncio.create_subprocess_exec(
            self.executable, "--quiet", "--nx", "--interpreter=mi3",
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT, limit=MAX_LINE, start_new_session=True,
        )
        self._pump_task = asyncio.create_task(self._pump())
        try:
            await self.submit(f"target remote {self.target}", origin="console")
        except BaseException:
            await self.stop()
            raise

    async def stop(self) -> None:
        if self._idle_task:
            self._idle_task.cancel()
            self._idle_task = None
        process, self._process = self._process, None
        if process and process.stdin:
            process.stdin.close()
        if process and process.returncode is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except (OSError, ProcessLookupError):
                process.terminate()
            try:
                await asyncio.wait_for(process.wait(), timeout=5)
            except (asyncio.TimeoutError, ProcessLookupError):
                try:
                    process.kill()
                except (OSError, ProcessLookupError):
                    pass
        if self._pump_task:
            # Let the stdout reader consume EOF after process exit so the
            # subprocess pipe transport is closed cleanly. A broken child
            # transport must not keep API shutdown waiting indefinitely.
            try:
                await asyncio.wait_for(asyncio.shield(self._pump_task), timeout=1)
            except asyncio.TimeoutError:
                self._pump_task.cancel()
                await asyncio.gather(self._pump_task, return_exceptions=True)
            self._pump_task = None

    def subscribe(self) -> tuple[tuple[dict, ...], asyncio.Queue[dict]]:
        """Return the transcript so far plus a queue of every later frame."""
        if self._idle_task:
            self._idle_task.cancel()
            self._idle_task = None
        queue: asyncio.Queue[dict] = asyncio.Queue(maxsize=QUEUE_FRAMES)
        history = tuple(self._history)
        self._subscribers.add(queue)
        self.broadcast({"type": "gdb.viewers", "count": len(self._subscribers)}, record=False)
        return history, queue

    def unsubscribe(self, queue: asyncio.Queue[dict]) -> None:
        self._subscribers.discard(queue)
        self.broadcast({"type": "gdb.viewers", "count": len(self._subscribers)}, record=False)
        if not self._subscribers and self._idle_task is None:
            self._idle_task = asyncio.create_task(self._expire())

    async def _expire(self) -> None:
        # Debug state (breakpoints, the attached target) is expensive to
        # rebuild, so a reload or a short handover keeps the same GDB process.
        try:
            await asyncio.sleep(self.idle_timeout)
        except asyncio.CancelledError:
            return
        self._idle_task = None
        await self.stop()
        if self._on_idle:
            self._on_idle(self.session_id)

    def broadcast(self, frame: dict, *, record: bool = True) -> dict:
        self._sequence += 1
        frame = {"v": 1, **frame, "sequence": self._sequence}
        if record:
            self._history.append(frame)
        for queue in tuple(self._subscribers):
            if queue.full():
                try:
                    queue.get_nowait()
                except asyncio.QueueEmpty:
                    pass
            try:
                queue.put_nowait(frame)
            except asyncio.QueueFull:
                pass
        return frame

    async def submit(self, text: str, origin: str) -> str:
        """Run one command, echoing it to every viewer before GDB sees it."""
        if not self.running or self._process is None or self._process.stdin is None:
            raise GdbUnavailable("the shared GDB console is not running")
        text = text.strip()
        if not text or len(text) > MAX_COMMAND_LENGTH or "\n" in text or "\r" in text:
            raise ValueError("a GDB command must be one nonempty line within the size limit")
        self._token += 1
        token = str(self._token)
        self.broadcast({"type": "gdb.command", "text": text, "origin": origin, "token": token})
        # Commands the operator writes in MI form are passed through unchanged;
        # everything else is a console command wrapped for the MI interpreter.
        escaped = text.replace("\\", "\\\\").replace('"', '\\"')
        line = f"{token}{text}\n" if text.startswith("-") else f'{token}-interpreter-exec console "{escaped}"\n'
        self._process.stdin.write(line.encode())
        await self._process.stdin.drain()
        return token

    async def _pump(self) -> None:
        process = self._process
        if process is None or process.stdout is None:
            return
        try:
            while True:
                try:
                    raw = await process.stdout.readline()
                except (ValueError, asyncio.LimitOverrunError):
                    self.broadcast({"type": "gdb.error", "error": "GDB produced an oversized output line"})
                    break
                if not raw:
                    break
                frame = parse_mi(raw.decode("utf-8", errors="replace"))
                if frame:
                    self.broadcast(frame)
        except asyncio.CancelledError:
            raise
        except OSError as exc:
            self.broadcast({"type": "gdb.error", "error": str(exc)})
        finally:
            if process.returncode is None:
                try:
                    await asyncio.wait_for(process.wait(), timeout=1)
                except asyncio.TimeoutError:
                    pass
            self.broadcast({"type": "gdb.exit", "returncode": process.returncode})
