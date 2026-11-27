#!/usr/bin/env python3
"""Build and publish the release corpus's comparison archives."""
from __future__ import annotations

import argparse
import json
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


def component(value: str) -> str:
    """Reject names that could escape the immutable version directory."""
    if not isinstance(value, str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._+-]*', value):
        raise ValueError(f'invalid archive path component: {value!r}')
    return value


def archive_path(work: Path, did: str, tool: str) -> Path:
    return work / component(did) / f'{tool}.pmtiles'


def build(corpus: Path, work: Path, out: Path) -> dict:
    source = json.loads(corpus.read_text())
    prov = {k: source['provenance'].get(k) for k in rm.PROVENANCE_KEYS}
    version = component(prov['release_version'])
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


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    b = sub.add_parser('build')
    b.add_argument('--corpus', required=True, type=Path)
    b.add_argument('--work', required=True, type=Path)
    b.add_argument('--out', required=True, type=Path)
    u = sub.add_parser('upload')
    u.add_argument('--manifest', required=True, type=Path)
    u.add_argument('--work', required=True, type=Path)
    u.add_argument('--target', required=True)
    u.add_argument('--force', action='store_true', help='replace an existing version directory')
    args = parser.parse_args(argv)
    try:
        if args.command == 'build':
            build(args.corpus, args.work, args.out)
            print(f'Wrote {args.out}')
        else:
            upload(args.manifest, args.work, args.target, args.force)
    except (ValueError, OSError, subprocess.CalledProcessError) as exc:
        parser.exit(1, f'{exc}\n')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
