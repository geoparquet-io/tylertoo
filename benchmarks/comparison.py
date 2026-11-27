#!/usr/bin/env python3
"""Build and publish the release corpus's comparison archives."""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run_manifest as rm

TOOLS = ('tylertoo', 'tippecanoe')
BASE_URL = 'https://data.source.coop/nlebovits/tylertoo-benchmarks/'
S3_TARGET = 's3://nlebovits/tylertoo-benchmarks'
MAP_TARGET = 's3://nlebovits/tylertoo-brazil-demo/brazil.pmtiles'
MAP_URL = 'https://data.source.coop/nlebovits/tylertoo-brazil-demo/brazil.pmtiles'


def component(value: str) -> str:
    """Reject names that could escape the immutable version directory."""
    if not isinstance(value, str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._+-]*', value):
        raise ValueError(f'invalid archive path component: {value!r}')
    return value


def archive_path(work: Path, did: str, tool: str) -> Path:
    return work / component(did) / f'{tool}.pmtiles'


def build(corpus: Path, work: Path, out: Path, version: str | None = None) -> dict:
    """`version` names the upload prefix; it defaults to the run's release
    version, and release.sh passes the git describe label for --prerelease."""
    source = json.loads(corpus.read_text())
    prov = {k: source['provenance'].get(k) for k in rm.PROVENANCE_KEYS}
    version = component(version or prov['release_version'])
    base = BASE_URL + version + '/'
    datasets = []
    for did, ds in source['datasets'].items():
        rec = {key: ds[key] for key in ('label', 'geometry', 'layer', 'features')}
        rec['id'] = component(did)
        for tool in TOOLS:
            path = archive_path(work, did, tool)
            measurement = ds['tylertoo' if tool == 'tylertoo' else 'tippecanoe_fgb']
            zooms = [int(z) for z in measurement['per_zoom']['by_zoom']]
            rec[tool] = {'url': base + f'{did}/{tool}.pmtiles', 'bytes': path.stat().st_size,
                         'sha256': rm.sha256_file(path), 'min_zoom': min(zooms),
                         'max_zoom': max(zooms), 'argv': measurement['argv']}
        datasets.append(rec)
    if not datasets:
        raise ValueError('corpus has no comparison datasets')
    doc = {'provenance': prov, 'version': version, 'base_url': base, 'datasets': datasets}
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(doc, indent=2) + '\n')
    return doc


def verify_url(url: str, size: int) -> None:
    with urllib.request.urlopen(urllib.request.Request(url, method='HEAD'), timeout=60) as response:
        if response.status != 200:
            raise ValueError(f'HEAD failed: {url}: {response.status}')
        if response.headers.get('Content-Length') != str(size):
            raise ValueError(f'public size mismatch: {url}: expected {size} bytes')


def upload(manifest: Path, work: Path, target: str, force: bool = False) -> None:
    doc = json.loads(manifest.read_text())
    version = component(doc['version'])
    s3 = target.startswith('s3://')
    destination = target.rstrip('/') + '/' + version + '/'
    if s3:
        listing = subprocess.run(['aws', 's3', 'ls', destination, '--recursive', '--profile',
                                  'source-coop'], capture_output=True, text=True)
        # `aws s3 ls` exits 1 when the prefix holds nothing; anything else is an error.
        if listing.returncode not in (0, 1):
            raise ValueError(f'aws s3 ls {destination} failed: {listing.stderr.strip()}')
        exists = bool(listing.stdout.strip())
    else:
        root = Path(target).expanduser() / version
        exists = root.exists() and (not root.is_dir() or any(root.iterdir()))
    if exists and not force:
        raise ValueError(f'{destination} already holds files; use --force to replace them')

    # Validate every source before writing the destination. In particular, never
    # upload an archive left by a different corpus run under this manifest.
    copies = []
    for ds in doc['datasets']:
        did = component(ds['id'])
        for tool in TOOLS:
            src = archive_path(work, did, tool)
            rec = ds[tool]
            if src.stat().st_size != rec['bytes'] or rm.sha256_file(src) != rec['sha256']:
                raise ValueError(f'archive differs from manifest: {src}')
            copies.append((src, f'{did}/{tool}.pmtiles', rec['bytes'], rec['url']))
    copies.append((manifest, 'comparison-pmtiles.json', manifest.stat().st_size,
                   doc['base_url'] + 'comparison-pmtiles.json'))
    for src, relative, size, url in copies:
        if s3:
            subprocess.run(['aws', 's3', 'cp', str(src), destination + relative,
                            '--profile', 'source-coop'], check=True)
        else:
            dst = root / relative
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(src, dst)
    # Verify after all copies, so a failure cannot be mistaken for completion.
    for _, relative, size, url in copies:
        if s3:
            verify_url(url, size)
        elif (root / relative).stat().st_size != size:
            raise ValueError(f'copied size mismatch: {root / relative}: expected {size} bytes')
    print(f'Verified {len(copies)} files at {destination}')


def gen_bench_docs():
    """scripts/gen_bench_docs.py owns the tutorial's expected tile count."""
    spec = importlib.util.spec_from_file_location('gen_bench_docs', rm.ROOT / 'scripts/gen_bench_docs.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def tutorial_map(work: Path, out: Path, binary: Path, target: str, source: Path | None = None) -> None:
    """Run tutorial step one, replace its map, and record verified facts.
    With `source`, record an archive that was already built and published:
    nothing is rebuilt or uploaded, and the public object must match it."""
    work = work.expanduser().resolve()
    binary = binary.expanduser().resolve()
    if source is None:
        tmp = work / 'tmp'
        tmp.mkdir(parents=True, exist_ok=True)
        env = {**os.environ, 'PATH': f'{binary.parent}:{os.environ["PATH"]}', 'TMPDIR': str(tmp)}
        subprocess.run(['bash', str(rm.ROOT / 'examples/brazil-fields/01-tiles.sh')],
                       cwd=work, env=env, check=True)
    archive = source.expanduser().resolve() if source else work / 'brazil.pmtiles'
    stats = json.loads(subprocess.run([str(binary), 'stats', '--json', str(archive)],
                                     capture_output=True, text=True, check=True).stdout)
    facts = {'url': MAP_URL, 'bytes': archive.stat().st_size,
             'sha256': rm.sha256_file(archive),
             'tiles': sum(z['tile_count'] for z in stats['per_zoom'])}
    # Never publish a map that disagrees with the tutorial's stated output.
    expected = gen_bench_docs().tutorial_tile_count(rm.ROOT)
    if facts['tiles'] != expected:
        raise ValueError(f"brazil.pmtiles has {facts['tiles']} tiles; examples/brazil-fields/README.md "
                         f"expects {expected:,}; fix the tutorial or the build before uploading")
    prov = rm.provenance([rm.parse_bin(f'head={binary}')], {})
    if source is not None:
        prov['notes'] = [f'Recorded from an existing archive ({archive.name}); not rebuilt by this run.']
        verify_url(MAP_URL, facts['bytes'])
    elif target.startswith('s3://'):
        subprocess.run(['aws', 's3', 'cp', str(archive), target, '--profile', 'source-coop'], check=True)
        verify_url(MAP_URL, facts['bytes'])
    else:
        dest = Path(target).expanduser() / archive.name
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(archive, dest)
        if dest.stat().st_size != facts['bytes']:
            raise ValueError(f'copied size mismatch: {dest}: expected {facts["bytes"]} bytes')
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps({'provenance': prov, 'archive': facts}, indent=2) + '\n')


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    b = sub.add_parser('build')
    b.add_argument('--corpus', required=True, type=Path)
    b.add_argument('--work', required=True, type=Path)
    b.add_argument('--out', required=True, type=Path)
    b.add_argument('--version', help='upload prefix (default: the run\'s release version)')
    u = sub.add_parser('upload')
    u.add_argument('--manifest', required=True, type=Path)
    u.add_argument('--work', required=True, type=Path)
    u.add_argument('--target', required=True)
    u.add_argument('--force', action='store_true', help='replace an existing version directory')
    m = sub.add_parser('tutorial-map')
    m.add_argument('--work', required=True, type=Path)
    m.add_argument('--out', required=True, type=Path)
    m.add_argument('--tylertoo', required=True, type=Path)
    m.add_argument('--target', default=os.environ.get('MAP_TARGET', MAP_TARGET))
    m.add_argument('--from', dest='source', type=Path,
                   help='record this already-published archive instead of rebuilding it')
    args = parser.parse_args(argv)
    try:
        if args.command == 'build':
            build(args.corpus, args.work, args.out, args.version)
            print(f'Wrote {args.out}')
        elif args.command == 'upload':
            upload(args.manifest, args.work, args.target, args.force)
        else:
            tutorial_map(args.work, args.out, args.tylertoo, args.target, args.source)
    except (ValueError, OSError, subprocess.CalledProcessError) as exc:
        parser.exit(1, f'{exc}\n')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
