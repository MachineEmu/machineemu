"""Session-scoped remote-device reservations and one-time connection tickets."""

from __future__ import annotations

import hashlib
import secrets
import time
import uuid
from dataclasses import dataclass


@dataclass
class RemoteAttachment:
    attachment_id: str
    instance_id: str
    session_id: str
    mode: str
    profile: str
    owner: str
    state: str = "reserved"
    generation: int = 1
    lease_deadline: float = 0.0
    ticket_digest: str | None = None
    ticket_used: bool = False
    selected_device: dict[str, object] | None = None

    def public(self) -> dict[str, object]:
        return {
            "attachment_id": self.attachment_id,
            "session_id": self.session_id,
            "mode": self.mode,
            "profile": self.profile,
            "selected_device": self.selected_device,
            "state": self.state,
            "generation": self.generation,
            "lease_deadline": self.lease_deadline,
        }


class RemoteDeviceRegistry:
    """Keep reservations in memory; USB payloads remain transport-owned."""

    def __init__(self, *, clock=time.monotonic):
        self._attachments: dict[str, RemoteAttachment] = {}
        self._owners: dict[tuple[str, str], str] = {}
        self._clock = clock

    def create(self, instance_id: str, session_id: str, owner: str, mode: str,
               profile: str, *, enabled: bool, adapter: str,
               profiles: dict[str, dict[str, object]]) -> RemoteAttachment:
        if mode != "generic_usb" or not enabled or adapter != "pc" or profile not in profiles:
            raise ValueError("requested remote-device mode is unavailable")
        owner_key = (session_id, profile)
        if owner_key in self._owners:
            raise ValueError("the approved physical device profile is already reserved")
        item = RemoteAttachment(
            attachment_id=uuid.uuid4().hex,
            instance_id=instance_id,
            session_id=session_id,
            mode=mode,
            profile=profile,
            owner=owner,
            lease_deadline=self._clock() + 300,
        )
        self._attachments[item.attachment_id] = item
        self._owners[owner_key] = item.attachment_id
        return item

    def list(self, instance_id: str, session_id: str, owner: str) -> list[dict[str, object]]:
        self.reap_expired()
        return [item.public() for item in self._attachments.values()
                if item.instance_id == instance_id and item.session_id == session_id and item.owner == owner]

    def ticket(self, attachment_id: str, instance_id: str, session_id: str,
               owner: str, role: str) -> str:
        item = self._get(attachment_id, instance_id, session_id, owner)
        if role not in {"local", "guest"} or item.state not in {"reserved", "connecting", "active"}:
            raise ValueError("attachment is not ticketable")
        if item.ticket_digest is not None and not item.ticket_used:
            raise ValueError("a connection ticket is already outstanding")
        ticket = secrets.token_urlsafe(32)
        item.ticket_digest = hashlib.sha256(ticket.encode()).hexdigest()
        item.ticket_used = False
        item.state = "connecting"
        item.lease_deadline = self._clock() + 300
        return ticket

    def revoke(self, attachment_id: str, instance_id: str, session_id: str,
               owner: str) -> dict[str, object]:
        item = self._get(attachment_id, instance_id, session_id, owner)
        self._attachments.pop(item.attachment_id, None)
        self._owners.pop((item.session_id, item.profile), None)
        item.state = "revoked"
        return item.public()

    def redeem(self, attachment_id: str, instance_id: str, session_id: str,
               owner: str, ticket: str) -> RemoteAttachment:
        item = self._get(attachment_id, instance_id, session_id, owner)
        if item.ticket_digest is None or item.ticket_used:
            raise ValueError("connection ticket is invalid or already used")
        if not secrets.compare_digest(item.ticket_digest, hashlib.sha256(ticket.encode()).hexdigest()):
            raise ValueError("connection ticket is invalid or already used")
        item.ticket_used = True
        item.state = "active"
        item.lease_deadline = self._clock() + 300
        return item

    def validate_metadata(self, item: RemoteAttachment,
                          metadata: dict[str, object], profiles: dict[str, dict[str, object]]) -> None:
        profile = profiles.get(item.profile)
        if not isinstance(profile, dict):
            raise ValueError("device does not match the approved profile")
        if profile.get("accept_all"):
            item.selected_device = {key: metadata.get(key) for key in ("vendor_id", "product_id", "serial", "product_name")}
            return
        if metadata.get("vendor_id") != profile.get("vendor_id") or metadata.get("product_id") != profile.get("product_id"):
            raise ValueError("device does not match the approved profile")
        if profile.get("serial") is not None and metadata.get("serial") != profile["serial"]:
            raise ValueError("device serial does not match the approved profile")
        item.selected_device = {key: metadata.get(key) for key in ("vendor_id", "product_id", "serial", "product_name")}

    def cleanup_ack(self, item: RemoteAttachment, *, quarantined: bool) -> None:
        if quarantined:
            item.state = "quarantined"
        else:
            self._attachments.pop(item.attachment_id, None)
            self._owners.pop((item.session_id, item.profile), None)

    def reap_expired(self) -> None:
        now = self._clock()
        for item in tuple(self._attachments.values()):
            if item.lease_deadline and item.lease_deadline <= now:
                self._attachments.pop(item.attachment_id, None)
                self._owners.pop((item.session_id, item.profile), None)

    def _get(self, attachment_id: str, instance_id: str, session_id: str,
             owner: str) -> RemoteAttachment:
        self.reap_expired()
        item = self._attachments.get(attachment_id)
        if item is None or item.instance_id != instance_id or item.session_id != session_id or item.owner != owner:
            raise KeyError("remote-device attachment not found")
        return item
