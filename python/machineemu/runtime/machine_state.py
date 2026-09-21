"""Own the writable machine state a session needs before QEMU starts.

The disk, UEFI variables, and TPM state are per-instance: seeded once from
their immutable imported assets, then reused, so a machine remembers what it
has done and anything sealed to its firmware or TPM survives. None of them are
ever written back into the shared asset store. The swtpm control socket is the
one per-session piece.

TPM preparation is ported from the unifi-qemu console.
"""

from __future__ import annotations

from dataclasses import dataclass
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
from typing import Any

from .state import RuntimeStateError

# A directly-started, never-manufactured swtpm state is tiny (~1.3 KiB) and has
# caused Windows to bind ACPI\MSFT0101 but fail with Code 10. swtpm_setup's
# manufactured state is larger and includes EK/PCR initialisation. Anything at
# or above this size is left alone so imported or already-manufactured states
# are preserved.
_MANUFACTURED_STATE_BYTES = 3000
_STARTUP_TIMEOUT = 5.0


@dataclass
class RunningTPM:
    process: subprocess.Popen[bytes]
    socket: Path
    state_dir: Path

    @property
    def pid(self) -> int:
        return self.process.pid

    def stop(self, timeout: float = 5.0) -> int | None:
        if self.process.poll() is not None:
            return self.process.returncode
        self.process.terminate()
        try:
            return self.process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.process.kill()
            return self.process.wait(timeout=timeout)


def seed_nvram(firmware: Any) -> Path | None:
    """Copy the imported UEFI variables into the instance on first use.

    The copy is what QEMU writes to. An existing copy is kept untouched: it
    holds the instance's enrolled keys and boot entries.
    """
    if firmware is None:
        return None
    if not isinstance(firmware, dict):
        raise RuntimeStateError("session firmware metadata must be a mapping")
    nvram = firmware.get("nvram")
    if not isinstance(nvram, dict):
        raise RuntimeStateError("session firmware.nvram metadata must be a mapping")
    target = nvram.get("path")
    seed = nvram.get("seed")
    if not isinstance(target, str) or not target or not isinstance(seed, str) or not seed:
        raise RuntimeStateError("session firmware.nvram needs both path and seed")
    target_path = Path(target)
    if target_path.is_symlink():
        raise RuntimeStateError(f"NVRAM copy is a symbolic link: {target_path}")
    if target_path.exists():
        if not target_path.is_file():
            raise RuntimeStateError(f"NVRAM copy is not a regular file: {target_path}")
        return target_path
    seed_path = Path(seed)
    if not seed_path.is_file():
        raise RuntimeStateError(f"NVRAM seed asset is unavailable: {seed_path}")
    target_path.parent.mkdir(parents=True, exist_ok=True)
    # Publish atomically so a crashed start cannot leave a truncated variable
    # store that the next boot would treat as the instance's enrolled state.
    with tempfile.NamedTemporaryFile(dir=target_path.parent, prefix=f".{target_path.name}.",
                                     delete=False) as temporary:
        temporary_path = Path(temporary.name)
        with seed_path.open("rb") as stream:
            shutil.copyfileobj(stream, temporary)
        temporary.flush()
        os.fsync(temporary.fileno())
    try:
        temporary_path.chmod(0o600)
        os.replace(temporary_path, target_path)
    except OSError as exc:
        temporary_path.unlink(missing_ok=True)
        raise RuntimeStateError(f"cannot publish NVRAM copy {target_path}: {exc}") from exc
    return target_path


def _check_overlay_backing(overlay: Path, backing: Path, qemu_img: str | Path | None) -> None:
    """Refuse an overlay built on a different baseline than the profile names.

    Baselines are content-addressed, so a changed asset is a genuinely
    different image. Booting the old chain would silently ignore the new one.
    """
    image_tool = str(qemu_img or os.environ.get("QEMU_IMG", "qemu-img"))
    result = subprocess.run([image_tool, "info", "--output=json", str(overlay)],
                            capture_output=True, text=True, check=False)
    if result.returncode:
        raise RuntimeStateError(f"cannot inspect disk overlay {overlay}: "
                                f"{result.stderr.strip() or 'qemu-img failed'}")
    try:
        info = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeStateError(f"qemu-img returned invalid metadata for {overlay}") from exc
    recorded = info.get("full-backing-filename") or info.get("backing-filename")
    if not isinstance(recorded, str) or not recorded:
        raise RuntimeStateError(f"disk overlay has no backing image: {overlay}")
    recorded_path = Path(recorded)
    if not recorded_path.is_absolute():
        recorded_path = overlay.parent / recorded_path
    if recorded_path != backing:
        raise RuntimeStateError(
            f"disk overlay {overlay} is backed by {recorded_path}, not {backing}; "
            "the profile's baseline asset changed")


def seed_disk_overlay(storage: Any, qemu_img: str | Path | None = None) -> Path | None:
    """Create the instance's qcow2 overlay over the imported baseline.

    The overlay is what the guest writes to. An existing one is kept: it holds
    everything this machine has done since it was created.
    """
    if storage is None:
        return None
    if not isinstance(storage, dict):
        raise RuntimeStateError("session storage metadata must be a mapping")
    disk = storage.get("disk")
    if not isinstance(disk, dict):
        raise RuntimeStateError("session storage.disk metadata must be a mapping")
    target = disk.get("path")
    backing = disk.get("backing")
    backing_format = disk.get("backing_format", "qcow2")
    if not isinstance(target, str) or not target or not isinstance(backing, str) or not backing:
        raise RuntimeStateError("session storage.disk needs both path and backing")
    if backing_format not in {"raw", "qcow2"}:
        raise RuntimeStateError("session storage.disk.backing_format must be raw or qcow2")
    target_path = Path(target)
    backing_path = Path(backing)
    if target_path.is_symlink():
        raise RuntimeStateError(f"disk overlay is a symbolic link: {target_path}")
    if target_path.exists():
        if not target_path.is_file():
            raise RuntimeStateError(f"disk overlay is not a regular file: {target_path}")
        _check_overlay_backing(target_path, backing_path, qemu_img)
        return target_path
    if not backing_path.is_file():
        raise RuntimeStateError(f"disk baseline is unavailable: {backing_path}")
    target_path.parent.mkdir(parents=True, exist_ok=True)
    image_tool = str(qemu_img or os.environ.get("QEMU_IMG", "qemu-img"))
    result = subprocess.run(
        [image_tool, "create", "-f", "qcow2", "-F", backing_format,
         "-b", str(backing_path), str(target_path)],
        capture_output=True, text=True, check=False)
    if result.returncode:
        raise RuntimeStateError(f"cannot create disk overlay {target_path}: "
                                f"{result.stderr.strip() or 'qemu-img failed'}")
    return target_path


def backend_version(tpm: Any) -> str:
    if not isinstance(tpm, dict):
        raise RuntimeStateError("session TPM metadata must be a mapping")
    backend = tpm.get("backend", {})
    version = backend.get("version", "2.0") if isinstance(backend, dict) else "2.0"
    if version not in {"1.2", "2.0"}:
        raise RuntimeStateError('TPM backend version must be "1.2" or "2.0"')
    return version


def _permanent_state(state_dir: Path) -> Path:
    return state_dir / "tpm2-00.permall"


def _needs_manufacturing(state_dir: Path) -> bool:
    permanent = _permanent_state(state_dir)
    if not permanent.exists():
        return True
    return permanent.stat().st_size < _MANUFACTURED_STATE_BYTES


def _manufacture(state_dir: Path, log_dir: Path) -> None:
    binary = shutil.which("swtpm_setup")
    if binary is None:
        raise RuntimeStateError("TPM 2.0 state requires swtpm_setup but it is not available")
    log = log_dir / "swtpm_setup.log"
    command = [
        binary,
        "--tpm-state", str(state_dir),
        "--tpm2",
        "--createek",
        "--decryption",
        "--lock-nvram",
        "--logfile", str(log),
        "--overwrite" if _permanent_state(state_dir).exists() else "--not-overwrite",
    ]
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if result.returncode != 0:
        detail = (result.stderr or result.stdout or "").strip()
        raise RuntimeStateError(f"swtpm_setup failed; see {log}: {detail}")


def start_tpm(tpm: Any, log_dir: Path) -> RunningTPM:
    """Manufacture state when needed and start swtpm on the session socket.

    `tpm` is the launch plan's recorded TPM block, so the socket and state
    directory are exactly the ones the QEMU argv already names.
    """
    version = backend_version(tpm)
    socket_value = tpm.get("socket")
    state_value = tpm.get("state")
    if not isinstance(socket_value, str) or not socket_value:
        raise RuntimeStateError("session TPM metadata has no control socket")
    if not isinstance(state_value, str) or not state_value:
        raise RuntimeStateError("session TPM metadata has no state directory")
    socket = Path(socket_value)
    state_dir = Path(state_value)
    binary = shutil.which("swtpm")
    if binary is None:
        raise RuntimeStateError("TPM is configured but swtpm is not available")
    state_dir.mkdir(parents=True, exist_ok=True)
    socket.parent.mkdir(parents=True, exist_ok=True)
    log_dir.mkdir(parents=True, exist_ok=True)
    if socket.exists():
        raise RuntimeStateError(f"TPM socket is already present: {socket}")
    if version == "2.0" and _needs_manufacturing(state_dir):
        _manufacture(state_dir, log_dir)
    log = log_dir / "swtpm.log"
    command = [
        binary, "socket",
        "--tpmstate", f"dir={state_dir},mode=0600,lock",
        "--ctrl", f"type=unixio,path={socket},mode=0600",
        "--log", f"file={log},level=20",
    ]
    if version == "2.0":
        command.append("--tpm2")
    command.append("--terminate")
    process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    running = RunningTPM(process, socket, state_dir)
    deadline = time.monotonic() + _STARTUP_TIMEOUT
    while time.monotonic() < deadline and not socket.exists():
        if process.poll() is not None:
            detail = process.stderr.read().decode(errors="replace").strip() if process.stderr else ""
            raise RuntimeStateError(f"swtpm failed to start; see {log}: {detail}")
        time.sleep(0.02)
    if not socket.exists():
        running.stop(timeout=1.0)
        raise RuntimeStateError(f"swtpm did not create its session socket; see {log}")
    if process.poll() is not None:
        raise RuntimeStateError(f"swtpm exited during startup; see {log}")
    return running
