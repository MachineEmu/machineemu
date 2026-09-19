"""Bounded readers for the container formats verified in this repository."""
from __future__ import annotations

import hashlib
import lzma
import struct
import zlib
from dataclasses import dataclass
from pathlib import Path

from .models import FirmwareError

MAX_PAYLOAD = 256 * 1024 * 1024


@dataclass(frozen=True)
class Section:
    name: str
    kind: str
    offset: int
    size: int

    def read(self, source: Path) -> bytes:
        if self.size > MAX_PAYLOAD:
            raise FirmwareError('payload exceeds reader limit')
        with source.open('rb') as stream:
            stream.seek(self.offset)
            data = stream.read(self.size)
        if len(data) != self.size:
            raise FirmwareError('truncated payload')
        return data


def container(source: Path) -> tuple[str, list[Section]]:
    size = source.stat().st_size
    with source.open('rb') as stream:
        header = stream.read(0x10c)
        if len(header) != 0x10c or header[:4] not in {b'UBNT', b'GEOS', b'OPEN'}:
            raise FirmwareError('unsupported firmware container')
        if zlib.crc32(header[:0x104]) != struct.unpack_from('>I', header, 0x104)[0]:
            raise FirmwareError('container header CRC mismatch')
        version = header[4:0x104].split(b'\0')[0].decode('ascii')
        sections = []
        while len(sections) < 64:
            start = stream.tell()
            magic = stream.read(4)
            if magic == b'ENDS':
                if size - start != 264 or stream.read(260)[-4:] != b'\0' * 4:
                    raise FirmwareError('invalid ENDS signature trailer')
                return version, sections
            if magic not in {b'PART', b'EMMC'}:
                raise FirmwareError('unsupported container record or missing ENDS trailer')
            record = magic + stream.read(52)
            if len(record) != 56:
                raise FirmwareError('truncated container record')
            name = record[4:20].split(b'\0')[0].decode('ascii')
            length, capacity = struct.unpack_from('>II', record, 48)
            if not name or any(s.name == name for s in sections) or length > capacity or start + 64 + length > size:
                raise FirmwareError('invalid or duplicate container section')
            crc = zlib.crc32(record)
            remaining = length
            while remaining:
                chunk = stream.read(min(remaining, 1024 * 1024))
                if not chunk:
                    raise FirmwareError('truncated container data')
                crc = zlib.crc32(chunk, crc)
                remaining -= len(chunk)
            trailer = stream.read(8)
            if len(trailer) != 8 or crc != struct.unpack_from('>I', trailer)[0]:
                raise FirmwareError(f'container section CRC mismatch: {name}')
            sections.append(Section(name, magic.decode(), start + 56, length))
    raise FirmwareError('too many container records')


def decompress(data: bytes, compression: str) -> bytes:
    try:
        if compression == 'none':
            result = data
        elif compression == 'lzma':
            decoder = lzma.LZMADecompressor(format=lzma.FORMAT_ALONE, memlimit=MAX_PAYLOAD)
            result = decoder.decompress(data, max_length=MAX_PAYLOAD + 1)
            if not decoder.eof or decoder.unused_data:
                raise FirmwareError('incomplete, oversized, or trailing LZMA data')
        elif compression == 'gzip':
            decoder = zlib.decompressobj(31)
            result = decoder.decompress(data, MAX_PAYLOAD + 1)
            if not decoder.eof or decoder.unused_data:
                raise FirmwareError('incomplete, oversized, or trailing gzip data')
        else:
            raise FirmwareError(f'unsupported compression: {compression}')
    except (lzma.LZMAError, zlib.error) as exc:
        raise FirmwareError('invalid compressed payload') from exc
    if len(result) > MAX_PAYLOAD:
        raise FirmwareError('decompression limit exceeded')
    return result


def string(value: bytes) -> str:
    if not value.endswith(b'\0') or b'\0' in value[:-1]:
        raise FirmwareError('expected a single terminated FIT string')
    return value[:-1].decode('ascii')


def fdt(data: bytes) -> dict[str, dict[str, bytes]]:
    if len(data) < 40:
        raise FirmwareError('truncated FDT header')
    magic, size, off, strings_off, _, version, _, _, strings_size, struct_size = struct.unpack_from('>10I', data)
    if magic != 0xd00dfeed or size > len(data) or size > MAX_PAYLOAD or version != 17:
        raise FirmwareError('unsupported or truncated FDT')
    if min(off, strings_off) < 40 or off + struct_size > size or strings_off + strings_size > size:
        raise FirmwareError('FDT section out of bounds')
    strings = data[strings_off:strings_off + strings_size]
    end = off + struct_size
    stack: list[str] = []
    nodes: dict[str, dict[str, bytes]] = {}
    def terminated(blob: bytes, start: int, limit: int) -> tuple[str, int]:
        stop = blob.find(b'\0', start, limit)
        if stop < 0:
            raise FirmwareError('unterminated FDT name')
        return blob[start:stop].decode('ascii'), stop + 1
    while off + 4 <= end:
        token = struct.unpack_from('>I', data, off)[0]
        off += 4
        if token == 1:
            name, off = terminated(data, off, end)
            if '/' in name or len(stack) > 64:
                raise FirmwareError('invalid FDT node')
            stack.append(name)
            path = '/'.join(stack) or '/'
            if path in nodes:
                raise FirmwareError('duplicate FDT node')
            nodes[path] = {}
            off = (off + 3) & ~3
        elif token == 2:
            if not stack:
                raise FirmwareError('unbalanced FDT nodes')
            stack.pop()
        elif token == 3:
            if not stack or off + 8 > end:
                raise FirmwareError('invalid FDT property')
            length, name_off = struct.unpack_from('>II', data, off)
            off += 8
            if off + length > end or name_off >= len(strings):
                raise FirmwareError('FDT property out of bounds')
            name, _ = terminated(strings, name_off, len(strings))
            props = nodes['/'.join(stack) or '/']
            if name in props:
                raise FirmwareError('duplicate FDT property')
            props[name] = data[off:off + length]
            off = (off + length + 3) & ~3
        elif token == 9:
            if stack or '/' not in nodes:
                raise FirmwareError('incomplete FDT')
            return nodes
        elif token != 4:
            raise FirmwareError('unknown FDT token')
    raise FirmwareError('missing FDT end token')


def fit(data: bytes, matches: tuple[str, ...], configuration: str | None = None) -> tuple[str, dict[str, bytes]]:
    nodes = fdt(data)
    candidates = []
    for path, props in nodes.items():
        if path.startswith('/configurations/') and path.count('/') == 2:
            dt_name = string(props.get('fdt', b'\0'))
            if dt_name in matches:
                candidates.append(path.rsplit('/', 1)[1])
    if configuration:
        candidates = [c for c in candidates if c == configuration]
    if len(candidates) != 1:
        raise FirmwareError('missing or ambiguous device FIT configuration')
    selected = candidates[0]
    result = {}
    for role, expected in [('kernel', 'kernel'), ('fdt', 'flat_dt'), ('ramdisk', 'ramdisk')]:
        ref = nodes['/configurations/' + selected].get(role)
        if ref is None:
            if role == 'ramdisk':
                continue
            raise FirmwareError(f'missing FIT {role}')
        name = string(ref)
        props = nodes.get('/images/' + name, {})
        if string(props.get('type', b'\0')) != expected or string(props.get('arch', b'\0')) != 'arm64':
            raise FirmwareError(f'incompatible FIT {role}')
        payload = props.get('data')
        if payload is None:
            raise FirmwareError('external FIT data is not supported')
        hashes = [v for p, v in nodes.items() if p.startswith('/images/' + name + '/hash') and p.count('/') == 3]
        if not hashes:
            raise FirmwareError('FIT image lacks checksum')
        for check in hashes:
            algo = string(check.get('algo', b'\0'))
            if algo not in {'sha1', 'sha256', 'crc32'}:
                raise FirmwareError('unsupported FIT hash algorithm')
            actual = struct.pack('>I', zlib.crc32(payload)) if algo == 'crc32' else hashlib.new(algo, payload).digest()
            if actual != check.get('value'):
                raise FirmwareError('FIT image checksum mismatch')
        result[role] = decompress(payload, string(props.get('compression', b'none\0')))
    if result['kernel'][56:60] != b'ARM\x64':
        raise FirmwareError('FIT kernel is not an ARM64 Linux Image')
    fdt(result['fdt'])
    return selected, result


def uimage(data: bytes) -> bytes:
    if len(data) < 64 or data[:4] != b'\x27\x05\x19\x56':
        raise FirmwareError('missing uImage header')
    header = bytearray(data[:64])
    crc = struct.unpack_from('>I', header, 4)[0]
    header[4:8] = b'\0' * 4
    size = struct.unpack_from('>I', header, 12)[0]
    if zlib.crc32(header) != crc or size != len(data) - 64:
        raise FirmwareError('uImage header CRC or size mismatch')
    if data[28:32] != bytes([5, 2, 2, 0]):
        raise FirmwareError('expected uncompressed ARM Linux uImage')
    payload = data[64:]
    if zlib.crc32(payload) != struct.unpack_from('>I', data, 24)[0]:
        raise FirmwareError('uImage data CRC mismatch')
    return payload
