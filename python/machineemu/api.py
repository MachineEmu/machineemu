"""Minimal HTTP adapter over the application operation layer."""

from __future__ import annotations

import hmac
import json
import secrets

from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import JSONResponse
from pydantic import BaseModel, ConfigDict, Field

from .runtime import OperatorApplication


class SessionRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    instance_id: str = Field(min_length=1, max_length=64)
    session_id: str = Field(min_length=1, max_length=64)


def _loopback_host(host: str) -> bool:
    hostname = host.rsplit(":", 1)[0].strip("[]").lower()
    return hostname in {"localhost", "127.0.0.1", "::1"}


def create_app(application: OperatorApplication, *, token: str | None = None) -> FastAPI:
    """Create an API that delegates all stateful work to ``application``."""
    app = FastAPI(title="MachineEmu", version="0.1.0")
    app.state.token = token or secrets.token_urlsafe(32)

    @app.middleware("http")
    async def security(request: Request, call_next):
        if not _loopback_host(request.headers.get("host", "")):
            return JSONResponse({"detail": "loopback Host required"}, status_code=403)
        if request.url.path.startswith("/api/"):
            supplied = request.headers.get("x-machineemu-token", "")
            if not hmac.compare_digest(supplied, app.state.token):
                return JSONResponse({"detail": "application token required"}, status_code=401)
            if request.method not in {"GET", "HEAD", "OPTIONS"}:
                origin = request.headers.get("origin")
                expected = f"{request.url.scheme}://{request.headers.get('host', '')}"
                if origin != expected:
                    return JSONResponse({"detail": "same-origin request required"}, status_code=403)
        response = await call_next(request)
        response.headers["X-Content-Type-Options"] = "nosniff"
        response.headers["X-Frame-Options"] = "DENY"
        response.headers["Referrer-Policy"] = "no-referrer"
        response.headers["Cache-Control"] = "no-store"
        return response

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
