"""Read-only display subscription socket; never exposes the guest UART or QMP."""
import asyncio
import json
from pathlib import Path
import time

MAX_FRAME = 65536


class LcmHub:
    def __init__(self, runtime: Path, artifacts: Path):
        self.runtime = runtime
        self.artifacts = artifacts
        self.latest = self.encode({"schema": "unifi.lcm.v1", "kind": "waiting",
                                   "pixel_exact": False, "ui": {}, "system": {}})
        self.clients = set()
        self.tasks = set()
        self.source = None
        self.server = None
        self.task = None
        self.input_server = None
        self.input_source = None
        self.input_reader = None
        self.input_lock = asyncio.Lock()

    @staticmethod
    def encode(value):
        return (json.dumps(value, ensure_ascii=True, separators=(',', ':')) + '\n').encode()

    async def start(self):
        path = self.runtime / 'display.sock'
        self.server = await asyncio.start_unix_server(self.subscribe, path)
        path.chmod(0o600)
        deadline = time.monotonic() + 30
        while True:
            try:
                reader, self.source = await asyncio.open_unix_connection(
                    self.runtime / 'lcm-events.sock', limit=MAX_FRAME)
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if time.monotonic() >= deadline:
                    raise
                await asyncio.sleep(.05)
        self.task = asyncio.create_task(self.capture(reader))
        self.input_reader, self.input_source = await asyncio.open_unix_connection(
            self.runtime / 'lcm-input.sock', limit=2048)
        path = self.runtime / 'display-input.sock'
        self.input_server = await asyncio.start_unix_server(self.control, path, limit=2048)
        path.chmod(0o600)

    async def control(self, reader, writer):
        task = asyncio.current_task()
        if len(self.tasks) >= 32:
            writer.close()
            return
        self.tasks.add(task)
        try:
            raw = await asyncio.wait_for(reader.readline(), 2)
            value = json.loads(raw)
            if not isinstance(value, dict) or len(raw) > 1024:
                raise ValueError('expected one action object, at most 1024 bytes')
            async with self.input_lock:
                if self.input_source is None:
                    raise ValueError('input transport unavailable; restart session')
                try:
                    self.input_source.write(self.encode(value))
                    await asyncio.wait_for(self.input_source.drain(), 2)
                    response = await asyncio.wait_for(self.input_reader.readline(), 5)
                    if not response:
                        raise ConnectionError('input transport closed')
                    result = json.loads(response)
                except (OSError, ValueError, asyncio.TimeoutError):
                    # Do not risk attributing a late reply to a different action.
                    self.input_source.close()
                    self.input_source = None
                    raise
            writer.write(self.encode(result))
            await asyncio.wait_for(writer.drain(), 2)
        except (OSError, ValueError, asyncio.TimeoutError) as exc:
            writer.write(self.encode({'ok': False, 'error': str(exc) or 'input timeout; delivery unknown'}))
            try:
                await asyncio.wait_for(writer.drain(), 2)
            except (OSError, asyncio.TimeoutError):
                pass
        finally:
            self.tasks.discard(task)
            writer.close()
            try:
                await writer.wait_closed()
            except ConnectionError:
                pass

    def publish(self, frame):
        self.latest = frame
        for queue in self.clients:
            if queue.full():
                queue.get_nowait()
            queue.put_nowait(frame)

    async def capture(self, reader):
        try:
            with (self.artifacts / 'lcm.jsonl').open('ab') as log:
                while raw := await reader.readline():
                    value = json.loads(raw)
                    if not isinstance(value, dict) or value.get('schema') != 'unifi.lcm.v1':
                        raise ValueError('invalid LCM event schema')
                    value['host_time'] = time.time()
                    frame = self.encode(value)
                    log.write(frame)
                    log.flush()
                    self.publish(frame)
        except (ValueError, OSError) as exc:
            self.publish(self.encode({'schema': 'unifi.lcm.v1', 'kind': 'error', 'error': str(exc)}))

    async def subscribe(self, reader, writer):
        task = asyncio.current_task()
        self.tasks.add(task)
        queue = asyncio.Queue(maxsize=1)
        if len(self.clients) >= 16:
            writer.close()
            await writer.wait_closed()
            self.tasks.discard(task)
            return
        self.clients.add(queue)
        queue.put_nowait(self.latest)
        eof = asyncio.create_task(reader.read(1))
        try:
            while True:
                update = asyncio.create_task(queue.get())
                try:
                    done, _ = await asyncio.wait([eof, update], return_when=asyncio.FIRST_COMPLETED)
                    if eof in done:
                        break  # Disconnect, or any input: this socket is read-only.
                    writer.write(update.result())
                    await asyncio.wait_for(writer.drain(), 2)
                finally:
                    update.cancel()
                    await asyncio.gather(update, return_exceptions=True)
        except (ConnectionError, asyncio.TimeoutError):
            pass
        finally:
            eof.cancel()
            await asyncio.gather(eof, return_exceptions=True)
            self.clients.discard(queue)
            self.tasks.discard(task)
            writer.close()
            try:
                await writer.wait_closed()
            except ConnectionError:
                pass

    async def close(self):
        if self.input_server:
            self.input_server.close()
            await self.input_server.wait_closed()
        if self.server:
            self.server.close()
            await self.server.wait_closed()
        if self.task:
            self.task.cancel()
            await asyncio.gather(self.task, return_exceptions=True)
        tasks = list(self.tasks)
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        if self.source:
            self.source.close()
            try:
                await self.source.wait_closed()
            except ConnectionError:
                pass
        if self.input_source:
            self.input_source.close()
            try:
                await self.input_source.wait_closed()
            except ConnectionError:
                pass
        input_path = self.runtime / 'display-input.sock'
        if input_path.exists():
            input_path.unlink()
        path = self.runtime / 'display.sock'
        if path.exists():
            path.unlink()
