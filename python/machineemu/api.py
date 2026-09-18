"""Minimal HTTP adapter over the application operation layer."""

from __future__ import annotations

import json

from fastapi import FastAPI, HTTPException
from pydantic import BaseModel, ConfigDict, Field

from .runtime import OperatorApplication


class SessionRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    instance_id: str = Field(min_length=1, max_length=64)
    session_id: str = Field(min_length=1, max_length=64)


def create_app(application: OperatorApplication) -> FastAPI:
    """Create an API that delegates all stateful work to ``application``."""
    app = FastAPI(title="MachineEmu", version="0.1.0")

    @app.get("/api/v1/health")
    async def health() -> dict[str, str]:
        return {"status": "ok"}

    @app.post("/api/v1/sessions/reconcile")
    async def reconcile(request: SessionRequest) -> dict[str, str]:
        try:
            record = application.open_session(request.instance_id, request.session_id)
            state = application.reconcile_session(record)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        return {"session_id": record.session_id, "state": state}

    @app.get("/api/v1/sessions/{instance_id}/{session_id}")
    async def inspect(instance_id: str, session_id: str) -> dict:
        try:
            record = application.open_session(instance_id, session_id)
            return json.loads(record.manifest.read_text(encoding="utf-8"))
        except (ValueError, OSError, json.JSONDecodeError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    return app
