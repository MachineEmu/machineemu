"""Minimal HTTP adapter over the application operation layer."""

from __future__ import annotations

import hmac
import json
from pathlib import Path
import secrets

from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import JSONResponse
from pydantic import BaseModel, ConfigDict, Field

from .catalog import CatalogError, ProfileCatalog
from .runtime import OperatorApplication


class SessionRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    instance_id: str = Field(min_length=1, max_length=64)
    session_id: str = Field(min_length=1, max_length=64)


class CreateSessionRequest(SessionRequest):
    profile_path: str = Field(min_length=1, max_length=4096)
    target: str = Field(min_length=1, max_length=128)


class CreateCatalogSessionRequest(SessionRequest):
    profile_id: str = Field(min_length=1, max_length=64)
    target: str | None = Field(default=None, max_length=128)


def _loopback_host(host: str) -> bool:
    hostname = host.rsplit(":", 1)[0].strip("[]").lower()
    return hostname in {"localhost", "127.0.0.1", "::1"}


def create_app(application: OperatorApplication, *, token: str | None = None,
               catalog_root: Path | None = None) -> FastAPI:
    """Create an API that delegates all stateful work to ``application``."""
    app = FastAPI(title="MachineEmu", version="0.1.0")
    app.state.token = token or secrets.token_urlsafe(32)
    app.state.catalog = ProfileCatalog(catalog_root) if catalog_root is not None else None
    if app.state.catalog is not None and application.catalog is None:
        application.catalog = app.state.catalog

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

    @app.get("/api/v1/catalog/profiles")
    async def catalog_profiles() -> list[dict]:
        if app.state.catalog is None:
            raise HTTPException(status_code=404, detail="catalog is not configured")
        try:
            return app.state.catalog.list_profiles()
        except CatalogError as exc:
            raise HTTPException(status_code=500, detail=str(exc)) from exc

    @app.get("/api/v1/catalog/profiles/{profile_id}")
    async def catalog_profile(profile_id: str) -> dict:
        if app.state.catalog is None:
            raise HTTPException(status_code=404, detail="catalog is not configured")
        try:
            return app.state.catalog.get(profile_id)
        except CatalogError as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.post("/api/v1/catalog/sessions", status_code=201)
    async def create_catalog_session(request: CreateCatalogSessionRequest) -> dict[str, str]:
        try:
            record = application.create_catalog_session(
                request.profile_id, target=request.target,
                instance_id=request.instance_id, session_id=request.session_id,
            )
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"session_id": record.session_id, "manifest": str(record.manifest), "state": "created"}

    @app.post("/api/v1/sessions/reconcile")
    async def reconcile(request: SessionRequest) -> dict[str, str]:
        try:
            record = application.open_session(request.instance_id, request.session_id)
            state = application.reconcile_session(record)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        return {"session_id": record.session_id, "state": state}

    @app.post("/api/v1/sessions", status_code=201)
    async def create_session(request: CreateSessionRequest) -> dict[str, str]:
        try:
            record = application.create_session(
                Path(request.profile_path), target=request.target,
                instance_id=request.instance_id, session_id=request.session_id,
            )
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"session_id": record.session_id, "manifest": str(record.manifest), "state": "created"}

    @app.get("/api/v1/sessions/{instance_id}/{session_id}")
    async def inspect(instance_id: str, session_id: str) -> dict:
        try:
            record = application.open_session(instance_id, session_id)
            return json.loads(record.manifest.read_text(encoding="utf-8"))
        except (ValueError, OSError, json.JSONDecodeError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.get("/api/v1/instances/{instance_id}/state/inventory")
    async def instance_state_inventory(instance_id: str) -> dict:
        """Hash managed instance state without modifying it."""
        try:
            return application.inventory_instance(instance_id)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/start")
    async def start(instance_id: str, session_id: str) -> dict[str, int | str]:
        try:
            record = application.open_session(instance_id, session_id)
            running = await application.start_recorded_session(record)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"session_id": record.session_id, "pid": running.process.pid, "state": "running"}

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/stop")
    async def stop(instance_id: str, session_id: str) -> dict[str, int | str]:
        try:
            record = application.open_session(instance_id, session_id)
            exit_code = application.stop_session(record)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"session_id": record.session_id, "exit_code": exit_code, "state": "stopped"}

    return app
