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
from machineemu.runtime import OperatorApplication, OperatorConfig, inventory_json, validate_inventory
from machineemu.domains.analysis import missing_observation_fields, record_observation, validate_clone
from machineemu.domains.analysis.kvm_guard import DEFAULT_MODULE, DEFAULT_STATS, load_command, session_from_record, snapshot, status
from machineemu.domains.analysis.acpi import dump_acpi_tables


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
    started.add_argument("--qmp-socket", type=Path)
    started.add_argument("exec_command", nargs=argparse.REMAINDER, help="command to execute after --")

    reconciled = commands.add_parser("session-reconcile", help="reconcile a persisted session PID")
    reconciled.add_argument("--operator-config", type=Path, required=True)
    reconciled.add_argument("--instance-id", required=True)
    reconciled.add_argument("--session-id", required=True)

    stopped = commands.add_parser("session-stop", help="stop a running session with bounded escalation")
    stopped.add_argument("--operator-config", type=Path, required=True)
    stopped.add_argument("--instance-id", required=True)
    stopped.add_argument("--session-id", required=True)
    stopped.add_argument("--timeout", type=float, default=5.0)

    inventoried = commands.add_parser("state-inventory", help="hash a state tree without modifying it")
    inventoried.add_argument("--source", type=Path, required=True)
    validated = commands.add_parser("state-validate", help="validate a state tree against an inventory")
    validated.add_argument("--source", type=Path, required=True)
    validated.add_argument("--inventory", type=Path, required=True)
    imported_state = commands.add_parser("state-import", help="import a file guarded by a state inventory")
    imported_state.add_argument("--operator-config", type=Path, required=True)
    imported_state.add_argument("--instance-id", required=True)
    imported_state.add_argument("--source", type=Path, required=True)
    imported_state.add_argument("--inventory", type=Path, required=True)
    imported_state.add_argument("--source-path", required=True)
    imported_state.add_argument("--name", required=True)

    guard_load = commands.add_parser("analysis-kvm-guard-load-command", help="print the privileged KVM guard load command")
    guard_load.add_argument("--operator-config", type=Path, required=True)
    guard_load.add_argument("--instance-id", required=True)
    guard_load.add_argument("--session-id", required=True)
    guard_load.add_argument("--module", type=Path, default=DEFAULT_MODULE)
    guard_load.add_argument("--no-hook-exits", action="store_true")
    guard_load.add_argument("--no-hook-tsc", action="store_true")
    guard_load.add_argument("--hyperv-fast-mode", action=argparse.BooleanOptionalAction, default=None)

    for name, help_text in (("analysis-kvm-guard-status", "inspect KVM guard state"),
                            ("analysis-kvm-guard-snapshot", "save KVM guard counters")):
        guard = commands.add_parser(name, help=help_text)
        guard.add_argument("--operator-config", type=Path, required=True)
        guard.add_argument("--instance-id", required=True)
        guard.add_argument("--session-id", required=True)
        guard.add_argument("--stats-path", type=Path, default=DEFAULT_STATS)
    acpi = commands.add_parser("analysis-acpi-dump", help="capture host ACPI tables")
    acpi.add_argument("--metadata-only", action="store_true")
    acpi.add_argument("--output-dir", type=Path)
    acpi.add_argument("--helper")
    clone_check = commands.add_parser("analysis-clone-validate", help="validate an analysis clone directory")
    clone_check.add_argument("--directory", type=Path, required=True)
    clone_check.add_argument("--qemu-img", type=Path)
    observe = commands.add_parser("analysis-observe", help="record guest observations in an environment report")
    observe.add_argument("--report", type=Path, required=True)
    observe.add_argument("--observation", type=Path, required=True)
    observe.add_argument("--allow-incomplete", action="store_true")
    observe.add_argument("--helper")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "asset-import":
            reference, path = AssetStore(args.asset_root).import_file(args.source, args.expected)
            print(json.dumps({"reference": reference, "path": str(path)}, sort_keys=True))
            return 0

        if args.command == "state-inventory":
            print(json.dumps(inventory_json(args.source), indent=2, sort_keys=True))
            return 0

        if args.command == "state-validate":
            expected = json.loads(args.inventory.read_text(encoding="utf-8"))
            print(json.dumps(validate_inventory(args.source, expected), indent=2, sort_keys=True))
            return 0

        if args.command == "state-import":
            config = OperatorConfig.load(args.operator_config)
            inventory = json.loads(args.inventory.read_text(encoding="utf-8"))
            app = OperatorApplication(config)
            record = app.instances.open(args.instance_id)
            digest, destination = app.instances.import_verified_file(
                record, args.source, inventory, args.source_path, args.name,
            )
            print(json.dumps({"instance_id": args.instance_id, "name": args.name,
                              "sha256": digest, "path": str(destination)}, sort_keys=True))
            return 0

        if args.command == "analysis-acpi-dump":
            value = dump_acpi_tables(include_data=not args.metadata_only,
                                     output_dir=args.output_dir, helper=args.helper)
            print(json.dumps(value, indent=2, sort_keys=True))
            return 0

        if args.command == "analysis-clone-validate":
            value = validate_clone(args.directory, qemu_img=args.qemu_img)
            print(json.dumps(value, indent=2, sort_keys=True))
            return 0

        if args.command == "analysis-observe":
            report = json.loads(args.report.read_text(encoding="utf-8"))
            observed = json.loads(args.observation.read_text(encoding="utf-8"))
            missing = missing_observation_fields(report, observed)
            if missing and not args.allow_incomplete:
                raise ValueError("observation is missing required field(s): " + ", ".join(missing))
            updated = record_observation(report, observed, helper=args.helper)
            args.report.write_text(json.dumps(updated, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            print(json.dumps(updated["guest_observed"], indent=2, sort_keys=True))
            return 0

        if args.command.startswith("analysis-kvm-guard-"):
            config = OperatorConfig.load(args.operator_config)
            app = OperatorApplication(config)
            record = app.open_session(args.instance_id, args.session_id)
            guard = session_from_record(record.session_id, record.runtime_dir,
                                        record.artifact_dir, record.manifest)
            if args.command == "analysis-kvm-guard-load-command":
                value = load_command(guard, args.module, hook_exits=not args.no_hook_exits,
                                     hook_tsc=not args.no_hook_tsc,
                                     hyperv_fast_mode=args.hyperv_fast_mode)
            elif args.command == "analysis-kvm-guard-status":
                value = status(guard, args.stats_path)
            else:
                value = snapshot(guard, args.stats_path)
            print(json.dumps(value, indent=2, sort_keys=True))
            return 0

        if args.command == "session-create":
            config = OperatorConfig.load(args.operator_config)
            app = OperatorApplication(config, release_set=args.release_set, bundle_root=args.bundle_root)
            record = app.create_session(
                args.profile, target=args.target,
                instance_id=args.instance_id, session_id=args.session_id,
            )
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
            record = OperatorApplication(config).open_session(args.instance_id, args.session_id)
            print(record.manifest.read_text(encoding="utf-8"), end="")
            return 0

        if args.command == "session-start":
            command = list(args.exec_command)
            if command and command[0] == "--":
                command = command[1:]
            config = OperatorConfig.load(args.operator_config)
            app = OperatorApplication(config)
            record = app.open_session(args.instance_id, args.session_id)
            launch_plan = None
            if not command:
                command, recorded_qmp = app.recorded_plan(record)
            else:
                recorded_qmp = None
            qmp_socket = args.qmp_socket
            if qmp_socket is None:
                qmp_socket = recorded_qmp
            if qmp_socket is None:
                raise ValueError("session-start requires --qmp-socket when no launch plan endpoint exists")
            running = asyncio.run(app.start_session(record, command, qmp_socket))
            print(json.dumps({"session_id": record.session_id, "pid": running.process.pid}, sort_keys=True))
            return 0

        if args.command == "session-reconcile":
            config = OperatorConfig.load(args.operator_config)
            app = OperatorApplication(config)
            record = app.open_session(args.instance_id, args.session_id)
            state = app.reconcile_session(record)
            print(json.dumps({"session_id": record.session_id, "state": state}, sort_keys=True))
            return 0

        if args.command == "session-stop":
            config = OperatorConfig.load(args.operator_config)
            app = OperatorApplication(config)
            record = app.open_session(args.instance_id, args.session_id)
            exit_code = app.stop_session(record, args.timeout)
            print(json.dumps({"session_id": record.session_id, "exit_code": exit_code}, sort_keys=True))
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
