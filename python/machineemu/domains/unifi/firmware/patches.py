"""Patch archive entries in memory; never create host links or device nodes."""
from __future__ import annotations

import posixpath
import secrets
import shutil
import stat
import subprocess
from dataclasses import dataclass, replace

from .formats import MAX_PAYLOAD
from .models import FirmwareError


@dataclass(frozen=True)
class Entry:
    name: str
    fields: tuple[int, ...]
    data: bytes

    @property
    def mode(self) -> int:
        return self.fields[1]


def guest_path(name: str) -> str:
    if name.startswith('/') or '..' in name.split('/') or '\0' in name:
        raise FirmwareError('unsafe archive path')
    return posixpath.normpath(name)


def read_cpio(data: bytes, start: int = 0, *, allow_root_clamped_links: bool = False) -> tuple[list[Entry], int]:
    entries = []
    seen: dict[str, Entry] = {}
    off = start
    while off - start <= MAX_PAYLOAD and len(entries) < 100000:
        if data[off:off + 6] != b'070701' or off + 110 > len(data):
            raise FirmwareError('unsupported or truncated newc archive')
        try:
            fields = tuple(int(data[i:i + 8], 16) for i in range(off + 6, off + 110, 8))
        except ValueError as exc:
            raise FirmwareError('invalid newc header') from exc
        length, name_length = fields[6], fields[11]
        name_start = off + 110
        name_end = name_start + name_length
        if not 1 <= name_length <= 4096 or name_end > len(data) or data[name_end - 1] != 0:
            raise FirmwareError('invalid newc name')
        name = guest_path(data[name_start:name_end - 1].decode('utf-8'))
        payload = start + ((name_end - start + 3) & ~3)
        off = start + ((payload - start + length + 3) & ~3)
        if off > len(data) or off - start > MAX_PAYLOAD:
            raise FirmwareError('newc payload exceeds bounds')
        if name == 'TRAILER!!!':
            if length:
                raise FirmwareError('invalid newc trailer')
            return entries, off
        entry = Entry(name, fields, data[payload:payload + length])
        if name in seen:
            old = seen[name]
            # Linux's generated initramfs repeats directories/device nodes.
            # Preserve those records, but reject conflicting or file aliases.
            same = all(old.fields[i] == fields[i] for i in (1, 2, 3, 9, 10))
            if not same or entry.data or not (stat.S_ISDIR(entry.mode) or stat.S_ISCHR(entry.mode)):
                raise FirmwareError('conflicting duplicate archive path')
        seen[name] = entry
        if stat.S_ISLNK(entry.mode):
            target = entry.data.rstrip(b'\0').decode('utf-8')
            # Absolute guest links are valid. Relative links may not traverse
            # above the guest root. No link is ever followed on the host.
            resolved = posixpath.normpath(posixpath.join(posixpath.dirname(name), target))
            # Some verified vendor archives contain bin/find -> ../../bin/busybox.
            # Guest VFS clamps .. at /. Preserve only for explicit in-memory
            # recipes; never extract these links onto the host filesystem.
            if '\0' in target or (not allow_root_clamped_links and
                                  (resolved == '..' or resolved.startswith('../'))):
                raise FirmwareError('archive link escapes guest root')
        entries.append(entry)
    raise FirmwareError('newc archive exceeds limits')


def write_cpio(entries: list[Entry]) -> bytes:
    output = bytearray()
    trailer = Entry('TRAILER!!!', (0,) * 13, b'')
    for entry in [*entries, trailer]:
        fields = list(entry.fields)
        name = entry.name.encode() + b'\0'
        fields[6], fields[11], fields[12] = len(entry.data), len(name), 0
        output += b'070701' + ''.join(f'{v:08x}' for v in fields).encode()
        output += name
        output += b'\0' * (-len(output) % 4)
        output += entry.data
        output += b'\0' * (-len(output) % 4)
    return bytes(output)


def replace_file(entries: list[Entry], path: str, content: bytes) -> list[Entry]:
    path = guest_path(path)
    matches = [e for e in entries if e.name == path]
    if len(matches) != 1 or not stat.S_ISREG(matches[0].mode) or matches[0].fields[4] != 1:
        raise FirmwareError('patch target must be an existing regular file without hardlinks')
    return [replace(e, data=content) if e.name == path else e for e in entries]


def password_hash(password: str, scheme: str) -> str:
    if scheme not in {'1', '5', '6'} or '\n' in password or '\0' in password or '\r' in password:
        raise FirmwareError('unsupported password scheme or control character')
    binary = shutil.which('openssl')
    if binary is None:
        raise FirmwareError('openssl is required for password hashing')
    salt = ''.join(secrets.choice('abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789./')
                   for _ in range(8 if scheme == '1' else 16))
    result = subprocess.run([binary, 'passwd', '-' + scheme, '-salt', salt, '-stdin'],
                            input=password + '\n', text=True, capture_output=True, timeout=15, check=False)
    # Do not expose child output or argv in errors; those may contain secrets.
    if result.returncode or not result.stdout.startswith('$' + scheme + '$'):
        raise FirmwareError('password hashing failed')
    return result.stdout.strip()


def set_passwords(entries: list[Entry], passwords: tuple[tuple[str, str], ...],
                  passwd_path: str, shadow_path: str | None = None,
                  verified_scheme: str | None = None) -> tuple[list[Entry], list[dict[str, str]]]:
    files = {e.name: e for e in entries}
    if passwd_path not in files:
        raise FirmwareError('unsupported guest passwd layout')
    accounts = [line.split(':') for line in files[passwd_path].data.decode().splitlines()]
    patches = []
    for user, password in passwords:
        if not user or any(c in user for c in ':\n\r\0'):
            raise FirmwareError('invalid password target account')
        rows = [r for r in accounts if r[0] == user]
        if len(rows) != 1 or len(rows[0]) != 7:
            raise FirmwareError('missing or invalid guest account')
        path = shadow_path if rows[0][1] == 'x' else passwd_path
        if path is None or path not in files:
            raise FirmwareError('unsupported guest shadow layout')
        content = files[path].data.decode()
        lines = content.splitlines(keepends=True)
        matching = [i for i, line in enumerate(lines) if line.split(':', 1)[0] == user]
        if len(matching) != 1:
            raise FirmwareError('missing or duplicate password field')
        i = matching[0]
        fields = lines[i].split(':')
        if len(fields) < 2:
            raise FirmwareError('invalid password field')
        old = fields[1].lstrip('!')
        scheme = old.split('$')[1] if old.startswith('$') else ''
        if not scheme and old in {'', '*', 'x'} and verified_scheme:
            scheme = verified_scheme
        if scheme not in {'1', '5', '6'}:
            raise FirmwareError('guest password hash scheme is not verified')
        fields[1] = password_hash(password, scheme)
        lines[i] = ':'.join(fields)
        entries = replace_file(entries, path, ''.join(lines).encode())
        files = {e.name: e for e in entries}
        patches.append({'type': 'set-password', 'account': user, 'path': path, 'revision': '1'})
    return entries, patches
