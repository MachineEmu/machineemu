import importlib.util
import json
from pathlib import Path

import pytest

MODULE = Path(__file__).resolve().parents[1] / 'scripts/import-udm-pro.py'
spec = importlib.util.spec_from_file_location('udm_import', MODULE)
udm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(udm)


def prepared(tmp_path):
    source = tmp_path / 'prepared'
    source.mkdir()
    for name in udm.COMPONENTS.values():
        (source / name).write_bytes(name.encode())
    profile = json.loads((udm.REPO / 'catalog/profiles/udm-pro-lab.json').read_text())
    profile['assets'] = {'boot': 'sha256:should-not-be-in-template'}
    return source, profile


def test_portable_udm_round_trip_and_default_template(tmp_path):
    source, profile = prepared(tmp_path)
    sources, _ = udm.bundle_sources(source)
    destination = tmp_path / 'portable'
    udm.export_bundle(destination, sources, profile, {'version': 'test-firmware'})
    resolved, manifest = udm.bundle_sources(destination)
    assert set(resolved) == set(udm.COMPONENTS)
    for role, path in resolved.items():
        assert path.read_bytes() == sources[role].read_bytes()
        assert manifest['components'][role]['sha256'] == udm.digest(path)
    template = json.loads((destination / 'profile.json').read_text())
    assert template['image'] == manifest['image_id'] == 'udm-pro'
    assert 'assets' not in template
    assert template['devices']['serial'] == 'socket'
    assert template['devices']['bluetooth'] is True
    assert template['network']['ports'][1]['bridge'] == 'br0'
    assert template['network']['ports'][2]['bridge'] == 'br10'
    with pytest.raises(ValueError, match='already exists'):
        udm.export_bundle(destination, sources, profile, {})


@pytest.mark.parametrize('problem', ['tamper', 'traversal', 'symlink', 'missing'])
def test_portable_udm_rejects_invalid_components(tmp_path, problem):
    source, profile = prepared(tmp_path)
    sources, _ = udm.bundle_sources(source)
    destination = tmp_path / 'portable'
    udm.export_bundle(destination, sources, profile, {})
    path = destination / 'components/Image'
    manifest = json.loads((destination / 'manifest.json').read_text())
    if problem == 'tamper':
        path.write_bytes(b'changed')
    elif problem == 'traversal':
        manifest['components']['kernel']['path'] = '../prepared/Image'
    elif problem == 'symlink':
        path.unlink()
        path.symlink_to(source / 'Image')
    else:
        del manifest['components']['kernel']
    (destination / 'manifest.json').write_text(json.dumps(manifest))
    with pytest.raises(ValueError):
        udm.bundle_sources(destination)
