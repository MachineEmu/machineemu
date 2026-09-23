#!/usr/bin/env python3
"""Run one UDM Pro display or front-panel hub for a MachineEmu instance."""
import argparse
import asyncio
import json
from pathlib import Path
import signal

from front_panel import FrontPanelHub
from lcm import LcmHub


async def serve(kind: str, runtime: Path, profile: Path | None) -> None:
    runtime.mkdir(parents=True, exist_ok=True)
    if kind == "lcm":
        hub = LcmHub(runtime, runtime)
    else:
        resolved = json.loads(profile.read_text()) if profile else None
        if resolved is not None:
            resolved["adapter"] = resolved.get("machine")
            network = resolved.get("network", {})
            for backend in network.get("ports", [network]):
                if isinstance(backend, dict) and "type" in backend:
                    backend["mode"] = backend["type"]
        hub = FrontPanelHub(runtime, runtime, resolved)
    await hub.start()
    stopped = asyncio.Event()
    loop = asyncio.get_running_loop()
    for signum in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(signum, stopped.set)
    try:
        await stopped.wait()
    finally:
        await hub.close()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("kind", choices=("lcm", "frontpanel"))
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--profile", type=Path)
    args = parser.parse_args()
    asyncio.run(serve(args.kind, args.runtime, args.profile))


if __name__ == "__main__":
    main()
