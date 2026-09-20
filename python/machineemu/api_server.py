"""Run the MachineEmu FastAPI application with operator-owned configuration."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import secrets

def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="machineemu-api")
    parser.add_argument("--operator-config", type=Path, required=True)
    parser.add_argument("--catalog-root", type=Path)
    parser.add_argument("--release-set", type=Path)
    parser.add_argument("--bundle-root", type=Path)
    parser.add_argument("--token", default=os.environ.get("MACHINEEMU_API_TOKEN"))
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8000)
    parser.add_argument("--reload", action="store_true")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        import uvicorn
    except ModuleNotFoundError as exc:
        raise SystemExit("uvicorn is required; install the API extra with: python -m pip install -e '.[api]'") from exc
    from .api import create_app
    from .runtime import OperatorApplication, OperatorConfig

    token = args.token or secrets.token_urlsafe(32)
    config = OperatorConfig.load(args.operator_config)
    application = OperatorApplication(
        config,
        release_set=args.release_set,
        bundle_root=args.bundle_root,
    )
    app = create_app(application, token=token, catalog_root=args.catalog_root)
    if args.token is None:
        print(f"MACHINEEMU_API_TOKEN={token}")
    uvicorn.run(app, host=args.host, port=args.port, reload=args.reload)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
