import struct

import pytest

from machineemu.runtime.spice_audio import SpiceClientGate, SpiceProtocolError, SpiceServerGate


def _client_link(channel: int = 1, connection_id: int = 0) -> bytes:
    body = bytearray(22)
    struct.pack_into("<I", body, 0, connection_id)
    body[4:6] = bytes((channel, 0))
    struct.pack_into("<III", body, 6, 1, 0, 18)
    struct.pack_into("<I", body, 18, (1 << 3) | (1 << 1))
    return b"REDQ" + struct.pack("<III", 2, 0, len(body)) + bytes(body) + struct.pack("<I", 1) + (b"x" * 128)


def test_client_gate_accepts_only_the_audio_prelude_and_allowed_messages():
    gate = SpiceClientGate("main", 0)
    payload = _client_link() + b"\x01\x00\x00\x00\x00\x00"
    assert gate.feed(payload) == payload


def test_client_gate_rejects_non_audio_message_types():
    gate = SpiceClientGate("playback", 0)
    gate.feed(_client_link(channel=5))
    with pytest.raises(SpiceProtocolError):
        gate.feed(b"\x65\x00\x00\x00\x00\x00")


def test_server_gate_observes_main_connection_id():
    reply = bytearray(182)
    struct.pack_into("<III", reply, 166, 1, 0, 178)
    struct.pack_into("<I", reply, 178, 1 << 3)
    init = struct.pack("<HI", 103, 32) + struct.pack("<I", 7) + (b"\0" * 28)
    gate = SpiceServerGate("main")
    gate.feed(b"REDQ" + struct.pack("<III", 2, 0, len(reply)) + bytes(reply) + struct.pack("<I", 0) + init)
    assert gate.connection_id == 7
