"""Capture host ACPI tables as bounded, provenance-friendly artifacts."""

from __future__ import annotations

import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
from typing import Any


def _entry(name: str, source: str, data: bytes, *, include_data: bool) -> dict[str, Any]:
    signature = data[:4].decode("ascii", errors="replace") if len(data) >= 4 else name[:4]
    item: dict[str, Any] = {"name": name, "source": source, "signature": signature,
                            "length": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    if len(data) >= 10:
        item.update({"revision": data[8], "checksum": data[9],
                     "checksum_valid": (sum(data) & 0xFF) == 0})
    if include_data:
        item["data_base64"] = base64.b64encode(data).decode("ascii")
    return item


def _safe_name(name: str, index: int) -> str:
    clean = re.sub(r"[^A-Za-z0-9_.-]+", "_", name).strip("._") or "ACPI"
    return f"{index:03d}-{clean}.bin"


def _linux_tables(root: Path, *, include_data: bool, output_dir: Path | None) -> list[dict[str, Any]]:
    tables: list[dict[str, Any]] = []
    for base in (root / "sys/firmware/acpi/tables", root / "sys/firmware/acpi/tables/dynamic"):
        try:
            paths = sorted(item for item in base.iterdir() if item.is_file())
        except OSError:
            continue
        for path in paths:
            try:
                data = path.read_bytes()
            except OSError:
                continue
            item = _entry(path.name, str(path), data, include_data=include_data and output_dir is None)
            if output_dir is not None:
                output_dir.mkdir(parents=True, exist_ok=True)
                target = output_dir / _safe_name(path.name, len(tables))
                target.write_bytes(data)
                item["path"] = str(target)
            tables.append(item)
    return tables


def dump_acpi_tables(*, root: Path = Path("/"), include_data: bool = True,
                     output_dir: Path | None = None, helper: str | None = None) -> dict[str, Any]:
    """Capture ACPI metadata, optionally writing raw tables outside the repo."""
    helper = helper or os.environ.get("ANALYSIS_PROFILE_HELPER")
    if helper and root == Path("/"):
        args = [helper, "acpi-dump"]
        if not include_data:
            args.append("--metadata-only")
        if output_dir is not None:
            args += ["--output-dir", str(output_dir)]
        result = subprocess.run(args, text=True, capture_output=True, check=False)
        if result.returncode:
            raise ValueError(result.stderr.strip() or "analysis profile helper rejected the ACPI dump")
        try:
            value = json.loads(result.stdout)
        except json.JSONDecodeError as exc:
            raise ValueError("analysis profile helper returned invalid ACPI dump JSON") from exc
        if not isinstance(value, dict):
            raise ValueError("analysis profile helper returned a non-object ACPI dump")
        return value
    system = platform.system() or "Unknown"
    if system == "Linux":
        tables = _linux_tables(root, include_data=include_data, output_dir=output_dir)
        method = "sysfs"
    else:
        tables, method = [], "unsupported"
    return {"schema_version": 1, "kind": "acpi-table-dump", "os": system,
            "method": method, "table_count": len(tables), "tables": tables}
