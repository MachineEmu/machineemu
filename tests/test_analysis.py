from pathlib import Path

import pytest

from machineemu.domains.analysis import build_identity, dump_host_profile
from machineemu.domains.analysis.verify import missing_observation_fields, record_observation


def test_analysis_identity_is_deterministic_and_uuid_like():
    first = build_identity("seed")
    assert first == build_identity("seed")
    assert first["uuid"].count("-") == 4
    assert first["mac"].startswith("02:")
    assert first["serials"]["processor"].startswith("AN-PROCESSOR-")
    assert build_identity("seed", "clone")["uuid"] != first["uuid"]
    with pytest.raises(ValueError, match="safe identifier"):
        build_identity("seed", "../clone")


def test_analysis_observation_normalizes_guest_values():
    report = {
        "identity": {"uuid": "A", "mac": "02:00:00:00:00:01", "serials": {"processor": "P"}},
        "machine": {"qemu_cpu": "max,model-id=Example CPU,-hypervisor"},
        "smbios": {"bios_vm": False, "processor_socket_prefix": "CPU", "processor_max_speed": 5000},
        "acpi": {"oem_id": "ANALY", "creator_revision": 1},
        "remaining_detectable_signals": ["timing"],
    }
    observed = {"uuid": "a", "mac": "02-00-00-00-00-01", "serials": {"processor": "P"},
                "cpu_name": "Example CPU", "hypervisor_present": False, "bios_vm": False,
                "processor_socket": "CPU", "processor_max_speed": "0x1388",
                "acpi": {"oem_id": "ANALY ", "creator_revision": "1"}}
    result = record_observation(report, observed)
    assert result["guest_observed"]["passed"] is True
    assert result["guest_observed"]["checks"]["acpi.oem_id"]["match"] is True


def test_analysis_missing_fields_are_explicit():
    report = {"identity": {"uuid": "A", "mac": "M", "serials": {"processor": "P"}},
              "machine": {"qemu_cpu": "max,model-id=Example"},
              "smbios": {"bios_vm": False, "processor_socket_prefix": "CPU"},
              "acpi": {"oem_id": "ANALY"}}
    missing = missing_observation_fields(report, {"uuid": "A", "mac": "M"})
    assert {"hypervisor_present", "serials.processor", "cpu_name", "bios_vm",
            "processor_socket", "acpi.oem_id"}.issubset(missing)


def test_host_profile_reads_only_the_supplied_linux_tree(tmp_path: Path, monkeypatch):
    monkeypatch.setattr("platform.system", lambda: "Linux")
    monkeypatch.setattr("platform.release", lambda: "6.10-test")
    monkeypatch.setattr("platform.machine", lambda: "x86_64")
    monkeypatch.setattr("platform.node", lambda: "analysis-host")
    (tmp_path / "proc").mkdir()
    (tmp_path / "sys/class/dmi/id").mkdir(parents=True)
    (tmp_path / "sys/block/nvme0n1/device").mkdir(parents=True)
    (tmp_path / "sys/class/hwmon/hwmon0").mkdir(parents=True)
    (tmp_path / "proc/cpuinfo").write_text("model name: Example CPU\n", encoding="utf-8")
    (tmp_path / "proc/meminfo").write_text("MemTotal: 16777216 kB\n", encoding="utf-8")
    (tmp_path / "sys/class/dmi/id/sys_vendor").write_text("Example Systems\n", encoding="utf-8")
    (tmp_path / "sys/class/dmi/id/product_name").write_text("Example PC\n", encoding="utf-8")
    (tmp_path / "sys/block/nvme0n1/device/model").write_text("Example SSD\n", encoding="utf-8")
    (tmp_path / "sys/class/hwmon/hwmon0/temp1_input").write_text("43000\n", encoding="utf-8")
    profile = dump_host_profile("seed", root=tmp_path)
    assert profile["analysis"]["smbios"]["system_manufacturer"] == "Example Systems"
    assert profile["analysis"]["device_descriptors"]["storage"]["disk_product"] == "Example SSD"
    assert profile["analysis"]["sensors"]["temperature_celsius"] == 43
    assert profile["memory"] == {"value": 16, "unit": "GiB"}
