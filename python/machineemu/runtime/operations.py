"""Small in-process operation journal for API lifecycle actions."""

from __future__ import annotations

from dataclasses import dataclass, field
import json
from pathlib import Path
import secrets
import tempfile
import time
from typing import Any


@dataclass
class Operation:
    operation_id: str
    kind: str
    session_id: str
    state: str = "accepted"
    result: dict[str, Any] = field(default_factory=dict)
    error: dict[str, str] | None = None
    created_at: float = field(default_factory=time.time)

    def public(self) -> dict[str, Any]:
        value: dict[str, Any] = {
            "operation_id": self.operation_id,
            "kind": self.kind,
            "session_id": self.session_id,
            "state": self.state,
            "created_at": self.created_at,
        }
        if self.result:
            value["result"] = self.result
        if self.error is not None:
            value["error"] = self.error
        return value


class OperationJournal:
    """Bounded journal whose entries never contain host paths or credentials."""

    def __init__(self, limit: int = 512, path: Path | None = None):
        self.limit = limit
        self.path = path
        self._entries: dict[str, Operation] = {}
        self._load()

    def _load(self) -> None:
        if self.path is None or not self.path.is_file():
            return
        try:
            values = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return
        if not isinstance(values, list):
            return
        for value in values[-self.limit:]:
            if not isinstance(value, dict) or not all(isinstance(value.get(key), str)
                                                       for key in ("operation_id", "kind", "session_id", "state")):
                continue
            try:
                created_at = float(value.get("created_at", time.time()))
            except (TypeError, ValueError):
                created_at = time.time()
            self._entries[value["operation_id"]] = Operation(
                value["operation_id"], value["kind"], value["session_id"], value["state"],
                value.get("result", {}) if isinstance(value.get("result", {}), dict) else {},
                value.get("error") if isinstance(value.get("error"), dict) else None,
                created_at,
            )

    def _save(self) -> None:
        if self.path is None:
            return
        self.path.parent.mkdir(parents=True, exist_ok=True)
        values = [entry.public() for entry in self._entries.values()]
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=self.path.parent,
                                         prefix=f".{self.path.name}.", delete=False) as stream:
            json.dump(values, stream, sort_keys=True)
            stream.write("\n")
            temporary = Path(stream.name)
        temporary.replace(self.path)

    def create(self, kind: str, session_id: str) -> Operation:
        operation = Operation(secrets.token_urlsafe(18), kind, session_id)
        self._entries[operation.operation_id] = operation
        while len(self._entries) > self.limit:
            oldest = min(self._entries, key=lambda key: self._entries[key].created_at)
            del self._entries[oldest]
        self._save()
        return operation

    def update(self, operation: Operation, state: str, *, result: dict[str, Any] | None = None,
               error: dict[str, str] | None = None) -> None:
        if state not in {"accepted", "executing", "succeeded", "failed"}:
            raise ValueError("invalid operation state")
        operation.state = state
        if result is not None:
            operation.result = result
        if error is not None:
            operation.error = error
        self._save()

    def get(self, operation_id: str) -> Operation | None:
        return self._entries.get(operation_id)
