"""Validated read-only fanout for front-panel state.

The US24PRO board model drives real LED registers and streams them; the UDM-Pro
has no LED block, so its ports are reported from the two things the console
does know: the instance's ordered PCI port table and whether the host end of
each link is carrying. That is stated in every frame's `source`, so a reader
can tell a modelled panel from a derived one.
"""
import asyncio
import json
from pathlib import Path
import time

MAX_FRAME = 65536
UDM_POLL_SECONDS = 2
# The ordered PCI slots of a UDM-Pro, as boards.py renders them.
UDM_PORTS = (("eth9", "sfp"), ("eth8", "rj45"), ("eth10", "sfp"), ("switch0", "rj45"))


def host_link(backend: dict) -> tuple[bool, str | None]:
    """Whether the host end of one port's backend is carrying, and what it is."""
    mode = backend.get("mode", "disabled")
    name = backend.get("bridge") if mode == "bridge" else backend.get("ifname") if mode == "tap" else None
    if mode == "user":
        return True, None
    if name is None:
        return False, None
    try:
        carrier = (Path("/sys/class/net") / name / "carrier").read_text().strip()
    except OSError:
        return False, name
    return carrier == "1", name


def udm_ports(resolved: dict) -> list[dict]:
    """One front-panel port per PCI slot, from the instance's port table."""
    network = resolved.get("network", {})
    table = network.get("ports") or [network]
    ports = []
    for index, (label, kind) in enumerate(UDM_PORTS):
        backend = table[index] if index < len(table) else {}
        link, interface = host_link(backend if isinstance(backend, dict) else {})
        ports.append({"port": index + 1, "kind": kind, "label": label,
                      "mode": (backend or {}).get("mode", "disabled"),
                      "interface": interface, "link": link, "rx": 0, "tx": 0,
                      "speed_mbps": None})
    return ports


class FrontPanelHub:
    def __init__(self, runtime: Path, artifacts: Path, resolved: dict | None = None):
        self.runtime = runtime
        self.artifacts = artifacts
        self.resolved = resolved
        self.latest = self.encode({"schema": "unifi.frontpanel.v1", "kind": "waiting", "ports": []})
        self.clients = set()
        self.tasks = set()
        self.source = None
        self.server = None
        self.task = None

    @staticmethod
    def encode(value):
        return (json.dumps(value, ensure_ascii=True, separators=(",", ":")) + "\n").encode()

    async def start(self):
        path = self.runtime / "frontpanel.sock"
        derived = self.resolved is not None and self.resolved.get("adapter") == "udm-pro"
        self.task = asyncio.create_task(self.derive() if derived else self.connect_and_capture())
        if not derived:
            deadline = time.monotonic() + 10
            while self.source is None:
                if self.task.done() or time.monotonic() >= deadline:
                    raise RuntimeError("front-panel event socket unavailable")
                await asyncio.sleep(.05)
        self.server = await asyncio.start_unix_server(self.subscribe, path)
        path.chmod(0o600)

    async def derive(self):
        """Publish ports derived from configuration and host carrier state."""
        sequence = 0
        try:
            with (self.artifacts / "frontpanel.jsonl").open("ab") as log:
                while True:
                    sequence += 1
                    frame = self.encode({"schema": "unifi.frontpanel.v1", "kind": "ports",
                                         "source": "configuration and host carrier",
                                         "device": "UniFi Dream Machine Pro",
                                         "sequence": sequence, "host_time": time.time(),
                                         "ports": udm_ports(self.resolved)})
                    log.write(frame)
                    log.flush()
                    self.publish(frame)
                    await asyncio.sleep(UDM_POLL_SECONDS)
        except asyncio.CancelledError:
            raise
        except (OSError, ValueError) as exc:
            self.publish(self.encode({"schema": "unifi.frontpanel.v1", "kind": "error",
                                      "error": str(exc)}))

    async def connect_and_capture(self):
        deadline = time.monotonic() + 30
        while True:
            try:
                reader, self.source = await asyncio.open_unix_connection(
                    self.runtime / "frontpanel-events.sock", limit=MAX_FRAME
                )
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if time.monotonic() >= deadline:
                    self.publish(self.encode({"schema": "unifi.frontpanel.v1", "kind": "error",
                                              "error": "front-panel event socket unavailable"}))
                    return
                await asyncio.sleep(0.05)
        await self.capture(reader)

    def publish(self, frame):
        self.latest = frame
        for queue in self.clients:
            if queue.full():
                queue.get_nowait()
            queue.put_nowait(frame)

    async def capture(self, reader):
        try:
            with (self.artifacts / "frontpanel.jsonl").open("ab") as log:
                while raw := await reader.readline():
                    if len(raw) > MAX_FRAME or not raw.endswith(b"\n"):
                        raise ValueError("invalid front-panel frame")
                    value = json.loads(raw)
                    if not isinstance(value, dict) or value.get("schema") != "unifi.frontpanel.v1":
                        raise ValueError("invalid front-panel event schema")
                    value["host_time"] = time.time()
                    frame = self.encode(value)
                    log.write(frame)
                    log.flush()
                    self.publish(frame)
        except (ValueError, OSError) as exc:
            self.publish(self.encode({"schema": "unifi.frontpanel.v1", "kind": "error", "error": str(exc)}))

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
                        break
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
        if self.server:
            self.server.close()
            await self.server.wait_closed()
        if self.task:
            self.task.cancel()
            await asyncio.gather(self.task, return_exceptions=True)
        for task in list(self.tasks):
            task.cancel()
        await asyncio.gather(*self.tasks, return_exceptions=True)
        if self.source:
            self.source.close()
            try:
                await self.source.wait_closed()
            except ConnectionError:
                pass
        path = self.runtime / "frontpanel.sock"
        if path.exists():
            path.unlink()
