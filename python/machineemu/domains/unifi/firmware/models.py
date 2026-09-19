"""Versioned, content-checked preparation contracts. No launch side effects."""
from __future__ import annotations

import hashlib
import json
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any


class FirmwareError(ValueError):
    pass


def digest(path: Path) -> str:
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def inside(root: Path, relative: str) -> Path:
    if not isinstance(relative, str) or not relative or Path(relative).is_absolute():
        raise FirmwareError('artifact paths must be relative')
    path = (root / relative).resolve()
    if not path.is_relative_to(root.resolve()) or path == root.resolve():
        raise FirmwareError('artifact path escapes its directory')
    return path


def write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n', encoding='utf-8')


@dataclass(frozen=True)
class FirmwareInfo:
    device: str
    version: str
    source_size: int
    source_sha256: str
    configuration: str | None = None
    limitations: tuple[str, ...] = ()


@dataclass(frozen=True)
class PrepareOptions:
    variant: str = 'stock'
    rootfs: str = 'embedded'
    passwords: tuple[tuple[str, str], ...] = field(default=(), repr=False)
    boot_template: Path | None = None
    spi_template: Path | None = None
    bypass_factory_signature: bool = False
    bypass_factory_auth: bool = False
    factory_lab_key: Path | None = field(default=None, repr=False)
    system_id: int | None = None

    def public(self) -> dict[str, Any]:
        if self.variant not in {'stock', 'diagnostic'}:
            raise FirmwareError('variant must be stock or diagnostic')
        if self.rootfs not in {'embedded', 'external'}:
            raise FirmwareError('rootfs must be embedded or external')
        if type(self.bypass_factory_signature) is not bool:
            raise FirmwareError('bypass_factory_signature must be boolean')
        if self.bypass_factory_signature and self.variant != 'diagnostic':
            raise FirmwareError('--bypass-factory-signature requires --variant diagnostic')
        if type(self.bypass_factory_auth) is not bool:
            raise FirmwareError('bypass_factory_auth must be boolean')
        if self.bypass_factory_auth and self.variant != 'diagnostic':
            raise FirmwareError('--bypass-factory-auth requires --variant diagnostic')
        if self.system_id is not None and not 1 <= self.system_id <= 0xffff:
            raise FirmwareError('system id must be a non-zero 16-bit value')
        users = [user for user, _ in self.passwords]
        if self.factory_lab_key is not None and (
                self.variant != 'diagnostic' or self.bypass_factory_signature or self.bypass_factory_auth):
            raise FirmwareError('--factory-lab-key requires diagnostic mode without a bypass')
        if len(set(users)) != len(users):
            raise FirmwareError('duplicate password target account')
        return {
            'variant': self.variant,
            'rootfs': self.rootfs,
            'password_accounts': users,
            **({'bypass_factory_signature': True} if self.bypass_factory_signature else {}),
            **({'bypass_factory_auth': True} if self.bypass_factory_auth else {}),
            **({'factory_lab_key': True} if self.factory_lab_key is not None else {}),
            **({'system_id': self.system_id} if self.system_id is not None else {}),
            'external_inputs': {
                role: {'size': path.stat().st_size, 'sha256': digest(path)}
                for role, path in [('boot', self.boot_template), ('spi', self.spi_template),
                                   ('factory_lab_key', self.factory_lab_key)]
                if path is not None
            },
        }


@dataclass(frozen=True)
class StorageSpec:
    role: str
    backend: str
    initialization: str
    persistent: bool
    template: str | None = None
    format: str = 'raw'


@dataclass
class PreparedFirmware:
    info: FirmwareInfo
    adapter: str
    boot: dict[str, Any]
    settings: dict[str, Any]
    storage: list[StorageSpec]
    modifications: list[dict[str, str]] = field(default_factory=list)
    seed_policy: str = 'emulated-factory'
    seed_revision: str = 'model-defined'


@dataclass(frozen=True)
class Bundle:
    path: Path
    manifest: dict[str, Any]

    @property
    def identity(self) -> str:
        return hashlib.sha256(json.dumps(self.manifest, sort_keys=True).encode()).hexdigest()

    def artifact(self, role: str) -> Path:
        try:
            return inside(self.path, self.manifest['outputs'][role]['path'])
        except KeyError as exc:
            raise FirmwareError(f'missing artifact role: {role}') from exc


ADAPTERS = {'u6plus': 'mt7981', 'us24pro': 'us24pro', 'udm-pro': 'udm-pro'}
STORAGE = {
    'mt7981': {'emmc': ('mt7981-emmc', True), 'spi': ('model-memory', False)},
    'us24pro': {'flash': ('model-memory', False)},
    'udm-pro': {'boot': ('udm-boot', True), 'spi': ('udm-config', True)},
}


def load_bundle(path: Path) -> Bundle:
    path = path.resolve()
    try:
        manifest = json.loads((path / 'manifest.json').read_text())
        if manifest['version'] != 1:
            raise FirmwareError('unsupported manifest version')
        device = manifest['info']['device']
        adapter = manifest['adapter']
        if ADAPTERS.get(device) != adapter:
            raise FirmwareError('bundle device/adapter mismatch')
        for output in manifest['outputs'].values():
            artifact = inside(path, output['path'])
            if not artifact.is_file() or artifact.stat().st_size != output['size'] or digest(artifact) != output['sha256']:
                raise FirmwareError(f'missing or changed bundle artifact: {output["path"]}')
        boot = manifest['boot']
        if set(boot) - {'kernel', 'dtb', 'initrd', 'append'} or 'kernel' not in boot:
            raise FirmwareError('invalid boot settings')
        for key in ('kernel', 'dtb', 'initrd'):
            if boot.get(key) is not None and boot[key] not in manifest['outputs']:
                raise FirmwareError(f'boot input lacks artifact: {key}')
        if adapter != 'us24pro' and not boot.get('dtb'):
            raise FirmwareError('DTB is required')
        settings = manifest['settings']
        if set(settings) - {'machine', 'cpu', 'cpus', 'memory'}:
            raise FirmwareError('unsupported prepared machine settings')
        if settings['machine']['type'] != adapter:
            raise FirmwareError('prepared machine type differs from adapter')
        props = settings['machine'].get('properties', {})
        if props != ({'secure': True, 'gic-version': 3} if adapter == 'mt7981' else {}):
            raise FirmwareError('unsupported prepared machine properties')
        specs = [StorageSpec(**spec) for spec in manifest['storage']]
        if len({spec.role for spec in specs}) != len(specs) or {spec.role for spec in specs} != set(STORAGE[adapter]):
            raise FirmwareError('missing, duplicate, or unknown storage role')
        for spec in specs:
            if (spec.backend, spec.persistent) != STORAGE[adapter][spec.role] or spec.format != 'raw':
                raise FirmwareError('unsupported storage capability')
            if spec.initialization not in {'copy', 'board-seeded'}:
                raise FirmwareError('unsupported storage initialization')
            if spec.initialization == 'copy':
                if spec.template not in manifest['outputs']:
                    raise FirmwareError('missing storage template')
            elif spec.template is not None:
                raise FirmwareError('board-seeded storage cannot have a template')
        if manifest['options']['variant'] not in {'stock', 'diagnostic'}:
            raise FirmwareError('unsupported variant')
        bypass = manifest['options'].get('bypass_factory_signature', False)
        if type(bypass) is not bool or (bypass and
                (device != 'us24pro' or manifest['options']['variant'] != 'diagnostic')):
            raise FirmwareError('factory signature bypass requires a US24PRO diagnostic bundle')
        auth = manifest['options'].get('bypass_factory_auth', False)
        lab = manifest['options'].get('factory_lab_key', False)
        if type(lab) is not bool or (lab and (device not in {'us24pro', 'udm-pro', 'u6plus'}
                or manifest['options']['variant'] != 'diagnostic' or bypass or auth
                or 'eeprom.bin' not in manifest['outputs'])):
            raise FirmwareError('lab key requires a supported diagnostic bundle without bypasses')
        if lab and device == 'u6plus' and (not boot.get('initrd')
                or manifest['options'].get('rootfs') != 'external'):
            raise FirmwareError('U6+ lab key requires an external rootfs containing the verifier')
        if type(auth) is not bool or (auth and
                (device != 'udm-pro' or manifest['options']['variant'] != 'diagnostic')):
            raise FirmwareError('factory auth bypass requires a UDM-Pro diagnostic bundle')
        return Bundle(path, manifest)
    except (KeyError, TypeError, json.JSONDecodeError, OSError) as exc:
        raise FirmwareError(f'invalid prepared bundle: {path}') from exc


def manifest_for(result: PreparedFirmware, output: Path, revision: str, options: dict, tools: dict) -> dict:
    files = {p.name: {'path': p.name, 'size': p.stat().st_size, 'sha256': digest(p)}
             for p in sorted(output.iterdir()) if p.is_file()}
    return {'version': 1, **asdict(result), 'recipe_revision': revision,
            'options': options, 'tools': tools, 'outputs': files}
