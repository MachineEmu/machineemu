"""Bluetooth simulator control client; it never starts or stops the helper."""
from __future__ import annotations

from pathlib import Path

from .control_socket import CONTROL_TIMEOUT, HelperError, request

ABSENT = "Bluetooth control socket is not present; start the simulator first"


class BluetoothError(HelperError):
    """The Bluetooth simulator is absent, unreachable, or rejected a request."""


def validate_peer(peer: dict) -> dict:
    if not isinstance(peer, dict) or not isinstance(peer.get("address"), str):
        raise BluetoothError("a simulated peer needs an address")
    parts = peer["address"].split(":")
    if len(parts) != 6 or any(len(part) != 2 for part in parts):
        raise BluetoothError("a Bluetooth address looks like aa:bb:cc:dd:ee:ff")
    try:
        bytes.fromhex(peer["address"].replace(":", ""))
        data = bytes.fromhex(peer.get("data", ""))
    except (TypeError, ValueError) as exc:
        raise BluetoothError(f"invalid simulated peer: {exc}") from exc
    if len(data) > 31:
        raise BluetoothError("legacy advertising data is at most 31 bytes")
    rssi = peer.get("rssi", -60)
    if type(rssi) is not int or not -127 <= rssi <= 20:
        raise BluetoothError("rssi must be a plausible dBm value")
    for name in ("event_type", "address_type"):
        if name in peer and (type(peer[name]) is not int or not 0 <= peer[name] <= 4):
            raise BluetoothError(f"{name} must be a small integer")
    return {key: peer[key] for key in ("address", "data", "rssi", "event_type", "address_type")
            if key in peer}


async def stats(control: Path | str, instance: str | None = None,
                timeout: float = CONTROL_TIMEOUT) -> dict:
    return await request(control, {"version": 1, "type": "stats"}, instance, timeout,
                         absent=ABSENT, error=BluetoothError)


async def advertise(control: Path | str, peer: dict, instance: str | None = None,
                    timeout: float = CONTROL_TIMEOUT) -> dict:
    return await request(control, {"version": 1, "type": "advertise", "peer": validate_peer(peer)},
                         instance, timeout, absent=ABSENT, error=BluetoothError)


async def configure(control: Path | str, settings: dict, instance: str | None = None,
                    timeout: float = CONTROL_TIMEOUT) -> dict:
    if not isinstance(settings, dict) or set(settings) - {"address", "name"} or not settings:
        raise BluetoothError("only the controller address and name can be configured")
    return await request(control, {"version": 1, "type": "configure", "settings": settings},
                         instance, timeout, absent=ABSENT, error=BluetoothError)


async def instances(control: Path | str, timeout: float = CONTROL_TIMEOUT) -> dict:
    return await request(control, {"version": 1, "type": "list"}, None, timeout,
                         absent=ABSENT, error=BluetoothError)
