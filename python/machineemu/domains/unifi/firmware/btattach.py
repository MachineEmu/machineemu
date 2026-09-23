"""Attach the lab H4 controller on ttyS1 from a prepared initramfs.

Adapted from unifi-qemu/compat/bt/install_btattach_hook.py.
"""
from .patches import Entry, read_cpio, replace_file, write_cpio

ORDER = "scripts/init-bottom/ORDER"
HOOK = "scripts/init-bottom/98-qemu-btattach"

SCRIPT = b'''#!/bin/sh
target="${rootmnt:-/root}"
mkdir -p "$target/etc/systemd/system/multi-user.target.wants"
cat > "$target/etc/systemd/system/qemu-btattach.service" <<'UNIT'
[Unit]
Description=Attach the emulated H4 controller on ttyS1
DefaultDependencies=no
After=systemd-udev-settle.service
Before=bluetooth.service

[Service]
Type=simple
ExecStart=/usr/bin/btattach -B /dev/ttyS1 -P h4
ExecStartPost=/bin/sh -c 'sleep 5; /bin/hciconfig hci0 up || /usr/bin/hciconfig hci0 up || true'
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
UNIT
ln -sf ../qemu-btattach.service \\
    "$target/etc/systemd/system/multi-user.target.wants/qemu-btattach.service"
'''



def install(source: bytes) -> bytes:
    entries, _ = read_cpio(source)
    if any(entry.name == HOOK for entry in entries):
        return source
    order = next((entry.data for entry in entries if entry.name == ORDER), None)
    if order is None:
        raise ValueError("prepared initramfs has no scripts/init-bottom/ORDER")
    entries = replace_file(entries, ORDER, order.rstrip(b"\n") + b"\n/" + HOOK.encode() + b"\n")
    script = SCRIPT + b'\nmkdir -p "$target/etc/ubnt/bt"\nprintf "%s\\n" UDMPRO > "$target/etc/ubnt/bt/shortname"\n'
    inode = max(entry.fields[0] for entry in entries) + 1
    entries.append(Entry(HOOK, (inode, 0o100755, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0), script))
    return write_cpio(entries)
