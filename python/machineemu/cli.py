"""Small operator-facing commands for validating MachineEmu inputs."""

from __future__ import annotations

import argparse
import asyncio
import json
from pathlib import Path
import sys

from machineemu.assets import AssetError, AssetStore
from machineemu.engines import EngineRegistry
from machineemu.profiles import ProfileError, resolve_profile
from machineemu.runtime import OperatorConfig, SessionStore, SessionSupervisor


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

    created = commands.add_parser("session-create", help="create isolated state for a validated profile")
    created.add_argument("--operator-config", type=Path, required=True)
    created.add_argument("--release-set", type=Path, required=True)
    created.add_argument("--bundle-root", type=Path, required=True)
    created.add_argument("--profile", type=Path, required=True)
    created.add_argument("--target", required=True)
    created.add_argument("--instance-id", required=True)
    created.add_argument("--session-id", required=True)

    inspected = commands.add_parser("session-inspect", help="inspect an existing session manifest")
    inspected.add_argument("--operator-config", type=Path, required=True)
    inspected.add_argument("--instance-id", required=True)
    inspected.add_argument("--session-id", required=True)

    started = commands.add_parser("session-start", help="start a session command and attach QMP")
    started.add_argument("--operator-config", type=Path, required=True)
    started.add_argument("--instance-id", required=True)
    started.add_argument("--session-id", required=True)
    started.add_argument("--qmp-socket", type=Path, required=True)
    started.add_argument("exec_command", nargs=argparse.REMAINDER, help="command to execute after --")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "asset-import":
            reference, path = AssetStore(args.asset_root).import_file(args.source, args.expected)
            print(json.dumps({"reference": reference, "path": str(path)}, sort_keys=True))
            return 0

        if args.command == "session-create":
            config = OperatorConfig.load(args.operator_config)
            registry = EngineRegistry.load(args.release_set, args.bundle_root)
            profile = resolve_profile(
                args.profile, registry, target=args.target,
                asset_store=AssetStore(config.asset_root),
            )
            record = SessionStore(
                config.runtime_root, config.state_root, config.artifact_root,
            ).create(args.instance_id, args.session_id, profile)
            print(json.dumps({
                "session_id": record.session_id,
                "instance_id": record.instance_id,
                "manifest": str(record.manifest),
                "runtime_dir": str(record.runtime_dir),
                "state_dir": str(record.state_dir),
                "artifact_dir": str(record.artifact_dir),
            }, sort_keys=True))
            return 0

        if args.command == "session-inspect":
            config = OperatorConfig.load(args.operator_config)
            record = SessionStore(
                config.runtime_root, config.state_root, config.artifact_root,
            ).open(args.instance_id, args.session_id)
            print(record.manifest.read_text(encoding="utf-8"), end="")
            return 0

        if args.command == "session-start":
            command = list(args.exec_command)
            if command and command[0] == "--":
                command = command[1:]
            if not command:
                raise ValueError("session-start requires a command after --")
            config = OperatorConfig.load(args.operator_config)
            store = SessionStore(config.runtime_root, config.state_root, config.artifact_root)
            record = store.open(args.instance_id, args.session_id)
            running = asyncio.run(SessionSupervisor(store).start(record, command, args.qmp_socket))
            print(json.dumps({"session_id": record.session_id, "pid": running.process.pid}, sort_keys=True))
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
