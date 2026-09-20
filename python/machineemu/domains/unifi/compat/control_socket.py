"""Client transport for operator-owned JSON Unix-datagram helper sockets."""
from __future__ import annotations

import asyncio
import json
from pathlib import Path
import socket
import stat
import tempfile

CONTROL_TIMEOUT = 3.0
MAX_RESPONSE = 65536


class HelperError(RuntimeError):
    """The helper is absent, unreachable, malformed, or rejected a request."""


def socket_present(path: Path | str | None) -> bool:
    if not path:
        return False
    try:
        return stat.S_ISSOCK(Path(path).stat().st_mode)
    except OSError:
        return False


def _request(control: Path, message: dict, timeout: float) -> dict:
    with tempfile.TemporaryDirectory(prefix="machineemu-helper-client-") as directory:
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as client:
            client.bind(str(Path(directory) / "client.sock"))
            client.settimeout(timeout)
            client.sendto(json.dumps(message).encode(), str(control))
            raw = client.recv(MAX_RESPONSE)
    response = json.loads(raw)
    if not isinstance(response, dict):
        raise HelperError("helper returned a malformed response")
    if response.get("type") == "error":
        raise HelperError(str(response.get("error", "helper rejected the request")))
    return response


async def request(control: Path | str, message: dict, instance: str | None = None,
                  timeout: float = CONTROL_TIMEOUT, *,
                  absent: str = "control socket is not present; start the helper first",
                  error: type[HelperError] = HelperError) -> dict:
    control = Path(control)
    if not socket_present(control):
        raise error(absent)
    if instance:
        message = {**message, "instance": instance}
    try:
        return await asyncio.wait_for(
            asyncio.to_thread(_request, control, message, timeout), timeout + 1,
        )
    except HelperError as exc:
        raise error(str(exc)) from exc
    except (OSError, ValueError, asyncio.TimeoutError) as exc:
        raise error(f"helper is unreachable: {exc}") from exc
