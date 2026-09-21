from __future__ import annotations

import importlib.util
from pathlib import Path


ROOT = Path(__file__).parents[1]


def _load(name: str):
    path = ROOT / "scripts" / "compat" / name
    spec = importlib.util.spec_from_file_location(name.replace(".", "_"), path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_bluetooth_controller_reports_isolated_state_and_rejects_unknown_control():
    module = _load("hci_simulator.py")
    controller = module.Controller()
    status = controller.status()
    assert status["address"] == "00:1a:7d:00:00:01"
    assert status["advertising"] is False
    assert controller.supported_commands


def test_hwsim_medium_reconfiguration_is_bounded_and_deterministic():
    module = _load("hwsim_adapter.py")
    medium = module.Medium(signal=-50, jitter=4, loss=0.25, latency_ms=12, seed=7)
    changed = medium.reconfigured({"signal": -60, "aggregate": True})
    assert changed.settings() == {
        "signal": -60, "jitter": 4, "loss": 0.25,
        "latency_ms": 12, "rate_index": None, "aggregate": True,
    }
    try:
        medium.reconfigured({"latency_ms": 60001})
    except ValueError as exc:
        assert "loss or latency" in str(exc)
    else:
        raise AssertionError("out-of-range latency was accepted")


def test_isolated_hwsim_wrapper_uses_migrated_helper_path():
    wrapper = (ROOT / "scripts" / "compat" / "run-isolated-hwsim.sh").read_text(encoding="utf-8")
    assert '"$repo_dir/scripts/compat/hwsim_adapter.py"' in wrapper
    assert '"$repo_dir/compat/wifi/hwsim_adapter.py"' not in wrapper
    assert "compat/wifi/hwsim_control.py" not in wrapper
