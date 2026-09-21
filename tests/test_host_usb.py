from pathlib import Path

from machineemu.runtime.host_usb import list_host_usb


def test_host_usb_inventory_excludes_root_hubs_and_interfaces(tmp_path: Path, monkeypatch):
    sysfs = tmp_path / "sys"
    dev = tmp_path / "dev" / "001"
    dev.mkdir(parents=True)
    node = dev / "002"
    node.write_bytes(b"")
    monkeypatch.setattr(Path, "is_char_device", lambda path: path == node)

    root_hub = sysfs / "1-0"
    root_hub.mkdir(parents=True)
    (root_hub / "busnum").write_text("1\n")
    (root_hub / "devnum").write_text("1\n")
    (root_hub / "idVendor").write_text("1d6b\n")

    device = sysfs / "1-2"
    device.mkdir(parents=True)
    for name, value in {"busnum": "1", "devnum": "2", "idVendor": "abcd",
                        "idProduct": "1234", "manufacturer": "Lab", "product": "Probe"}.items():
        (device / name).write_text(value + "\n")
    (sysfs / "1-2:1.0").mkdir()

    result = list_host_usb(sysfs_root=sysfs, dev_root=tmp_path / "dev")
    assert result == [{
        "id": "usb-1-2", "kind": "host", "hostbus": 1, "hostaddr": 2,
        "vendor_id": "abcd", "product_id": "1234", "manufacturer": "Lab",
        "product": "Probe", "serial": None,
    }]
