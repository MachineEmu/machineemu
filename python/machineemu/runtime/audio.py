"""Session-scoped audio client bindings and exclusive capture leases."""

from __future__ import annotations

import secrets
import time
from dataclasses import dataclass, field


BINDING_TTL_SECONDS = 60
CAPTURE_TTL_SECONDS = 30


@dataclass
class AudioClient:
    token: str
    instance_id: str
    session_id: str
    principal: str
    deadline: float
    capture_deadline: float = 0.0
    connection_id: int | None = None
    channels: set[str] = field(default_factory=set)


class AudioClientRegistry:
    def __init__(self, *, clock=time.monotonic):
        self._clients: dict[tuple[str, str], dict[str, AudioClient]] = {}
        self._capture: dict[tuple[str, str], tuple[str, float]] = {}
        self._clock = clock

    def attach(self, instance_id: str, session_id: str, principal: str) -> AudioClient:
        client = AudioClient(secrets.token_hex(16), instance_id, session_id, principal,
                             self._clock() + BINDING_TTL_SECONDS)
        self._clients.setdefault((instance_id, session_id), {})[client.token] = client
        return client

    def lookup(self, instance_id: str, session_id: str, token: str, principal: str) -> AudioClient | None:
        self.reap()
        client = self._clients.get((instance_id, session_id), {}).get(token)
        if client is None or client.principal != principal:
            return None
        return client

    def renew(self, client: AudioClient) -> int:
        client.deadline = self._clock() + BINDING_TTL_SECONDS
        return BINDING_TTL_SECONDS

    def capture_owner(self, instance_id: str, session_id: str) -> str | None:
        self.reap()
        owner = self._capture.get((instance_id, session_id))
        return owner[0] if owner else None

    def claim(self, client: AudioClient, takeover: bool = False) -> bool:
        self.reap()
        key = (client.instance_id, client.session_id)
        owner = self._capture.get(key)
        if owner and owner[0] != client.token and not takeover:
            return False
        self._capture[key] = (client.token, self._clock() + CAPTURE_TTL_SECONDS)
        client.capture_deadline = self._capture[key][1]
        return True

    def release(self, client: AudioClient) -> bool:
        if self._capture.get((client.instance_id, client.session_id), (None,))[0] != client.token:
            return False
        self._capture.pop((client.instance_id, client.session_id), None)
        client.capture_deadline = 0.0
        return True

    def revoke(self, client: AudioClient) -> None:
        self.release(client)
        client.channels.clear()
        client.connection_id = None
        self._clients.get((client.instance_id, client.session_id), {}).pop(client.token, None)

    def reap(self) -> None:
        now = self._clock()
        for key, clients in tuple(self._clients.items()):
            for token, client in tuple(clients.items()):
                if client.deadline <= now:
                    if self._capture.get(key, (None,))[0] == token:
                        self._capture.pop(key, None)
                    clients.pop(token, None)
            if not clients:
                self._clients.pop(key, None)
        for key, (_, deadline) in tuple(self._capture.items()):
            if deadline <= now:
                self._capture.pop(key, None)
