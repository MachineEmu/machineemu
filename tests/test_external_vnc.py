import asyncio

import pytest

from machineemu.runtime.external_vnc import ExternalVncListener, challenge_response


def test_vnc_challenge_response_is_deterministic():
    try:
        first = challenge_response("secret", bytes(range(16)))
    except ImportError:
        pytest.skip("cryptography is not installed")
    assert first == challenge_response("secret", bytes(range(16)))
    assert len(first) == 16


def test_external_vnc_listener_binds_loopback_and_stops(tmp_path):
    async def scenario():
        listener = ExternalVncListener(tmp_path / "vnc.sock")
        details = await listener.start()
        assert details["enabled"] is True
        assert details["url"].startswith("vnc://127.0.0.1:")
        await listener.stop()

    try:
        asyncio.run(scenario())
    except ImportError:
        pytest.skip("cryptography is not installed")
