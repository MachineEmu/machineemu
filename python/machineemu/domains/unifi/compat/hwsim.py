"""Wi-Fi medium control client for an operator-started mac80211_hwsim helper."""
from __future__ import annotations

from pathlib import Path

from .control_socket import CONTROL_TIMEOUT, HelperError, request, socket_present

MEDIUM_SETTINGS = {"signal", "jitter", "loss", "latency_ms", "rate_index", "aggregate", "seed"}
ABSENT = "hwsim control socket is not present; start the helper first"


class HwsimError(HelperError):
    """The hwsim helper is absent, unreachable, or rejected a request."""


def validate_settings(settings: dict) -> dict:
    if not isinstance(settings, dict) or not settings:
        raise HwsimError("at least one medium setting is required")
    unknown = set(settings) - MEDIUM_SETTINGS
    if unknown:
        raise HwsimError(f"unknown medium setting: {', '.join(sorted(unknown))}")
    for name in ("signal", "jitter", "latency_ms", "seed"):
        if name in settings and type(settings[name]) is not int:
            raise HwsimError(f"{name} must be an integer")
    if "loss" in settings and (type(settings["loss"]) not in (int, float)
                               or not 0 <= settings["loss"] <= 1):
        raise HwsimError("loss must be a number between zero and one")
    if "rate_index" in settings and settings["rate_index"] is not None:
        if type(settings["rate_index"]) is not int or not 0 <= settings["rate_index"] < 32:
            raise HwsimError("rate_index must be null or an index below 32")
    if "aggregate" in settings and type(settings["aggregate"]) is not bool:
        raise HwsimError("aggregate must be boolean")
    if "signal" in settings and not -110 <= settings["signal"] <= 0:
        raise HwsimError("signal must be between -110 and 0 dBm")
    if "jitter" in settings and not 0 <= settings["jitter"] <= 60:
        raise HwsimError("jitter must be between 0 and 60 dB")
    if "latency_ms" in settings and not 0 <= settings["latency_ms"] <= 60000:
        raise HwsimError("latency_ms must be between 0 and 60000")
    return settings


async def stats(control: Path | str, instance: str | None = None,
                timeout: float = CONTROL_TIMEOUT) -> dict:
    return await request(control, {"version": 1, "type": "stats"}, instance, timeout,
                         absent=ABSENT, error=HwsimError)


async def configure(control: Path | str, settings: dict, instance: str | None = None,
                    timeout: float = CONTROL_TIMEOUT) -> dict:
    return await request(control, {"version": 1, "type": "configure", "settings": validate_settings(settings)},
                         instance, timeout, absent=ABSENT, error=HwsimError)


async def instances(control: Path | str, timeout: float = CONTROL_TIMEOUT) -> dict:
    return await request(control, {"version": 1, "type": "list"}, None, timeout,
                         absent=ABSENT, error=HwsimError)


def preflight(control: Path | str) -> None:
    if not socket_present(control):
        raise HwsimError(f"hwsim backend socket {control} is not listening; start the helper first")
