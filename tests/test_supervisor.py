import asyncio
import json
import sys

from machineemu.engines import EngineRegistry
from machineemu.profiles import resolve_profile
from machineemu.runtime import SessionStore, SessionSupervisor


def _record(tmp_path):
    bundle = tmp_path / "bundles" / "track"
    (bundle / "bin").mkdir(parents=True)
    (bundle / "bin/qemu").write_bytes(b"qemu")
    digest = "1" * 64
    (bundle / "engine-build.json").write_text(json.dumps({
        "schema_version": 1, "track_id": "track", "build_digest": digest,
        "source_revision": "commit", "targets": ["aarch64-softmmu"],
        "executables": {"aarch64-softmmu": "bin/qemu"}, "dirty_source": False,
    }), encoding="utf-8")
    release = tmp_path / "release.json"
    release.write_text(json.dumps({"schema_version": 1, "engines": {
        "track": {"manifest": "track/engine-build.json", "build_digest": digest}
    }}), encoding="utf-8")
    profile = tmp_path / "profile.json"
    profile.write_text(json.dumps({"schema_version": 1, "id": "debian",
        "engine": {"track": "track"}, "machine": "virt"}), encoding="utf-8")
    resolved = resolve_profile(profile, EngineRegistry.load(release, tmp_path / "bundles"), target="aarch64-softmmu")
    store = SessionStore(tmp_path / "run", tmp_path / "state", tmp_path / "artifacts")
    return store, store.create("instance", "session", resolved)


def test_supervisor_attaches_qmp_and_stops_process(tmp_path):
    async def scenario():
        store, record = _record(tmp_path)
        qmp_socket = record.runtime_dir / "sockets/qmp.sock"

        async def handler(reader, writer):
            writer.write(b'{"QMP":{"version":{}}}\r\n')
            await writer.drain()
            while line := await reader.readline():
                request = json.loads(line)
                writer.write((json.dumps({"return": {}, "id": request["id"]}) + "\r\n").encode())
                await writer.drain()
                if request["execute"] == "qmp_capabilities":
                    writer.close()
                    await writer.wait_closed()
                    return

        server = await asyncio.start_unix_server(handler, path=qmp_socket)
        async with server:
            running = await SessionSupervisor(store).start(
                record, [sys.executable, "-c", "import time; time.sleep(30)"], qmp_socket,
                qmp_timeout=1,
            )
            assert json.loads(record.manifest.read_text())["qmp_socket"] == str(qmp_socket)
            await running.stop(timeout=1)

    asyncio.run(scenario())
