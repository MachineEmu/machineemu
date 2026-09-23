#!/usr/bin/env python3
"""Import prepared UDM Pro firmware or a portable images/udm-pro bundle."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile

REPO = Path(__file__).resolve().parent.parent
COMPONENTS = {'kernel': 'Image', 'initrd': 'initramfs.cpio', 'boot': 'boot.img', 'spi': 'spi.img'}


def digest(path):
    with path.open('rb') as stream:
        return 'sha256:' + hashlib.file_digest(stream, 'sha256').hexdigest()


def read_json(path):
    return json.loads(path.read_text())


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def bundle_sources(bundle):
    """Resolve and verify every portable component before importing anything."""
    manifest_path = bundle / 'manifest.json'
    manifest = read_json(manifest_path) if manifest_path.is_file() else {}
    if 'components' not in manifest:
        sources = {role: bundle / name for role, name in COMPONENTS.items()}
        for source in sources.values():
            if not source.is_file():
                raise ValueError(f'missing prepared component: {source}')
        return sources, None
    if (manifest.get('schema_version') != 1 or manifest.get('target') != 'aarch64-softmmu'
            or manifest.get('engine_track') != 'unifi-10.2'):
        raise ValueError('unsupported UDM Pro image manifest')
    if set(manifest['components']) != set(COMPONENTS):
        raise ValueError('UDM Pro bundle must contain kernel, initrd, boot, and spi components')
    sources = {}
    for role in COMPONENTS:
        component = manifest['components'].get(role)
        if not isinstance(component, dict):
            raise ValueError(f'missing portable component: {role}')
        relative = Path(component['path'])
        if relative.is_absolute() or '..' in relative.parts:
            raise ValueError(f'unsafe component path: {relative}')
        source = (bundle / relative).resolve()
        if not source.is_relative_to(bundle.resolve()) or not source.is_file():
            raise ValueError(f'component escapes bundle or is missing: {relative}')
        if digest(source) != component['sha256']:
            raise ValueError(f'digest mismatch: {relative}')
        sources[role] = source
    return sources, manifest


def export_bundle(destination, sources, profile, provenance):
    """Publish a complete portable directory, never overwrite an existing image."""
    if destination.exists():
        raise ValueError(f'export destination already exists: {destination}')
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.udm-export-', dir=destination.parent) as staging:
        stage = Path(staging) / 'bundle'
        (stage / 'components').mkdir(parents=True)
        components = {}
        for role, name in COMPONENTS.items():
            target = stage / 'components' / name
            # Preserve sparse boot images and use copy-on-write when available.
            subprocess.run(['cp', '--reflink=auto', '--sparse=always', '--', str(sources[role]), str(target)], check=True)
            components[role] = {'path': f'components/{name}', 'sha256': digest(target)}
        template = dict(profile)
        template.pop('assets', None)
        template['image'] = 'udm-pro'
        write_json(stage / 'profile.json', template)
        manifest = {
            'schema_version': 1, 'image_id': 'udm-pro',
            'engine_track': profile['engine']['track'], 'target': profile['target'],
            'components': components,
        }
        if provenance:
            manifest['firmware'] = provenance
        write_json(stage / 'manifest.json', manifest)
        stage.rename(destination)
    return destination


def import_profile(workspace, executable, sources, profile):
    profile = dict(profile)
    profile['assets'] = {}
    for role, source in sources.items():
        reference = digest(source)
        cached = workspace / 'blobs/sha256' / reference[7:]
        if cached.exists():
            if not cached.is_file() or digest(cached) != reference:
                raise ValueError(f'corrupt cached component: {cached}')
            profile['assets'][role] = reference
            continue
        result = subprocess.run([
            executable, 'import-asset', '--workspace', str(workspace), '--source', str(source),
        ], check=True, text=True, capture_output=True)
        if result.stdout.strip() != reference:
            raise ValueError(f'component changed during import: {source}')
        profile['assets'][role] = reference
    directory = workspace / 'profiles'
    directory.mkdir(parents=True, exist_ok=True)
    manifest = directory / (profile['id'] + '-image.json')
    write_json(manifest, {
        'image_id': profile.get('image', profile['id']), 'engine_track': profile['engine']['track'],
        'target': profile['target'], 'disk_sha256': profile['assets']['boot'][7:],
        'firmware_sha256': None, 'tpm_state_sha256': None,
    })
    subprocess.run([executable, 'register-image', '--workspace', str(workspace),
                    '--manifest', str(manifest)], check=True)
    output = directory / (profile['id'] + '.json')
    write_json(output, profile)
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('bundle', type=Path)
    parser.add_argument('--workspace', type=Path, default=Path('machineemu-workspace'))
    parser.add_argument('--machineemu', default='target/debug/machineemu')
    parser.add_argument('--profile', choices=['udm-pro', 'udm-pro-lab'],
                        help='override the bundle template; prepared firmware defaults to udm-pro')
    parser.add_argument('--export-bundle', type=Path, help='also create a portable image with a default profile template')
    args = parser.parse_args()
    try:
        sources, manifest = bundle_sources(args.bundle)
        template = args.bundle / 'profile.json'
        if args.profile or not template.is_file():
            template = REPO / 'profiles' / ((args.profile or 'udm-pro') + '.json')
        profile = read_json(template)
        if (profile.get('machine') != 'udm-pro' or profile.get('target') != 'aarch64-softmmu'
                or profile.get('engine', {}).get('track') != 'unifi-10.2'
                or not re.fullmatch(r'[a-z][a-z0-9._-]{0,63}', profile.get('id', ''))):
            raise ValueError('invalid UDM Pro profile template identity or engine')
        if manifest:
            image_id = manifest.get('image_id', '')
            if not re.fullmatch(r'[a-z][a-z0-9._-]{0,63}', image_id):
                raise ValueError('invalid image_id in portable manifest')
            profile['image'] = image_id
        elif args.export_bundle:
            profile['image'] = 'udm-pro'
        if args.export_bundle and args.export_bundle.exists():
            raise ValueError(f'export destination already exists: {args.export_bundle}')
        with tempfile.TemporaryDirectory(prefix='udm-import-') as temporary:
            if profile.get('devices', {}).get('bluetooth'):
                sys.path.insert(0, str(REPO / 'python'))
                from machineemu.domains.unifi.firmware.btattach import install
                patched = Path(temporary) / 'initramfs.cpio'
                patched.write_bytes(install(sources['initrd'].read_bytes()))
                sources['initrd'] = patched
            if args.export_bundle:
                prepared = args.bundle / 'manifest.json'
                info = read_json(prepared).get('info', {}) if prepared.is_file() else {}
                provenance = manifest.get('firmware', {}) if manifest else {
                    key: info[key] for key in ['version', 'source_sha256'] if key in info
                }
                export_bundle(args.export_bundle, sources, profile, provenance)
                print(f'exported {args.export_bundle}')
            output = import_profile(args.workspace, args.machineemu, sources, profile)
            print(output)
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f'import-udm-pro: {error}\n')


if __name__ == '__main__':
    main()
