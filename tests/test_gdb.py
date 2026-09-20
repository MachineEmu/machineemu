import pytest

from machineemu.runtime.gdb import gdb_target, parse_mi


def test_gdb_target_accepts_owned_endpoint_shapes():
    assert gdb_target({"transport": "tcp", "host": "127.0.0.1", "port": 1234}) == "127.0.0.1:1234"
    assert gdb_target({"transport": "unix", "path": "/tmp/gdb.sock"}) == "/tmp/gdb.sock"


def test_gdb_target_rejects_untrusted_metadata():
    with pytest.raises(ValueError):
        gdb_target({"transport": "tcp", "host": "127.0.0.1", "port": "1234"})
    with pytest.raises(ValueError):
        gdb_target({"transport": "serial"})


def test_parse_mi_preserves_stream_and_result_frames():
    assert parse_mi('~"hello\\n"') == {"type": "gdb.output", "stream": "console", "text": "hello\n"}
    assert parse_mi('4^done,msg="ok"') == {
        "type": "gdb.result", "token": "4", "status": "done", "message": "ok", "payload": 'msg="ok"',
    }
