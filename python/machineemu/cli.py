"""Small operator-facing commands for validating MachineEmu inputs."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

from machineemu.assets import AssetError, AssetStore
from machineemu.engines import EngineRegistry
from machineemu.profiles import ProfileError, resolve_profile


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="machineemu")
    commands = parser.add_subparsers(dest="command", required=True)

    imported = commands.add_parser("asset-import", help="import a file into the SHA-256 asset store")
    imported.add_argument("--asset-root", type=Path, required=True)
    imported.add_argument("--source", type=Path, required=True)
    imported.add_argument("--expected")

    checked = commands.add_parser("profile-check", help="validate a profile and exact engine bundle")
    checked.add_argument("--release-set", type=Path, required=True)
    checked.add_argument("--bundle-root", type=Path, required=True)
    checked.add_argument("--profile", type=Path, required=True)
    checked.add_argument("--target", required=True)
    checked.add_argument("--asset-root", type=Path)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "asset-import":
            reference, path = AssetStore(args.asset_root).import_file(args.source, args.expected)
            print(json.dumps({"reference": reference, "path": str(path)}, sort_keys=True))
            return 0

        asset_store = AssetStore(args.asset_root) if args.asset_root else None
        registry = EngineRegistry.load(args.release_set, args.bundle_root)
        profile = resolve_profile(args.profile, registry, target=args.target, asset_store=asset_store)
        print(json.dumps({
            "profile_id": profile.profile_id,
            "target": profile.target,
            "executable": str(profile.executable),
            "engine": {
                "track_id": profile.engine.track_id,
                "build_digest": profile.engine.build_digest,
            },
            "assets": {name: str(path) for name, path in profile.assets.items()},
        }, sort_keys=True))
        return 0
    except (AssetError, ProfileError, OSError, ValueError) as exc:
        print(f"machineemu: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
