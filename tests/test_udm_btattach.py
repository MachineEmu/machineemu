import pytest

from machineemu.domains.unifi.firmware.btattach import HOOK, ORDER, install
from machineemu.domains.unifi.firmware.patches import Entry, read_cpio, write_cpio


def entry(name, data):
    return Entry(name, (1, 0o100644, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0), data)


def test_btattach_preserves_existing_hooks_and_is_idempotent():
    original = write_cpio([entry(ORDER, b'/scripts/init-bottom/existing\n'), entry('keep', b'unchanged')])
    patched = install(original)
    files = {e.name: e for e in read_cpio(patched)[0]}
    assert files['keep'].data == b'unchanged'
    assert files[ORDER].data == b'/scripts/init-bottom/existing\n/' + HOOK.encode() + b'\n'
    assert files[HOOK].mode & 0o111
    assert b'btattach -B /dev/ttyS1 -P h4' in files[HOOK].data
    assert b'UDMPRO' in files[HOOK].data
    assert install(patched) == patched


def test_btattach_rejects_an_archive_without_init_bottom_order():
    with pytest.raises(ValueError, match='ORDER'):
        install(write_cpio([entry('keep', b'unchanged')]))
