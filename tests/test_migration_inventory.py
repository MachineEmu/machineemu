import pytest

from machineemu.runtime import RuntimeStateError, inventory_json, inventory_tree, validate_inventory


def test_inventory_is_stable_and_content_addressed(tmp_path):
    (tmp_path / "disk.img").write_bytes(b"disk")
    (tmp_path / "nested").mkdir()
    (tmp_path / "nested/config").write_text("config", encoding="utf-8")
    report = inventory_json(tmp_path)
    assert report["file_count"] == 2
    assert [item["path"] for item in report["files"]] == ["disk.img", "nested/config"]
    assert report["files"][0]["sha256"].startswith("sha256:")


def test_inventory_rejects_symlinks(tmp_path):
    source = tmp_path / "source"
    source.write_text("source", encoding="utf-8")
    link = tmp_path / "link"
    try:
        link.symlink_to(source)
    except OSError:
        pytest.skip("symlinks unavailable")
    with pytest.raises(RuntimeStateError, match="symlink"):
        inventory_tree(tmp_path)


def test_inventory_validation_detects_source_changes(tmp_path):
    (tmp_path / "disk.img").write_bytes(b"disk")
    captured = inventory_json(tmp_path)
    assert validate_inventory(tmp_path, captured)["valid"] is True
    (tmp_path / "disk.img").write_bytes(b"changed")
    result = validate_inventory(tmp_path, captured)
    assert result["valid"] is False
    assert "differ" in result["errors"][0]
