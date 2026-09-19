"""Short-lived browser-terminal authorization without putting API tokens in URLs."""

from __future__ import annotations

import hmac
import secrets
import time


class TerminalTicketStore:
    """Issue one-time tickets bound to exactly one owned session."""

    def __init__(self, *, lifetime_seconds: float = 30.0):
        self.lifetime_seconds = lifetime_seconds
        self._tickets: dict[str, tuple[float, str, str]] = {}

    def issue(self, instance_id: str, session_id: str) -> str:
        self._discard_expired()
        ticket = secrets.token_urlsafe(32)
        self._tickets[ticket] = (time.monotonic() + self.lifetime_seconds, instance_id, session_id)
        return ticket

    def consume(self, ticket: str, instance_id: str, session_id: str) -> bool:
        self._discard_expired()
        if not isinstance(ticket, str):
            return False
        found = self._tickets.pop(ticket, None)
        if found is None:
            return False
        _, expected_instance, expected_session = found
        return hmac.compare_digest(expected_instance, instance_id) and hmac.compare_digest(expected_session, session_id)

    def _discard_expired(self) -> None:
        now = time.monotonic()
        for ticket, (expires_at, _, _) in list(self._tickets.items()):
            if expires_at <= now:
                self._tickets.pop(ticket, None)
