"""Africa pins, saved run provenance, and history. Network calls are mocked."""
import json
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import showcase as sc


@pytest.fixture
def pins(tmp_path, monkeypatch):
    manifest = tmp_path / 'manifest-africa.toml'
    manifest.write_text(sc.MANIFEST.read_text())
    tsv = tmp_path / sc.PARTS_TSV.name
    tsv.write_text('s3://bucket/first\t5\tea\t2\t0,0,1,1\n'
                   's3://bucket/middle\t6\teb\t3\t1,1,2,2\n'
                   's3://bucket/last\t7\tec\t4\t2,2,3,3\n')
    monkeypatch.setattr(sc, 'MANIFEST', manifest)
    monkeypatch.setattr(sc, 'PARTS_TSV', tsv)
    return manifest, tsv


def test_release_and_bbox_come_from_manifest():
    spec = sc.rm.load_manifest(sc.MANIFEST)['cells']['ovt-s3']
    assert f'/release/{sc.RELEASE}/' in spec['args'][0]
    assert sc.region_bbox() == tuple(map(float, next(x for x in spec['flags'] if x.startswith('--bbox=')).split('=')[1].split(',')))
    assert set(sc.rm.load_manifest(sc.MANIFEST)['cells']) == {'ovt-s3'}


def test_check_pins_heads_only_endpoints(pins, monkeypatch):
    called = []
    def head(url):
        called.append(url)
        return (5, 'ea') if url.endswith('first') else (7, 'ec')
    monkeypatch.setattr(sc, 'head_object', head)
    sc.check_pins()
    assert called == ['s3://bucket/first', 's3://bucket/last']


@pytest.mark.parametrize('answer', [(8, 'ec'), (7, 'changed'), OSError('gone')])
def test_check_pins_repin_advice(pins, monkeypatch, answer):
    def head(url):
        if isinstance(answer, Exception):
            raise answer
        return answer
    monkeypatch.setattr(sc, 'head_object', head)
    with pytest.raises(ValueError, match=r'run `showcase.py pin <release>`'):
        sc.check_pins()


def test_anonymous_head_uses_https(monkeypatch):
    class Response:
        status = 200
        headers = {'Content-Length': '5', 'ETag': '"abc"'}
        def __enter__(self): return self
        def __exit__(self, *args): pass
    def opened(req, **kwargs):
        assert req.full_url == 'https://bucket.s3.us-west-2.amazonaws.com/a'
        assert req.method == 'HEAD'
        assert not req.has_header('Authorization')
        return Response()
    monkeypatch.setattr(sc.urllib.request, 'urlopen', opened)
    assert sc.head_object('s3://bucket/a') == (5, 'abc')


def test_pin_rewrites_listing_checksum_and_manifest(pins, monkeypatch):
    manifest, _ = pins
    prefix = f's3://{sc.BUCKET}/release/new-release/theme=buildings/type=building/'
    monkeypatch.setattr(sc, 'list_objects', lambda p: [
        {'url': p + 'part-a.parquet', 'bytes': 5, 'etag': 'e1'},
        {'url': p + 'part-b.parquet', 'bytes': 7, 'etag': 'e2'}])
    monkeypatch.setattr(sc, 'footer_groups', lambda url: [((0, 1, 2, 3), 4, 5), ((-1, 0, 3, 4), 6, 7)])
    sc.pin_release('new-release')
    tsv = manifest.parent / 'overture-buildings-new-release.tsv'
    parts = sc.read_parts(tsv)
    assert [p['url'] for p in parts] == [prefix + 'part-a.parquet', prefix + 'part-b.parquet']
    assert parts[0] == {'url': prefix + 'part-a.parquet', 'bytes': 5, 'etag': 'e1',
                        'rows': 10, 'bbox': (-1, 0, 3, 4)}
    assert (tsv.with_suffix('.tsv.sha256')).read_text().split() == [sc.rm.sha256_file(tsv), tsv.name]
    parsed = sc.rm.load_manifest(manifest)
    assert parsed['cells']['ovt-s3']['args'][0] == prefix
    assert parsed['inputs']['overture-listing']['sha256'] == sc.rm.sha256_file(tsv)
    assert parsed['inputs']['overture-listing']['path'].endswith(tsv.name)
    assert f'Overture buildings new-release' in manifest.read_text()
    assert sc.bbox_arg(sc.region_bbox()) in manifest.read_text()


def run():
    return {'wall_s': 10, 'cpu_s': 20, 'peak_rss_kib': 2048, 'scratch_peak_mib': 1,
            'output': {'bytes': 5, 'sha256': 'abc'}, 'bytes_fetched': 3,
            'profile': {'convert': {'pass1_rows': 2, 'true_peak_mib': 1}}, 'scan': {'features': 1},
            'argv': ['tylertoo', 'tiles', 's3://bucket/input', '/scratch/out.pmtiles'],
            'env': {'AWS_SKIP_SIGNATURE': 'true', 'TMPDIR': '/scratch/tmp'}}


def test_stream_row_retains_argv_env_and_exact_peak():
    row = sc.tool_row('default', [run()])
    assert row['argv'] == run()['argv']
    assert row['env'] == run()['env']
    assert row['peak_rss_bytes'] == 2048 * 1024
    assert sc.tool_row('default', [run()], archive={'sha256': 'other'})['archive'] is None


def test_saved_provenance_has_no_collector_host_fallback(tmp_path):
    saved = {k: f'saved-{k}' for k in sc.rm.PROVENANCE_KEYS}
    (tmp_path / 'provenance.json').write_text(json.dumps(saved))
    assert sc.run_provenance(tmp_path) == saved
    saved.pop('host_class')
    (tmp_path / 'provenance.json').write_text(json.dumps(saved))
    assert sc.run_provenance(tmp_path)['host_class'] is None


@pytest.mark.parametrize('describe', ['v0.8.0-2-gafb6b15', None])
def test_collect_stream_history(tmp_path, monkeypatch, describe):
    previous = {'provenance': {'commit': 'afb6b15', 'date': 'then', 'tylertoo_version': 'old',
                               'host_class': 'old-box', 'mem_gib': 247},
                'dataset': f'Overture buildings {sc.RELEASE}', 'rows': [{'runs_wall_s': [30, 20, 40]}],
                'history': [{'provenance': {'commit': 'older'}, 'wall_s': 50, 'release': 'older-release'}]}
    old = tmp_path / 'old.json'
    old.write_text(json.dumps(previous))
    calls = []
    def git(argv):
        calls.append(argv)
        return describe
    monkeypatch.setattr(sc.rm, '_out', git)
    monkeypatch.setattr(sc, 'ok_runs', lambda *args: [run()])
    monkeypatch.setattr(sc, 'read_parts', lambda: [])
    monkeypatch.setattr(sc, 'bbox_row_group_bytes', lambda *args: (0, 0))
    monkeypatch.setattr(sc, 'run_provenance', lambda *args: {'commit': 'new', 'git_describe': 'new-tag',
                                                          'host_class': 'box', 'release_version': 'next'})
    out = tmp_path / 'new.json'
    sc.collect_stream(SimpleNamespace(data=str(tmp_path), region='africa', name='africa-s3',
                                      binary='head', archive=None, history_from=str(old), out=str(out)))
    doc = json.loads(out.read_text())
    assert doc['history'][:-1] == previous['history']
    entry = doc['history'][-1]
    assert entry['provenance']['commit'] == 'afb6b15'
    assert entry['provenance']['date'] == 'then'
    assert entry['provenance']['host_class'] == 'old-box'
    assert entry['provenance']['mem_gib'] == 247
    assert entry['provenance']['git_describe'] == describe
    assert entry['provenance']['release_version'] is None
    assert entry['release'] == sc.RELEASE
    assert entry['wall_s'] == 30
    assert calls[0][-1] == 'afb6b15'
    assert doc['rows'][0]['argv'] == run()['argv']
    assert doc['provenance']['host_class'] == 'box'
    assert old.read_text() == json.dumps(previous)


def test_pin_failure_preserves_manifest(pins, monkeypatch):
    manifest, _ = pins
    before = manifest.read_bytes()
    monkeypatch.setattr(sc, 'list_objects', lambda p: [])
    with pytest.raises(ValueError, match='no parts'):
        sc.pin_release('missing')
    assert manifest.read_bytes() == before


def test_listing_paginates_anonymously(monkeypatch):
    responses = iter([
        '<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">'
        '<Contents><Key>prefix/b.parquet</Key><Size>7</Size><ETag>"b"</ETag></Contents>'
        '<NextContinuationToken>next</NextContinuationToken></ListBucketResult>',
        '<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">'
        '<Contents><Key>prefix/a.parquet</Key><Size>5</Size><ETag>"a"</ETag></Contents>'
        '<Contents><Key>prefix/ignore.txt</Key><Size>0</Size><ETag>"c"</ETag></Contents>'
        '</ListBucketResult>'])
    urls = []
    class Response:
        def read(self): return next(responses).encode()
        def __enter__(self): return self
        def __exit__(self, *args): pass
    def opened(url, **kwargs):
        urls.append(url)
        return Response()
    monkeypatch.setattr(sc.urllib.request, 'urlopen', opened)
    assert sc.list_objects('s3://bucket/prefix/') == [
        {'url': 's3://bucket/prefix/a.parquet', 'bytes': 5, 'etag': 'a'},
        {'url': 's3://bucket/prefix/b.parquet', 'bytes': 7, 'etag': 'b'}]
    assert 'continuation-token=next' in urls[1]
