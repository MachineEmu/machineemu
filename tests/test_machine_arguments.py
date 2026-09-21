from pathlib import Path

import pytest

from machineemu.profiles import ProfileError
from machineemu.profiles.machine import (accelerator_arguments, disk_plan, firmware_arguments,
                                         memory_argument, nvram_plan, smp_argument,
                                         storage_arguments, tpm_arguments)

ASSETS = {"disk": Path("/store/disk"), "code": Path("/store/code"), "vars": Path("/store/vars")}
STATE = Path("/state/instances/one")


@pytest.mark.parametrize("value,expected", [
    ("8GiB", "8G"), ("8G", "8G"), ("512MiB", "512M"), ("2048M", "2048M"),
    ("1TiB", "1T"), ("4096", "4096M"), (4096, "4096M"),
])
def test_memory_sizes_are_normalised_to_suffixes_qemu_accepts(value, expected):
    assert memory_argument(value) == expected


@pytest.mark.parametrize("value", ["", "8GB", "eight", "0GiB", "-1G", None, True])
def test_memory_rejects_sizes_qemu_cannot_parse(value):
    with pytest.raises(ProfileError):
        memory_argument(value)


def test_smp_renders_the_declared_topology():
    resources = {"topology": {"sockets": 1, "cores": 4, "threads": 2}}
    assert smp_argument(resources, 8) == "8,sockets=1,cores=4,threads=2"
    assert smp_argument({}, 4) == "4"


def test_smp_rejects_a_non_positive_topology_value():
    with pytest.raises(ProfileError, match="cores"):
        smp_argument({"topology": {"cores": 0}}, 4)


def test_accelerator_renders_kvm_directly_and_tcg_with_a_thread_mode():
    assert accelerator_arguments({"accelerator": "kvm"}) == ["-accel", "kvm"]
    assert accelerator_arguments({"accelerator": "tcg"}) == ["-accel", "tcg,thread=multi"]
    assert accelerator_arguments({"accelerator": "tcg", "accelerator_thread": "single"}) == [
        "-accel", "tcg,thread=single"]
    assert accelerator_arguments({}) == []
    with pytest.raises(ProfileError):
        accelerator_arguments({"accelerator": "whpx"})


def test_firmware_runs_against_a_per_instance_nvram_copy_not_the_asset():
    arguments = firmware_arguments({
        "loader": {"asset": "code", "readonly": True, "secure": True},
        "nvram": {"asset": "vars"},
    }, ASSETS, STATE)
    assert arguments == [
        "-drive", "if=pflash,format=raw,unit=0,readonly=on,file=/store/code",
        "-drive", "if=pflash,format=raw,unit=1,file=/state/instances/one/OVMF_VARS.fd",
        "-global", "driver=cfi.pflash01,property=secure,value=on",
    ]
    assert str(ASSETS["vars"]) not in " ".join(arguments)


def test_nvram_plan_names_the_copy_and_the_asset_that_seeds_it():
    plan = nvram_plan({"loader": {"asset": "code"}, "nvram": {"asset": "vars"}},
                      ASSETS, STATE)
    assert plan == {"path": Path("/state/instances/one/OVMF_VARS.fd"), "seed": Path("/store/vars")}
    assert nvram_plan(None, ASSETS, STATE) is None


def test_nvram_name_cannot_escape_the_instance_directory():
    with pytest.raises(ProfileError, match="plain file name"):
        nvram_plan({"loader": {"asset": "code"}, "nvram": {"asset": "vars", "name": "../escape.fd"}},
                   ASSETS, STATE)


def test_firmware_requires_both_halves_so_vars_cannot_be_attached_alone():
    with pytest.raises(ProfileError, match="loader and nvram"):
        firmware_arguments({"nvram": {"asset": "vars"}}, ASSETS, STATE)


def test_firmware_rejects_an_asset_that_was_never_imported():
    with pytest.raises(ProfileError, match="not imported"):
        firmware_arguments({"loader": {"asset": "missing"}, "nvram": {"asset": "vars"}},
                           ASSETS, STATE)


def test_tpm_renders_the_emulator_backend_against_its_session_socket():
    arguments = tpm_arguments({"model": "tpm-crb", "backend": {"type": "emulator", "version": "2.0"}},
                              Path("/run/tpm.sock"))
    assert arguments == [
        "-chardev", "socket,id=chrtpm,path=/run/tpm.sock",
        "-tpmdev", "emulator,id=tpm0,chardev=chrtpm",
        "-device", "tpm-crb,tpmdev=tpm0",
    ]
    assert tpm_arguments(None, Path("/run/tpm.sock")) == []


def test_tpm_rejects_a_passthrough_backend_and_an_unknown_model():
    with pytest.raises(ProfileError, match="emulator"):
        tpm_arguments({"backend": {"type": "passthrough"}}, Path("/run/tpm.sock"))
    with pytest.raises(ProfileError, match="tpm-tis or tpm-crb"):
        tpm_arguments({"model": "tpm-spapr"}, Path("/run/tpm.sock"))


def test_storage_boots_a_per_instance_overlay_not_the_shared_baseline():
    arguments = storage_arguments(
        {"disk": {"asset": "disk", "format": "qcow2", "bus": "sata", "discard": "unmap"}},
        ASSETS, "pc-q35-10.1",
        {"device_descriptors": {"storage": {"disk_product": "SATA SSD", "disk_serial_prefix": "ANSSD"}}},
        STATE)
    assert arguments == [
        "-device", "ich9-ahci,id=pc-sata",
        "-drive", "if=none,id=pc-disk,file=/state/instances/one/overlay.qcow2,format=qcow2,discard=unmap",
        "-device", "ide-hd,bus=pc-sata.0,drive=pc-disk,model=SATA SSD,serial=ANSSD-0",
    ]
    assert str(ASSETS["disk"]) not in " ".join(arguments)


def test_disk_plan_names_the_overlay_and_the_baseline_behind_it():
    plan = disk_plan({"disk": {"asset": "disk", "format": "raw"}}, ASSETS, STATE)
    assert plan == {"path": Path("/state/instances/one/overlay.qcow2"),
                    "backing": Path("/store/disk"), "backing_format": "raw"}
    assert disk_plan(None, ASSETS, STATE) is None


def test_storage_uses_the_legacy_ide_bus_on_the_pc_machine():
    arguments = storage_arguments({"disk": {"asset": "disk", "format": "raw", "bus": "ide"}},
                                  ASSETS, "pc", None, STATE)
    assert arguments == [
        "-drive", "if=none,id=pc-disk,file=/state/instances/one/overlay.qcow2,format=qcow2",
        "-device", "ide-hd,bus=ide.0,drive=pc-disk",
    ]


def test_storage_rejects_a_bus_the_machine_does_not_provide():
    with pytest.raises(ProfileError, match="sata requires q35"):
        storage_arguments({"disk": {"asset": "disk", "bus": "sata"}}, ASSETS, "pc", None, STATE)
    with pytest.raises(ProfileError, match="ide requires the pc machine"):
        storage_arguments({"disk": {"asset": "disk", "bus": "ide"}}, ASSETS, "q35", None, STATE)


def test_storage_keeps_analysis_descriptors_off_buses_that_cannot_answer_them():
    arguments = storage_arguments(
        {"disk": {"asset": "disk", "bus": "virtio"}}, ASSETS, "q35",
        {"device_descriptors": {"storage": {"disk_product": "SATA SSD", "disk_serial_prefix": "ANSSD"}}},
        STATE)
    assert arguments[-1] == "virtio-blk-pci,drive=pc-disk,id=pc-disk-device"
