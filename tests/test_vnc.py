import pytest

from machineemu.runtime.vnc import RfbInputGate, RfbProtocolError


def test_rfb_gate_forwards_fragmented_handshake_and_non_input_messages():
    gate = RfbInputGate()
    handshake = b"RFB 003.008\n" + b"\x01\x01"
    assert gate.feed(handshake[:5], allow_input=False) == handshake[:5]
    assert gate.feed(handshake[5:], allow_input=False) == handshake[5:]
    update_request = b"\x03\x00\x00\x00\x00\x00\x01\x00\x01\x00"
    assert gate.feed(update_request[:3], allow_input=False) == b""
    assert gate.feed(update_request[3:], allow_input=False) == update_request


def test_rfb_gate_drops_input_without_lease_and_forwards_with_lease():
    gate = RfbInputGate()
    gate.feed(b"RFB 003.008\n\x01\x01", allow_input=False)
    key = b"\x04\x01\x00\x00\x00\x1b\x00\x00"
    assert gate.feed(key, allow_input=False) == b""
    assert gate.feed(key, allow_input=True) == key


def test_rfb_gate_rejects_unknown_and_oversized_clipboard_messages():
    gate = RfbInputGate()
    gate.feed(b"RFB 003.008\n\x01\x01", allow_input=False)
    with pytest.raises(RfbProtocolError, match="unsupported"):
        gate.feed(b"\x99", allow_input=False)
    gate = RfbInputGate()
    gate.feed(b"RFB 003.008\n\x01\x01", allow_input=False)
    with pytest.raises(RfbProtocolError, match="clipboard"):
        gate.feed(b"\x06\x00\x00\x00\x00\x7f\xff\xff\xff", allow_input=False)
