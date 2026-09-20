"""Loopback-only password-authenticated VNC forwarding."""

from __future__ import annotations

import asyncio
import secrets
from pathlib import Path


def challenge_response(password: str, challenge: bytes) -> bytes:
    """Implement the classic VNC DES challenge with the RFC bit-reversed key."""
    from cryptography.hazmat.decrepit.ciphers.algorithms import TripleDES
    from cryptography.hazmat.primitives.ciphers import Cipher, modes

    key = bytes(int(f"{byte:08b}"[::-1], 2) for byte in password.encode("ascii")[:8].ljust(8, b"\0"))
    encryptor = Cipher(TripleDES(key * 3), modes.ECB()).encryptor()
    return encryptor.update(challenge) + encryptor.finalize()


class ExternalVncListener:
    """Share a private VNC socket through a password-protected loopback port."""

    def __init__(self, endpoint: Path):
        self.endpoint = endpoint
        self.password = secrets.token_urlsafe(6)
        self.server: asyncio.Server | None = None
        self.tasks: set[asyncio.Task[None]] = set()

    async def start(self) -> dict[str, object]:
        challenge_response(self.password, bytes(16))
        self.server = await asyncio.start_server(self._accept, "127.0.0.1", 0)
        return self.details()

    def details(self) -> dict[str, object]:
        if self.server is None or not self.server.sockets:
            raise RuntimeError("external VNC listener is not running")
        port = self.server.sockets[0].getsockname()[1]
        return {"ok": True, "enabled": True, "port": port, "password": self.password,
                "url": f"vnc://127.0.0.1:{port}"}

    def _accept(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        task = asyncio.create_task(self._proxy(reader, writer))
        self.tasks.add(task)
        task.add_done_callback(self.tasks.discard)

    async def _authenticate(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> bool:
        writer.write(b"RFB 003.008\n")
        await writer.drain()
        version = await reader.readexactly(12)
        if version not in (b"RFB 003.003\n", b"RFB 003.007\n", b"RFB 003.008\n"):
            return False
        if version == b"RFB 003.003\n":
            writer.write((2).to_bytes(4, "big"))
        else:
            writer.write(b"\x01\x02")
            await writer.drain()
            if await reader.readexactly(1) != b"\x02":
                return False
        challenge = secrets.token_bytes(16)
        writer.write(challenge)
        await writer.drain()
        accepted = secrets.compare_digest(await reader.readexactly(16), challenge_response(self.password, challenge))
        writer.write((0 if accepted else 1).to_bytes(4, "big"))
        if not accepted and version == b"RFB 003.008\n":
            reason = b"Incorrect VNC password"
            writer.write(len(reason).to_bytes(4, "big") + reason)
        await writer.drain()
        return accepted

    async def _proxy(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        upstream: tuple[asyncio.StreamReader, asyncio.StreamWriter] | None = None
        pipes: list[asyncio.Task[None]] = []
        try:
            async with asyncio.timeout(30):
                if not await self._authenticate(reader, writer):
                    return
                upstream = await asyncio.open_unix_connection(self.endpoint)
                if await upstream[0].readexactly(12) != b"RFB 003.008\n":
                    raise ConnectionError("private VNC socket did not offer RFB 3.8")
                upstream[1].write(b"RFB 003.008\n")
                await upstream[1].drain()
                count = (await upstream[0].readexactly(1))[0]
                if count == 0 or 1 not in await upstream[0].readexactly(count):
                    raise ConnectionError("private VNC socket did not offer None authentication")
                upstream[1].write(b"\x01")
                await upstream[1].drain()
                if await upstream[0].readexactly(4) != b"\0" * 4:
                    raise ConnectionError("private VNC socket rejected the connection")
                await reader.readexactly(1)
                upstream[1].write(b"\x01")
                await upstream[1].drain()

            async def pipe(source: asyncio.StreamReader, target: asyncio.StreamWriter) -> None:
                while data := await source.read(65536):
                    target.write(data)
                    await target.drain()

            pipes = [asyncio.create_task(pipe(reader, upstream[1])), asyncio.create_task(pipe(upstream[0], writer))]
            await asyncio.wait(pipes, return_when=asyncio.FIRST_COMPLETED)
        except (OSError, asyncio.IncompleteReadError, TimeoutError, ValueError):
            pass
        finally:
            for task in pipes:
                task.cancel()
            await asyncio.gather(*pipes, return_exceptions=True)
            writer.close()
            if upstream:
                upstream[1].close()
                await upstream[1].wait_closed()

    async def stop(self) -> None:
        if self.server is not None:
            self.server.close()
        tasks = tuple(self.tasks)
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        if self.server is not None:
            await self.server.wait_closed()
            self.server = None
