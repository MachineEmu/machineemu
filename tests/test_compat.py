import asyncio
import json
import socket
import threading

import pytest

from machineemu.domains.unifi.compat import BluetoothError, HwsimError
from machineemu.domains.unifi.compat import bluetooth, hwsim


def _helper(path, seen, response):
    server = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    server.bind(str(path))

    def serve():
        raw, address = server.recvfrom(65536)
        seen.append(json.loads(raw))
        server.sendto(json.dumps(response).encode(), address)
        server.close()

    thread = threading.Thread(target=serve)
    thread.start()
    return thread


def test_bluetooth_peer_validation_is_strict():
    assert bluetooth.validate_peer({
        "address": "aa:bb:cc:dd:ee:ff", "data": "020106", "rssi": -70,
        "ignored": "not forwarded",
    }) == {"address": "aa:bb:cc:dd:ee:ff", "data": "020106", "rssi": -70}
    with pytest.raises(BluetoothError, match="needs an address"):
        bluetooth.validate_peer({})
    with pytest.raises(BluetoothError, match="at most 31"):
        bluetooth.validate_peer({"address": "aa:bb:cc:dd:ee:ff", "data": "00" * 32})


def test_hwsim_settings_reject_unsafe_values():
    assert hwsim.validate_settings({"signal": -80, "loss": 0.25}) == {"signal": -80, "loss": 0.25}
    with pytest.raises(HwsimError, match="unknown"):
        hwsim.validate_settings({"channel": 1})
    with pytest.raises(HwsimError, match="between -110 and 0"):
        hwsim.validate_settings({"signal": 1})


def test_bluetooth_control_protocol_is_instance_scoped(tmp_path):
    control = tmp_path / "bluetooth.sock"
    seen = []
    thread = _helper(control, seen, {"type": "stats", "controller": {"advertising": True}})
    result = asyncio.run(bluetooth.stats(control, "udm-a"))
    thread.join(timeout=1)
    assert result["controller"]["advertising"] is True
    assert seen == [{"version": 1, "type": "stats", "instance": "udm-a"}]


def test_helper_absence_is_recoverable(tmp_path):
    with pytest.raises(BluetoothError, match="not present"):
        asyncio.run(bluetooth.stats(tmp_path / "missing.sock"))
    with pytest.raises(HwsimError, match="not listening"):
        hwsim.preflight(tmp_path / "missing.sock")
