from pathlib import Path

from machineemu.domains.analysis.acpi import dump_acpi_tables


def _table(signature: bytes, payload: bytes = b"payload") -> bytes:
    data = bytearray(signature + b"\x10\x00\x00\x00\x01\x00" + payload)
    data[9] = (-sum(data)) & 0xFF
    return bytes(data)


def test_acpi_dump_reads_linux_tables_and_reports_checksums(tmp_path: Path, monkeypatch):
    monkeypatch.setattr("platform.system", lambda: "Linux")
    table_dir = tmp_path / "sys/firmware/acpi/tables"
    (table_dir / "dynamic").mkdir(parents=True)
    (table_dir / "DSDT").write_bytes(_table(b"DSDT"))
    (table_dir / "dynamic/SSDT1").write_bytes(_table(b"SSDT", b"dynamic"))

    result = dump_acpi_tables(root=tmp_path)
    assert result["method"] == "sysfs"
    assert result["table_count"] == 2
    assert [item["signature"] for item in result["tables"]] == ["DSDT", "SSDT"]
    assert all(item["checksum_valid"] for item in result["tables"])
    assert all("data_base64" in item for item in result["tables"])


def test_acpi_dump_writes_raw_tables_without_embedding_data(tmp_path: Path, monkeypatch):
    monkeypatch.setattr("platform.system", lambda: "Linux")
    table_dir = tmp_path / "sys/firmware/acpi/tables"
    table_dir.mkdir(parents=True)
    data = _table(b"FACP")
    (table_dir / "FACP").write_bytes(data)
    result = dump_acpi_tables(root=tmp_path, output_dir=tmp_path / "out")
    item = result["tables"][0]
    assert "data_base64" not in item
    assert Path(item["path"]).read_bytes() == data
