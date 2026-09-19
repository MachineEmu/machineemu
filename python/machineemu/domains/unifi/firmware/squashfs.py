"""Bounded SquashFS 4 support backed by the supported native ABI.

The image reader/rebuilder is deliberately an optional preparation dependency:
the normal runtime package can inspect and launch an existing bundle without
loading it.  When a UDM-Pro image needs editing, version 1 of libsquashfs from
squashfs-tools-ng is required.  It is loaded explicitly rather than invoking
host extraction tools, so guest paths and metadata are never materialized on
the host filesystem.
"""
from __future__ import annotations

import ctypes as C
import ctypes.util
import os
import shutil
from contextlib import ExitStack
from dataclasses import dataclass
from pathlib import Path

from .models import FirmwareError
from .patches import guest_path

U16, U32, U64 = C.c_uint16, C.c_uint32, C.c_uint64
P, Z, I = C.c_void_p, C.c_size_t, C.c_int
NONE = 0xffffffff
INVALID = 0xffffffffffffffff
MAX_IMAGE = 2 * 1024**3
MAX_TOTAL = 8 * 1024**3
MAX_FILE = 1024**3


def memory_file() -> int:
    if hasattr(os, 'memfd_create'):
        return os.memfd_create('machineemu-squashfs', os.MFD_CLOEXEC)
    libc = C.CDLL(None, use_errno=True)
    try:
        fn = libc.memfd_create
    except AttributeError as exc:
        raise FirmwareError('in-memory SquashFS requires Linux memfd_create') from exc
    fn.argtypes, fn.restype = [C.c_char_p, C.c_uint], I
    fd = fn(b'machineemu-squashfs', 1)
    if fd < 0:
        raise FirmwareError('unable to allocate anonymous memory image')
    return fd


class Super(C.Structure):
    _fields_ = [(n, U32) for n in ('magic', 'inode_count', 'modification_time', 'block_size', 'fragment_entry_count')] + [
        (n, U16) for n in ('compression_id', 'block_log', 'flags', 'id_count', 'version_major', 'version_minor')] + [
        (n, U64) for n in ('root_inode_ref', 'bytes_used', 'id_table_start', 'xattr_id_table_start',
                          'inode_table_start', 'directory_table_start', 'fragment_table_start', 'export_table_start')]


class Base(C.Structure):
    _fields_ = [(n, U16) for n in ('type', 'mode', 'uid_idx', 'gid_idx')] + [('mod_time', U32), ('inode_number', U32)]


class FileExt(C.Structure):
    _fields_ = [(n, U64) for n in ('blocks_start', 'file_size', 'sparse')] + [
        (n, U32) for n in ('nlink', 'fragment_idx', 'fragment_offset', 'xattr_idx')]


class Inode(C.Structure):
    _fields_ = [('base', Base), ('available', U32), ('used', U32), ('data', FileExt)]


class Node(C.Structure):
    pass


Node._fields_ = [('parent', C.POINTER(Node)), ('children', C.POINTER(Node)), ('next', C.POINTER(Node)),
                 ('inode', C.POINTER(Inode)), ('uid', U32), ('gid', U32)]


class CompressorConfig(C.Structure):
    _fields_ = [('id', U16), ('flags', U16), ('block_size', U32), ('level', U32), ('opt', U64 * 2)]


class XattrDesc(C.Structure):
    _fields_ = [('xattr', U64), ('count', U32), ('size', U32)]


class Object(C.Structure):
    _fields_ = [('destroy', C.CFUNCTYPE(None, P)), ('copy', P)]


class Compressor(C.Structure):
    _fields_ = [('base', Object), ('get_configuration', P),
                ('write_options', C.CFUNCTYPE(I, P, P)), ('read_options', C.CFUNCTYPE(I, P, P)),
                ('do_block', C.CFUNCTYPE(C.c_int32, P, P, U32, P, U32))]


def library_path() -> str:
    """Resolve a user-selected or discoverable libsquashfs SONAME-1 library."""
    override = os.environ.get('MACHINEEMU_SQUASHFS_LIBRARY')
    if override:
        return override
    found = ctypes.util.find_library('squashfs')
    if found:
        return found
    binary = shutil.which('rdsquashfs')
    if binary:
        candidate = Path(binary).resolve().parent.parent / 'lib/libsquashfs.so.1'
        if candidate.exists():
            return str(candidate)
    raise FirmwareError('libsquashfs.so.1 is required; install squashfs-tools-ng or set MACHINEEMU_SQUASHFS_LIBRARY')


def _bind() -> C.CDLL:
    try:
        lib = C.CDLL(library_path())
        signatures = {
            'open_file': (P, [C.c_char_p, U32]), 'free': (None, [P]),
            'super_read': (I, [P, P]), 'super_write': (I, [P, P]),
            'compressor_config_init': (I, [P, I, Z, U16]), 'compressor_create': (I, [P, P]),
            'id_table_create': (P, [U32]), 'id_table_read': (I, [P, P, P, P]), 'id_table_write': (I, [P, P, P, P]),
            'dir_reader_create': (P, [P, P, P, U32]), 'dir_reader_get_full_hierarchy': (I, [P, P, C.c_char_p, U32, P]),
            'dir_tree_destroy': (None, [P]), 'data_reader_create': (P, [P, Z, P, U32]),
            'data_reader_load_fragment_table': (I, [P, P]), 'data_reader_read': (C.c_int32, [P, P, U64, P, U32]),
            'inode_get_file_size': (I, [P, P]), 'inode_get_xattr_index': (I, [P, P]), 'inode_set_xattr_index': (I, [P, U32]),
            'meta_writer_create': (P, [P, P, U32]), 'meta_writer_flush': (I, [P]), 'meta_writer_write_inode': (I, [P, P]),
            'meta_writer_get_position': (None, [P, P, P]), 'meta_write_write_to_file': (I, [P]),
            'dir_writer_create': (P, [P, U32]), 'dir_writer_begin': (I, [P, U32]),
            'dir_writer_add_entry': (I, [P, C.c_char_p, U32, U64, U16]), 'dir_writer_end': (I, [P]),
            'dir_writer_create_inode': (P, [P, Z, U32, U32]), 'dir_writer_write_export_table': (I, [P, P, P, U32, U64, P]),
            'xattr_reader_create': (P, [U32]), 'xattr_reader_load': (I, [P, P, P, P]), 'xattr_reader_get_desc': (I, [P, U32, P]),
            'xattr_reader_seek_kv': (I, [P, P]), 'xattr_reader_read_key': (I, [P, P]), 'xattr_reader_read_value': (I, [P, P, P]),
            'xattr_writer_create': (P, [U32]), 'xattr_writer_begin': (I, [P, U32]),
            'xattr_writer_add': (I, [P, C.c_char_p, P, Z]), 'xattr_writer_end': (I, [P, P]),
            'xattr_writer_flush': (I, [P, P, P, P]),
        }
        for name, (result, args) in signatures.items():
            fn = getattr(lib, 'sqfs_' + name)
            fn.restype, fn.argtypes = result, args
        return lib
    except (OSError, AttributeError) as exc:
        raise FirmwareError('incompatible libsquashfs; the 1.3.x API with SONAME 1 is required') from exc


def _check(code: int) -> None:
    if code < 0:
        raise FirmwareError(f'libsquashfs rejected image operation ({code})')


def _destroy(obj) -> None:
    C.cast(obj, C.POINTER(Object)).contents.destroy(obj)


@dataclass(frozen=True)
class Metadata:
    mode: int
    uid: int
    gid: int
    mtime: int
    inode: int
    size: int
    nlink: int
    xattrs: dict[str, bytes]


KINDS = {1: 0o040000, 2: 0o100000, 3: 0o120000, 4: 0o060000,
         5: 0o020000, 6: 0o010000, 7: 0o140000}


def _nlink(inode) -> int:
    kind = inode.contents.base.type
    if kind == 2:
        return 1
    offset = 4 if kind == 1 else 24 if kind == 9 else 0
    return U32.from_address(C.addressof(inode.contents) + Inode.data.offset + offset).value


class SquashFS:
    """A bounded image reader; use as a context manager to free native memory."""
    def __init__(self, image: bytes):
        self.resources = ExitStack()
        try:
            if len(image) > MAX_IMAGE or len(image) < 96:
                raise FirmwareError('SquashFS image exceeds bounds')
            self.lib = _bind()
            self.input_fd, self.file = self._file(image)
            self.super = Super()
            _check(self.lib.sqfs_super_read(C.byref(self.super), self.file))
            if self.super.inode_count > 100000 or self.super.bytes_used > len(image):
                raise FirmwareError('SquashFS metadata exceeds bounds')
            self.decoder = self._compressor(self.super.block_size, True)
            if self.super.flags & 0x400:
                _check(C.cast(self.decoder, C.POINTER(Compressor)).contents.read_options(self.decoder, self.file))
            self.ids = self._own(self.lib.sqfs_id_table_create(0))
            _check(self.lib.sqfs_id_table_read(self.ids, self.file, C.byref(self.super), self.decoder))
            self.reader = self._own(self.lib.sqfs_dir_reader_create(C.byref(self.super), self.decoder, self.file, 0))
            root = C.POINTER(Node)()
            _check(self.lib.sqfs_dir_reader_get_full_hierarchy(self.reader, self.ids, None, 0, C.byref(root)))
            self.resources.callback(self.lib.sqfs_dir_tree_destroy, root)
            self.nodes: dict[str, C.POINTER(Node)] = {}
            pending = [('', root, 0)]
            addresses: set[int] = set()
            total = 0
            while pending:
                path, node, depth = pending.pop()
                address = C.addressof(node.contents)
                if address in addresses or depth > 128 or len(addresses) >= 100000:
                    raise FirmwareError('cyclic or oversized SquashFS tree')
                addresses.add(address)
                inode = node.contents.inode.contents
                if inode.base.type not in range(1, 15):
                    raise FirmwareError('unknown SquashFS inode type')
                self.nodes[path] = node
                if inode.base.type in (2, 9):
                    length = U64()
                    _check(self.lib.sqfs_inode_get_file_size(node.contents.inode, C.byref(length)))
                    total += length.value
                    if length.value > MAX_FILE or total > MAX_TOTAL:
                        raise FirmwareError('SquashFS decompressed data exceeds limits')
                child = node.contents.children
                sibling_addresses: set[int] = set()
                while child:
                    child_addr = C.addressof(child.contents)
                    if child_addr in sibling_addresses:
                        raise FirmwareError('cyclic SquashFS directory')
                    sibling_addresses.add(child_addr)
                    name = C.string_at(child_addr + C.sizeof(Node)).decode('utf-8')
                    if '/' in name or guest_path(name) in ('.', '..'):
                        raise FirmwareError('invalid SquashFS directory name')
                    child_path = path + '/' + name if path else name
                    pending.append((child_path, child, depth + 1))
                    child = child.contents.next
            self.data_reader = self._own(self.lib.sqfs_data_reader_create(self.file, self.super.block_size, self.decoder, 0))
            _check(self.lib.sqfs_data_reader_load_fragment_table(self.data_reader, C.byref(self.super)))
            self.xr = None
            if self.super.xattr_id_table_start != INVALID:
                self.xr = self._own(self.lib.sqfs_xattr_reader_create(0))
                _check(self.lib.sqfs_xattr_reader_load(self.xr, C.byref(self.super), self.file, self.decoder))
        except BaseException:
            self.close()
            raise

    def _own(self, obj):
        if not obj:
            raise FirmwareError('libsquashfs allocation failed')
        self.resources.callback(_destroy, obj)
        return obj

    def _file(self, initial: bytes = b''):
        fd = memory_file()
        self.resources.callback(os.close, fd)
        if initial:
            with os.fdopen(os.dup(fd), 'wb') as stream:
                stream.write(initial)
        file = self._own(self.lib.sqfs_open_file(f'/proc/self/fd/{fd}'.encode(), 1 if initial else 2))
        return fd, file

    def _compressor(self, block_size: int, decode: bool):
        cfg = CompressorConfig()
        _check(self.lib.sqfs_compressor_config_init(C.byref(cfg), self.super.compression_id, block_size, 0x8000 if decode else 0))
        if not decode and self.super.compression_id == 6:
            cfg.level = 3
        result = P()
        _check(self.lib.sqfs_compressor_create(C.byref(cfg), C.byref(result)))
        return self._own(result)

    def close(self):
        self.resources.close()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()

    def _inode(self, path: str):
        path = guest_path(path)
        try:
            return self.nodes['' if path == '.' else path].contents.inode
        except KeyError as exc:
            raise FirmwareError('SquashFS path does not exist') from exc

    def xattrs(self, inode) -> dict[str, bytes]:
        index = U32()
        _check(self.lib.sqfs_inode_get_xattr_index(inode, C.byref(index)))
        if index.value == NONE:
            return {}
        if not self.xr:
            raise FirmwareError('inode refers to a missing xattr table')
        desc = XattrDesc()
        _check(self.lib.sqfs_xattr_reader_get_desc(self.xr, index, C.byref(desc)))
        if desc.count > 10000 or desc.size > 16 * 1024**2:
            raise FirmwareError('xattrs exceed limits')
        _check(self.lib.sqfs_xattr_reader_seek_kv(self.xr, C.byref(desc)))
        result = {}
        for _ in range(desc.count):
            key, value = P(), P()
            try:
                _check(self.lib.sqfs_xattr_reader_read_key(self.xr, C.byref(key)))
                _check(self.lib.sqfs_xattr_reader_read_value(self.xr, key, C.byref(value)))
                length = C.cast(value, C.POINTER(U32)).contents.value
                if length > 16 * 1024**2:
                    raise FirmwareError('xattr value exceeds limits')
                result[C.string_at(key.value + 4).decode()] = C.string_at(value.value + 4, length)
            finally:
                self.lib.sqfs_free(key)
                self.lib.sqfs_free(value)
        return result

    def metadata(self, path: str) -> Metadata:
        inode = self._inode(path)
        base = inode.contents.base
        node = self.nodes['' if path == '.' else guest_path(path)].contents
        size = U64()
        if base.type in (2, 9):
            _check(self.lib.sqfs_inode_get_file_size(inode, C.byref(size)))
        return Metadata(base.mode | KINDS[(base.type - 1) % 7 + 1], node.uid, node.gid,
                        base.mod_time, base.inode_number, size.value, _nlink(inode), self.xattrs(inode))

    def read(self, path: str) -> bytes:
        inode = self._inode(path)
        if inode.contents.base.type not in (2, 9):
            raise FirmwareError('SquashFS read target must be a regular file; links are not followed')
        size = self.metadata(path).size
        output = bytearray()
        buffer = C.create_string_buffer(min(size, 1024**2) or 1)
        while len(output) < size:
            n = self.lib.sqfs_data_reader_read(self.data_reader, inode, len(output), buffer, min(len(buffer), size - len(output)))
            _check(n)
            if n == 0:
                raise FirmwareError('truncated SquashFS file')
            output += buffer.raw[:n]
        return bytes(output)

    def rebuild(self, replacements: dict[str, bytes], block_size: int | None = None) -> bytes:
        """Rebuild all entries while preserving source metadata and hardlinks.

        Native objects and image buffers remain anonymous memfds.  In
        particular, this does not use ``unsquashfs`` or make an attacker
        controlled guest filename into a host pathname.
        """
        block_size = block_size or self.super.block_size
        if block_size < 4096 or block_size > 1048576 or block_size & (block_size - 1):
            raise FirmwareError('invalid SquashFS block size')
        changes: dict[int, bytes] = {}
        for path, content in replacements.items():
            inode = self._inode(path)
            if inode.contents.base.type not in (2, 9) or len(content) > MAX_FILE:
                raise FirmwareError('replacement must target a bounded regular file')
            number = inode.contents.base.inode_number
            if number in changes and changes[number] != content:
                raise FirmwareError('conflicting hardlink replacements')
            changes[number] = content
        with ExitStack() as output_resources:
            original_resources = self.resources
            self.resources = output_resources
            try:
                fd, output = self._file()
                encoder = self._compressor(block_size, False)
                cmp = C.cast(encoder, C.POINTER(Compressor)).contents
                superblock = Super.from_buffer_copy(self.super)
                superblock.block_size, superblock.block_log = block_size, block_size.bit_length() - 1
                superblock.flags = 0x10 | 0x80
                superblock.fragment_entry_count = 0
                superblock.fragment_table_start = INVALID
                superblock.xattr_id_table_start = INVALID
                superblock.export_table_start = INVALID
                _check(self.lib.sqfs_super_write(C.byref(superblock), output))
                options_size = cmp.write_options(encoder, output)
                _check(options_size)
                if options_size:
                    superblock.flags |= 0x400
                iw = self._own(self.lib.sqfs_meta_writer_create(output, encoder, 1))
                dw = self._own(self.lib.sqfs_meta_writer_create(output, encoder, 1))
                dirs = self._own(self.lib.sqfs_dir_writer_create(dw, 1))
                xw = self._own(self.lib.sqfs_xattr_writer_create(0))
                emitted: dict[int, tuple[int, int]] = {}
                uncompressed = C.create_string_buffer(block_size)
                compressed = C.create_string_buffer(block_size)
                children: dict[str, list[str]] = {}
                for path in self.nodes:
                    if path:
                        children.setdefault(path.rpartition('/')[0], []).append(path)

                def emit(path: str) -> tuple[int, int]:
                    source = self.nodes[path].contents.inode
                    old = source.contents
                    number = old.base.inode_number
                    if number in emitted:
                        return emitted[number]
                    attrs = self.xattrs(source)
                    xindex = U32(NONE)
                    if attrs:
                        _check(self.lib.sqfs_xattr_writer_begin(xw, 0))
                        for key, value in sorted(attrs.items()):
                            _check(self.lib.sqfs_xattr_writer_add(xw, key.encode(), value, len(value)))
                        _check(self.lib.sqfs_xattr_writer_end(xw, C.byref(xindex)))
                    allocated = None
                    if old.base.type in (1, 8):
                        child_entries = [(p, emit(p)) for p in sorted(children.get(path, []))]
                        _check(self.lib.sqfs_dir_writer_begin(dirs, 0))
                        for child, (reference, mode) in child_entries:
                            num = self.nodes[child].contents.inode.contents.base.inode_number
                            _check(self.lib.sqfs_dir_writer_add_entry(dirs, child.rsplit('/', 1)[-1].encode(), num, reference, mode))
                        _check(self.lib.sqfs_dir_writer_end(dirs))
                        parent = self.nodes[path.rpartition('/')[0]].contents.inode.contents.base.inode_number if path else number
                        allocated = self.lib.sqfs_dir_writer_create_inode(dirs, 1, xindex, parent)
                        if not allocated:
                            raise FirmwareError('directory inode allocation failed')
                        inode = C.cast(allocated, C.POINTER(Inode))
                        kind = inode.contents.base.type
                        inode.contents.base = old.base
                        inode.contents.base.type = kind
                        offset = 4 if kind == 1 else 0
                        U32.from_address(allocated + Inode.data.offset + offset).value = _nlink(source)
                    elif old.base.type in (2, 9):
                        replacement = changes.get(number)
                        size = U64()
                        _check(self.lib.sqfs_inode_get_file_size(source, C.byref(size)))
                        length = len(replacement) if replacement is not None else size.value
                        count = (length + block_size - 1) // block_size
                        storage = C.create_string_buffer(C.sizeof(Inode) + count * 4)
                        inode = C.cast(storage, C.POINTER(Inode))
                        inode.contents.base = old.base
                        inode.contents.base.type = 9
                        inode.contents.available = inode.contents.used = count * 4
                        file = inode.contents.data
                        file.blocks_start = os.fstat(fd).st_size
                        file.file_size = length
                        file.nlink = old.data.nlink if old.base.type == 9 else 1
                        file.fragment_idx, file.xattr_idx = NONE, xindex.value
                        sizes = (U32 * count).from_buffer(storage, C.sizeof(Inode))
                        for i in range(count):
                            offset = i * block_size
                            wanted = min(block_size, length - offset)
                            if replacement is None:
                                n = self.lib.sqfs_data_reader_read(self.data_reader, source, offset, uncompressed, wanted)
                                _check(n)
                                if n != wanted:
                                    raise FirmwareError('truncated file during SquashFS rebuild')
                            else:
                                C.memmove(uncompressed, replacement[offset:offset + wanted], wanted)
                            n = cmp.do_block(encoder, uncompressed, wanted, compressed, block_size)
                            _check(n)
                            data = compressed.raw[:n] if n else uncompressed.raw[:wanted]
                            sizes[i] = n if n else wanted | (1 << 24)
                            self._append(output, fd, data)
                    else:
                        storage = C.create_string_buffer(C.sizeof(Inode) + old.available)
                        C.memmove(storage, source, C.sizeof(Inode) + old.used)
                        inode = C.cast(storage, C.POINTER(Inode))
                        _check(self.lib.sqfs_inode_set_xattr_index(inode, xindex))
                    try:
                        block, offset = U64(), U32()
                        self.lib.sqfs_meta_writer_get_position(iw, C.byref(block), C.byref(offset))
                        reference = (block.value << 16) | offset.value
                        _check(self.lib.sqfs_meta_writer_write_inode(iw, inode))
                        mode = old.base.mode | KINDS[(old.base.type - 1) % 7 + 1]
                        emitted[number] = (reference, mode)
                        return reference, mode
                    finally:
                        if allocated:
                            self.lib.sqfs_free(allocated)

                root_ref, _ = emit('')
                _check(self.lib.sqfs_meta_writer_flush(iw))
                _check(self.lib.sqfs_meta_writer_flush(dw))
                superblock.inode_table_start = os.fstat(fd).st_size
                _check(self.lib.sqfs_meta_write_write_to_file(iw))
                superblock.directory_table_start = os.fstat(fd).st_size
                _check(self.lib.sqfs_meta_write_write_to_file(dw))
                root_number = self.nodes[''].contents.inode.contents.base.inode_number
                _check(self.lib.sqfs_dir_writer_write_export_table(dirs, output, encoder, root_number, root_ref, C.byref(superblock)))
                _check(self.lib.sqfs_id_table_write(self.ids, output, C.byref(superblock), encoder))
                _check(self.lib.sqfs_xattr_writer_flush(xw, output, C.byref(superblock), encoder))
                superblock.root_inode_ref = root_ref
                superblock.bytes_used = os.fstat(fd).st_size
                if superblock.bytes_used > MAX_IMAGE:
                    raise FirmwareError('rebuilt SquashFS exceeds memory limit')
                _check(self.lib.sqfs_super_write(C.byref(superblock), output))
                padding = (-superblock.bytes_used) % 4096
                if padding:
                    self._append(output, fd, bytes(padding))
                with os.fdopen(os.dup(fd), 'rb') as stream:
                    stream.seek(0)
                    return stream.read()
            finally:
                self.resources = original_resources

    @staticmethod
    def _append(output, fd: int, data: bytes):
        class File(C.Structure):
            _fields_ = [('base', Object), ('read_at', P), ('write_at', C.CFUNCTYPE(I, P, U64, P, Z))]
        offset = os.fstat(fd).st_size
        if offset + len(data) > MAX_IMAGE:
            raise FirmwareError('rebuilt SquashFS exceeds memory limit')
        _check(C.cast(output, C.POINTER(File)).contents.write_at(output, offset, data, len(data)))
