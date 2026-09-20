"""Compare guest-observed analysis values with a generated report."""
from __future__ import annotations

from copy import deepcopy
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
from typing import Any


def _norm_mac(value: object) -> str | None:
    return value.replace("-", ":").lower() if isinstance(value, str) else None


def _norm_text(value: object) -> str | None:
    return value.rstrip("\x00 ") if isinstance(value, str) else None


def _norm_int(value: object) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str):
        try:
            return int(value, 0)
        except ValueError:
            return None
    return None


def _observed_acpi(observed: dict[str, Any], key: str) -> object:
    acpi = observed.get("acpi")
    return acpi[key] if isinstance(acpi, dict) and key in acpi else observed.get(f"acpi_{key}", observed.get(key))


def missing_observation_fields(report: dict[str, Any], observed: dict[str, Any]) -> list[str]:
    missing: list[str] = []
    for key in ("uuid", "mac", "hypervisor_present"):
        if observed.get(key) is None:
            missing.append(key)
    identity = report.get("identity", {})
    serials = identity.get("serials", {}) if isinstance(identity, dict) else {}
    observed_serials = observed.get("serials")
    if isinstance(serials, dict):
        for key in ("system", "board", "chassis", "processor", "memory"):
            if key in serials and (not isinstance(observed_serials, dict) or observed_serials.get(key) is None):
                missing.append(f"serials.{key}")
    machine = report.get("machine", {})
    cpu = machine.get("qemu_cpu", machine.get("cpu")) if isinstance(machine, dict) else None
    if isinstance(cpu, str) and "model-id=" in cpu and observed.get("cpu_name") is None:
        missing.append("cpu_name")
    smbios = report.get("smbios", {})
    if isinstance(smbios, dict):
        fields = ("bios_vendor", "bios_version", "system_manufacturer", "system_product", "system_version",
                  "board_manufacturer", "board_product", "board_version", "chassis_manufacturer", "chassis_version",
                  "chassis_asset", "chassis_sku", "processor_manufacturer", "processor_version", "processor_asset",
                  "processor_part", "processor_max_speed", "processor_current_speed", "memory_manufacturer",
                  "memory_bank", "memory_asset", "memory_part", "memory_speed")
        missing.extend(key for key in fields if key in smbios and observed.get(key) is None)
        if "processor_socket_prefix" in smbios and observed.get("processor_socket") is None:
            missing.append("processor_socket")
        if "memory_locator_prefix" in smbios and observed.get("memory_locator") is None:
            missing.append("memory_locator")
        if "bios_vm" in smbios and observed.get("bios_vm") is None:
            missing.append("bios_vm")
    acpi = report.get("acpi", {})
    if isinstance(acpi, dict):
        missing.extend(f"acpi.{key}" for key in ("oem_id", "oem_table_id", "creator_id", "oem_revision", "creator_revision")
                       if key in acpi and _observed_acpi(observed, key) is None)
    return missing


def _helper_record_observation(helper: str, report: dict[str, Any], observed: dict[str, Any]) -> dict[str, Any]:
    with TemporaryDirectory() as directory:
        root = Path(directory)
        report_path, observed_path = root / "environment.json", root / "observed.json"
        report_path.write_text(json.dumps(report), encoding="utf-8")
        observed_path.write_text(json.dumps(observed), encoding="utf-8")
        result = subprocess.run([helper, "record-observation", str(report_path), str(observed_path)],
                                capture_output=True, text=True, check=False)
    if result.returncode:
        raise ValueError(result.stderr.strip() or "analysis helper rejected the observation")
    value = json.loads(result.stdout)
    if not isinstance(value, dict) or not isinstance(value.get("guest_observed"), dict):
        raise ValueError("analysis helper returned an incomplete observation report")
    return value


def record_observation(report: dict[str, Any], observed: dict[str, Any], helper: str | None = None) -> dict[str, Any]:
    helper = helper or os.environ.get("ANALYSIS_PROFILE_HELPER")
    if helper:
        return _helper_record_observation(helper, report, observed)
    result = deepcopy(report)
    identity = report.get("identity", {})
    checks: dict[str, dict[str, object]] = {}

    def check(name: str, expected: object, actual: object) -> None:
        checks[name] = {"expected": expected, "observed": actual, "match": expected == actual}

    check("uuid", str(identity.get("uuid", "")).lower(), str(observed.get("uuid", "")).lower())
    check("mac", _norm_mac(identity.get("mac")), _norm_mac(observed.get("mac")))
    serials, observed_serials = identity.get("serials", {}), observed.get("serials", {})
    if isinstance(serials, dict) and isinstance(observed_serials, dict):
        for key in ("system", "board", "chassis", "processor", "memory"):
            if key in serials:
                check(f"serial.{key}", serials[key], observed_serials.get(key))
    machine = report.get("machine", {})
    cpu = machine.get("qemu_cpu", machine.get("cpu")) if isinstance(machine, dict) else None
    if isinstance(cpu, str) and "model-id=" in cpu:
        check("cpu.model_id", cpu.split("model-id=", 1)[1].split(",", 1)[0], observed.get("cpu_name"))
    check("cpuid.hypervisor", False, observed.get("hypervisor_present"))
    smbios = report.get("smbios", {})
    if isinstance(smbios, dict):
        for key, expected in smbios.items():
            if key in {"processor_socket_prefix", "memory_locator_prefix"}:
                check(f"smbios.{key.removesuffix('_prefix')}", expected,
                      observed.get(key.removesuffix("_prefix")))
            elif key in {"processor_max_speed", "processor_current_speed", "memory_speed"}:
                check(f"smbios.{key}", _norm_int(expected), _norm_int(observed.get(key)))
            elif key in {"bios_vm", "bios_vendor", "bios_version", "system_manufacturer", "system_product",
                         "system_version", "board_manufacturer", "board_product", "board_version",
                         "chassis_manufacturer", "chassis_version", "chassis_asset", "chassis_sku",
                         "processor_manufacturer", "processor_version", "processor_asset", "processor_part",
                         "memory_manufacturer", "memory_bank", "memory_asset", "memory_part"}:
                check(f"smbios.{key}", expected, observed.get(key))
    acpi = report.get("acpi", {})
    if isinstance(acpi, dict):
        for key, expected in acpi.items():
            check(f"acpi.{key}", _norm_int(expected) if key.endswith("revision") else _norm_text(expected),
                  _norm_int(_observed_acpi(observed, key)) if key.endswith("revision") else _norm_text(_observed_acpi(observed, key)))
    passed = all(item["match"] for item in checks.values())
    result["guest_observed"] = {
        "schema_version": 1, "collected_at": datetime.now(timezone.utc).isoformat(),
        "checks": checks, "passed": passed,
        "remaining_detectable_signals": [] if passed else result.get("remaining_detectable_signals", []),
        "checks_version": "analysis-guest-checks-1",
    }
    return result
