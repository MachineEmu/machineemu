import asyncio
import json

from machineemu.runtime import QMPClient, QMPError


def test_qmp_negotiates_executes_and_collects_events(tmp_path):
    async def scenario():
        socket_path = tmp_path / "qmp.sock"
        ready = asyncio.Event()

        async def handler(reader, writer):
            writer.write(b'{"QMP":{"version":{}}}\r\n')
            await writer.drain()
            ready.set()
            while line := await reader.readline():
                request = json.loads(line)
                command = request["execute"]
                if command == "qmp_capabilities":
                    response = {"return": {}, "id": request["id"]}
                else:
                    writer.write(b'{"event":"RESET"}\r\n')
                    response = {"return": {"status": "running"}, "id": request["id"]}
                writer.write((json.dumps(response) + "\r\n").encode())
                await writer.drain()
            writer.close()
            await writer.wait_closed()

        server = await asyncio.start_unix_server(handler, path=socket_path)
        async with server:
            client = await QMPClient.connect(socket_path)
            await ready.wait()
            assert await client.execute("query-status") == {"status": "running"}
            assert (await client.wait_event("RESET"))["event"] == "RESET"
            await client.close()

    asyncio.run(scenario())


def test_qmp_rejects_bad_greeting(tmp_path):
    async def scenario():
        socket_path = tmp_path / "qmp.sock"

        async def handler(reader, writer):
            writer.write(b'{"hello":true}\r\n')
            await writer.drain()
            await asyncio.sleep(0.1)
            writer.close()

        server = await asyncio.start_unix_server(handler, path=socket_path)
        async with server:
            try:
                await QMPClient.connect(socket_path, timeout=1)
            except QMPError as exc:
                assert "greeting" in str(exc)
            else:
                raise AssertionError("bad QMP greeting was accepted")

    asyncio.run(scenario())
