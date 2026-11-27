"""Comparison manifests and local publication, without real archives or AWS."""
import hashlib
import json
from types import SimpleNamespace
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run_manifest as rm
import comparison as cp


@pytest.fixture
def corpus(tmp_path):
    work = tmp_path / 'corpus'
    ds = work / 'tiny'
    ds.mkdir(parents=True)
    for tool in cp.TOOLS:
        (ds / f'{tool}.pmtiles').write_bytes(tool.encode())
    provenance = {k: f'value-{k}' for k in rm.PROVENANCE_KEYS}
    provenance['release_version'] = 'next'
    entry = {'label': 'Tiny polygons', 'geometry': 'polygon', 'layer': 'tiny', 'features': 4,
             'min_zoom': 0, 'max_zoom': 3,
             'tylertoo': {'argv': ['tylertoo', 'tiles'], 'per_zoom': {'by_zoom': {'0': {}, '2': {}}}},
             'tippecanoe_fgb': {'argv': ['tippecanoe', '-z3'], 'per_zoom': {'by_zoom': {'0': {}, '3': {}}}}}
    result = tmp_path / 'e2e.json'
    result.write_text(json.dumps({'provenance': provenance, 'datasets': {'tiny': entry}}))
    manifest = tmp_path / 'comparison-pmtiles.json'
    cp.build(result, work, manifest)
    return work, manifest, provenance


def test_build(corpus):
    work, path, provenance = corpus
    doc = json.loads(path.read_text())
    assert doc['provenance'] == provenance
    assert set(rm.PROVENANCE_KEYS) <= doc['provenance'].keys()
    assert doc['version'] == 'next'
    assert doc['base_url'] == cp.BASE_URL + 'next/'
    ds = doc['datasets'][0]
    assert (ds['id'], ds['label'], ds['geometry'], ds['layer'], ds['features']) == (
        'tiny', 'Tiny polygons', 'polygon', 'tiny', 4)
    for tool in cp.TOOLS:
        rec = ds[tool]
        assert rec['url'] == doc['base_url'] + f'tiny/{tool}.pmtiles'
        assert rec['bytes'] == len(tool)
        assert rec['sha256'] == hashlib.sha256(tool.encode()).hexdigest()
        assert rec['min_zoom'] == 0
        assert rec['max_zoom'] == (2 if tool == 'tylertoo' else 3)
        assert rec['argv'][0] == tool


def test_upload_local_and_refuse_existing(corpus, tmp_path):
    work, manifest, _ = corpus
    target = tmp_path / 'published'
    cp.upload(manifest, work, str(target))
    for tool in cp.TOOLS:
        assert (target / 'next/tiny' / f'{tool}.pmtiles').read_bytes() == tool.encode()
    assert (target / 'next/comparison-pmtiles.json').read_bytes() == manifest.read_bytes()
    with pytest.raises(ValueError, match='already'):
        cp.upload(manifest, work, str(target))
    cp.upload(manifest, work, str(target), force=True)


def test_local_size_mismatch(corpus, tmp_path, monkeypatch):
    work, manifest, _ = corpus
    copy = cp.shutil.copyfile
    def broken(src, dst):
        copy(src, dst)
        if str(dst).endswith('tylertoo.pmtiles'):
            Path(dst).write_bytes(b'')
    monkeypatch.setattr(cp.shutil, 'copyfile', broken)
    with pytest.raises(ValueError, match='size'):
        cp.upload(manifest, work, str(tmp_path / 'bad'))


def test_wrong_source_archive_refused(corpus, tmp_path):
    work, manifest, _ = corpus
    (work / 'tiny/tylertoo.pmtiles').write_bytes(b'changed')
    with pytest.raises(ValueError, match='archive'):
        cp.upload(manifest, work, str(tmp_path / 'bad'))


def test_public_head_mismatch(monkeypatch):
    class Response:
        status = 200
        headers = {'Content-Length': '1'}
        def __enter__(self): return self
        def __exit__(self, *args): pass
    def head(req, **kwargs):
        assert req.method == 'HEAD'
        return Response()
    monkeypatch.setattr(cp.urllib.request, 'urlopen', head)
    with pytest.raises(ValueError, match='size'):
        cp.verify_url('https://example.test/archive', 2)


def test_s3_copy_profile_and_public_verification(corpus, monkeypatch):
    work, manifest, _ = corpus
    calls, heads = [], []
    def aws(argv, **kwargs):
        calls.append(argv)
        # The real `aws s3 ls` exits 1 on an empty prefix.
        return cp.subprocess.CompletedProcess(argv, 1 if argv[2] == 'ls' else 0, '', '')
    monkeypatch.setattr(cp.subprocess, 'run', aws)
    monkeypatch.setattr(cp, 'verify_url', lambda url, size: heads.append((url, size)))
    cp.upload(manifest, work, cp.S3_TARGET)
    assert calls[0][:3] == ['aws', 's3', 'ls']
    copies = calls[1:]
    assert len(copies) == len(cp.TOOLS) + 1
    assert all(c[-2:] == ['--profile', 'source-coop'] for c in calls)
    assert all(c[4].startswith(cp.S3_TARGET + '/next/') for c in copies)
    assert all(url.startswith(cp.BASE_URL + 'next/') for url, size in heads)
    assert [size for url, size in heads[:-1]] == [len(tool) for tool in cp.TOOLS]


def test_s3_listing_error_stops_upload(corpus, monkeypatch):
    work, manifest, _ = corpus
    monkeypatch.setattr(cp.subprocess, 'run',
                        lambda argv, **kw: cp.subprocess.CompletedProcess(argv, 255, '', 'expired'))
    with pytest.raises(ValueError, match='expired'):
        cp.upload(manifest, work, cp.S3_TARGET)


def test_s3_nonempty_prefix_refused(corpus, monkeypatch):
    work, manifest, _ = corpus
    calls = []
    def aws(argv, **kwargs):
        calls.append(argv)
        return cp.subprocess.CompletedProcess(argv, 0, 'existing object', '')
    monkeypatch.setattr(cp.subprocess, 'run', aws)
    with pytest.raises(ValueError, match='already'):
        cp.upload(manifest, work, cp.S3_TARGET)
    assert len(calls) == 1


def test_existing_empty_directory_is_allowed(corpus, tmp_path):
    work, manifest, _ = corpus
    (tmp_path / 'target/next').mkdir(parents=True)
    cp.upload(manifest, work, str(tmp_path / 'target'))


def test_isolated_cli_upload(corpus, tmp_path):
    work, manifest, _ = corpus
    result = cp.subprocess.run([sys.executable, '-I', str(Path(cp.__file__)), 'upload',
                               '--manifest', str(manifest), '--work', str(work),
                               '--target', str(tmp_path / 'cli')], capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    assert 'Verified' in result.stdout


@pytest.mark.parametrize('s3', [False, True])
def test_tutorial_map_record_and_publication(tmp_path, monkeypatch, s3):
    binary = tmp_path / 'bin/tylertoo'
    work, out = tmp_path / 'work', tmp_path / 'tutorial-map.json'
    prov = {key: f'fact-{key}' for key in rm.PROVENANCE_KEYS}
    monkeypatch.setattr(rm, 'parse_bin', lambda spec: {'path': str(binary)})
    monkeypatch.setattr(rm, 'provenance', lambda bins, inputs: prov)
    calls, heads = [], []
    def run(argv, **kw):
        calls.append(argv)
        if argv[0] == 'bash':
            assert Path(argv[1]) == rm.ROOT / 'examples/brazil-fields/01-tiles.sh'
            assert kw['cwd'] == work
            assert kw['env']['PATH'].split(':')[0] == str(binary.parent)
            assert Path(kw['env']['TMPDIR']).is_relative_to(work)
            (work / 'brazil.pmtiles').write_bytes(b'tiny synthetic archive')
        elif argv[0] == str(binary):
            assert argv[1:] == ['stats', '--json', str(work / 'brazil.pmtiles')]
            return cp.subprocess.CompletedProcess(argv, 0, json.dumps(
                {'per_zoom': [{'tile_count': 2}, {'tile_count': 5}]}), '')
        return cp.subprocess.CompletedProcess(argv, 0, '', '')
    monkeypatch.setattr(cp.subprocess, 'run', run)
    monkeypatch.setattr(cp, 'verify_url', lambda *args: heads.append(args))
    monkeypatch.setattr(cp, 'gen_bench_docs', lambda: SimpleNamespace(tutorial_tile_count=lambda root: 7))
    target = cp.MAP_TARGET if s3 else str(tmp_path / 'published')
    cp.tutorial_map(work, out, binary, target)
    doc = json.loads(out.read_text())
    assert doc['provenance'] == prov
    assert doc['archive'] == {'url': cp.MAP_URL, 'bytes': len(b'tiny synthetic archive'),
        'sha256': hashlib.sha256(b'tiny synthetic archive').hexdigest(), 'tiles': 7}
    if s3:
        assert calls[-1] == ['aws', 's3', 'cp', str(work / 'brazil.pmtiles'), cp.MAP_TARGET,
                             '--profile', 'source-coop']
        assert heads == [(cp.MAP_URL, doc['archive']['bytes'])]
    else:
        assert (Path(target) / 'brazil.pmtiles').read_bytes() == b'tiny synthetic archive'
        assert not heads
        # The tutorial object is intentionally replaceable.
        cp.tutorial_map(work, out, binary, target)


def test_tutorial_map_refuses_count_that_disagrees_with_tutorial(tmp_path, monkeypatch):
    def run(argv, **kw):
        if argv[0] == 'bash':
            (tmp_path / 'work/brazil.pmtiles').write_bytes(b'tiny')
        return cp.subprocess.CompletedProcess(argv, 0, '{"per_zoom": [{"tile_count": 3}]}', '')
    monkeypatch.setattr(cp.subprocess, 'run', run)
    calls = []
    monkeypatch.setattr(cp.shutil, 'copyfile', lambda *a: calls.append(a))
    with pytest.raises(ValueError, match='disagrees|expects'):
        cp.tutorial_map(tmp_path / 'work', tmp_path / 'out.json', tmp_path / 'tylertoo', str(tmp_path / 'pub'))
    assert not calls and not (tmp_path / 'out.json').exists()


def test_tutorial_map_failed_verification_leaves_no_record(tmp_path, monkeypatch):
    monkeypatch.setattr(cp, 'gen_bench_docs', lambda: SimpleNamespace(tutorial_tile_count=lambda root: 0))
    monkeypatch.setattr(rm, 'parse_bin', lambda spec: {'path': 'tylertoo'})
    monkeypatch.setattr(rm, 'provenance', lambda *args: {})
    def run(argv, **kw):
        if argv[0] == 'bash':
            (tmp_path / 'work/brazil.pmtiles').write_bytes(b'tiny')
        return cp.subprocess.CompletedProcess(argv, 0, '{"per_zoom": []}', '')
    monkeypatch.setattr(cp.subprocess, 'run', run)
    monkeypatch.setattr(cp, 'verify_url', lambda *args: (_ for _ in ()).throw(ValueError('size')))
    out = tmp_path / 'tutorial-map.json'
    with pytest.raises(ValueError, match='size'):
        cp.tutorial_map(tmp_path / 'work', out, Path('tylertoo'), 's3://test/map.pmtiles')
    assert not out.exists()


def test_build_falls_back_to_the_measured_describe_label(corpus, tmp_path):
    source = json.loads((tmp_path / 'e2e.json').read_text())
    source['provenance'].update(release_version=None, git_describe='v9.9.9-52-gabcdef0')
    (tmp_path / 'e2e.json').write_text(json.dumps(source))
    doc = cp.build(tmp_path / 'e2e.json', corpus[0], tmp_path / 'm.json')
    assert doc['version'] == 'v9.9.9-52-gabcdef0'


def test_build_takes_prerelease_label(corpus, tmp_path):
    work = corpus[0]
    doc = cp.build(tmp_path / 'e2e.json', work, tmp_path / 'm.json', 'v9.9.9-52-gabcdef0')
    assert doc['version'] == 'v9.9.9-52-gabcdef0'
    assert doc['base_url'] == cp.BASE_URL + 'v9.9.9-52-gabcdef0/'


def test_tutorial_map_from_existing_archive(tmp_path, monkeypatch):
    archive = tmp_path / 'brazil.pmtiles'
    archive.write_bytes(b'already published')
    monkeypatch.setattr(rm, 'parse_bin', lambda spec: {'path': 'tylertoo'})
    monkeypatch.setattr(rm, 'provenance', lambda *args: {})
    monkeypatch.setattr(cp, 'gen_bench_docs', lambda: SimpleNamespace(tutorial_tile_count=lambda root: 4))
    calls, heads = [], []
    def run(argv, **kw):
        calls.append(argv)
        assert argv[1:3] == ['stats', '--json']  # nothing rebuilt, nothing uploaded
        return cp.subprocess.CompletedProcess(argv, 0, '{"per_zoom": [{"tile_count": 4}]}', '')
    monkeypatch.setattr(cp.subprocess, 'run', run)
    monkeypatch.setattr(cp, 'verify_url', lambda *a: heads.append(a))
    out = tmp_path / 'tutorial-map.json'
    cp.tutorial_map(tmp_path / 'work', out, tmp_path / 'tylertoo', cp.MAP_TARGET, archive)
    doc = json.loads(out.read_text())
    assert doc['archive']['tiles'] == 4 and doc['archive']['bytes'] == len(b'already published')
    assert heads == [(cp.MAP_URL, len(b'already published'))]
    assert len(calls) == 1 and 'not rebuilt' in doc['provenance']['notes'][0]
