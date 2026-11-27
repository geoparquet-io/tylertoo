"""Release selection and preview tests; no stages, git, or network run."""
import json
import os
import re
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
STAGES = 'build tippecanoe corpus cells africa tutorial-map upload docs'.split()


@pytest.fixture
def preview(tmp_path):
    tools = tmp_path / 'bin'
    tools.mkdir()
    # Delegate only Python to the test interpreter; never resolve/download uv tools.
    (tools / 'uv').write_text(f'#!/bin/sh\nwhile [ "$1" != python ]; do shift; done\nshift\nexec {sys.executable} "$@"\n')
    (tools / 'git').write_text('''#!/bin/sh
case "$*" in
  *"rev-parse HEAD"*) echo abc123 ;;
  *"status --porcelain"*) [ "${TEST_RC:-0}" = 1 ] || printf ' M source.rs\\0' ;;
  *"tag --list"*) : ;;
  *"describe"*) echo v-old-1-gabc123 ;;
  *) exit 1 ;;
esac
''')
    for tool in tools.iterdir():
        tool.chmod(0o755)
    data = tmp_path / 'data with spaces'
    def run(*args, **env):
        return subprocess.run(['bash', str(ROOT / 'benchmarks/release.sh'), '--data', str(data),
                               '--dry-run', *args], cwd=ROOT, text=True, capture_output=True,
                              env={**os.environ, 'SKIP': '', 'ONLY': '', 'PATH': f'{tools}:{os.environ["PATH"]}', **env})
    return run, data


def selected(result):
    assert result.returncode == 0, result.stderr
    return re.search(r'^Stages: (.*)$', result.stdout, re.M)[1].split()


@pytest.mark.parametrize('args,env,want', [
    ([], {}, STAGES),
    (['--skip', 'africa'], {}, [s for s in STAGES if s != 'africa']),
    (['--only', 'cells docs'], {}, ['cells', 'docs']),
    (['--only', 'cells', 'docs'], {}, ['cells', 'docs']),
    (['docs', 'cells'], {}, ['cells', 'docs']),
    ([], {'SKIP': 'africa'}, [s for s in STAGES if s != 'africa']),
    ([], {'ONLY': 'docs cells'}, ['cells', 'docs']),
])
def test_selection(preview, args, env, want):
    run, data = preview
    assert selected(run(*args, **env)) == want
    assert not data.exists()  # no lock, directories, or completion stamps


@pytest.mark.parametrize('args', [['unknown'], ['--only', 'unknown'], ['--skip', 'unknown']])
def test_unknown_stage(preview, args):
    result = preview[0](*args)
    assert result.returncode == 2
    assert ' '.join(STAGES) in result.stderr


def test_done_and_force(preview):
    run, data = preview
    out = data / 'release' / 'v-old-1-gabc123'
    out.mkdir(parents=True)
    (out / 'cells.done').touch()
    assert 'cells: skip (done)' in run('cells').stdout
    assert 'cells: skip (done)' not in run('--force', 'cells').stdout
    assert 'run --set release' in run('--force', 'cells').stdout


def test_non_candidate_refuses_publication(preview):
    result = preview[0]()
    assert 'upload: refuse' in result.stdout and 'docs: refuse' in result.stdout
    assert 'not a release candidate' in result.stdout
    assert 'v-old-1-gabc123' in result.stdout
    assert 'tutorial-map: refuse' in result.stdout


def test_tutorial_map_preview_and_resume(preview):
    run, data = preview
    result = run('--only', 'tutorial-map', MAP_TARGET='/tmp/local-map')
    assert selected(result) == ['tutorial-map']
    assert 'comparison.py tutorial-map' in result.stdout
    assert '--target /tmp/local-map' in result.stdout
    assert 'tutorial-map.json' in result.stdout
    assert 'scratch' not in result.stdout
    out = data / 'release/v-old-1-gabc123'
    out.mkdir(parents=True)
    (out / 'tutorial-map.done').touch()
    assert 'tutorial-map: skip (done)' in run('tutorial-map').stdout
    assert 'comparison.py tutorial-map' in run('--force', 'tutorial-map').stdout


def test_tutorial_map_refuses_non_candidate_before_any_tools(preview):
    _, data = preview
    tools = data.parent / 'bin'
    result = subprocess.run(['bash', str(ROOT / 'benchmarks/release.sh'), '--data', str(data),
                             '--only', 'tutorial-map'], cwd=ROOT, text=True, capture_output=True,
                            env={**os.environ, 'TEST_RC': '0', 'SKIP': '', 'ONLY': '',
                                 'PATH': f'{tools}:{os.environ["PATH"]}'})
    assert result.returncode == 1
    assert 'tutorial-map, upload, and docs refuse' in result.stderr
    assert not list(data.rglob('*.done'))
    assert not list(data.rglob('*.pmtiles'))


def test_data_env_default_and_explicit_override(preview, tmp_path):
    run, explicit = preview
    tools = explicit.parent / 'bin'
    env = {**os.environ, 'SKIP': '', 'ONLY': '', 'PATH': f'{tools}:{os.environ["PATH"]}',
           'TYLERTOO_BENCH_DATA': str(tmp_path / 'env-data')}
    result = subprocess.run(['bash', str(ROOT / 'benchmarks/release.sh'), '--dry-run'],
                            cwd=ROOT, env=env, capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    out = tmp_path / 'env-data/release/v-old-1-gabc123'
    assert f'Output: {out}' in result.stdout
    assert f'--bench-out {out}/scratch' in result.stdout
    assert f'--work {out}/corpus' in result.stdout
    assert f'--work {out}/tutorial-map' in result.stdout
    assert f'free under {tmp_path}/env-data' in result.stdout
    assert not (tmp_path / 'env-data').exists()
    assert f'Output: {explicit}/release/' in run(TYLERTOO_BENCH_DATA='/unused').stdout
    help_text = run('--help').stdout
    assert 'TYLERTOO_BENCH_DATA' in help_text and '$HOME/bench-data' in help_text


def test_preflights_and_commands(preview):
    result = preview[0](TEST_RC='1')
    for command in ['release-version', 'check-pins', '--version', 'aws s3 ls',
                    'source-coop login', 'cargo build --locked --release -p tylertoo',
                    '--history-from', '--diff origin/main', 'comparison.py upload']:
        assert command in result.stdout
    script = (ROOT / 'benchmarks/release.sh').read_text()
    disk = re.search(r'^AFRICA_DISK_GB=(\d+)', script, re.M)[1]
    assert re.search(rf'at least {disk} GB', result.stdout)
    corpus = json.loads((ROOT / 'benchmarks/results/e2e-corpus.json').read_text())
    for did in corpus['datasets']:
        assert f'--only {did}' in result.stdout
    assert 'check-pins' not in preview[0]('--skip', 'africa').stdout


def test_documented_release_flags_exist(preview):
    help_text = preview[0]('--help').stdout
    for flag in set(re.findall(r'--[a-z][a-z-]*', help_text)):
        assert flag in {'--data', '--force', '--dry-run', '--skip', '--only', '--help'}


def test_skipped_africa_keeps_committed_provenance_in_generated_page(preview, tmp_path):
    """Exercise setup and docs in an isolated copy, with setup, tools, and git stubbed."""
    import shutil
    import tomllib

    run, data = preview
    root = tmp_path / 'repo'
    for directory in ('benchmarks/results', 'benchmarks/e2e', 'benchmarks/coalesce', 'scripts'):
        (root / directory).mkdir(parents=True)
    for path in ('Cargo.toml', 'benchmarks/release.sh', 'benchmarks/run_manifest.py',
                 'benchmarks/e2e/run_e2e.py', 'benchmarks/e2e/setup_tippecanoe.sh',
                 'benchmarks/coalesce/bench.toml', 'scripts/gen_bench_docs.py', 'README.md',
                 'crates/core/README.md', 'crates/cli/README.md', 'crates/python/README.md', 'docs/benchmarks.md'):
        (root / path).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / path, root / path)
    for path in (ROOT / 'benchmarks/results').glob('*.json'):
        shutil.copyfile(path, root / 'benchmarks/results' / path.name)
    africa = root / 'benchmarks/results/overture-buildings-africa.json'
    original = africa.read_bytes()
    old = json.loads(original)['provenance']
    version = tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['package']['version']
    out = data / 'release' / version
    out.mkdir(parents=True)
    corpus = json.loads((root / 'benchmarks/results/e2e-corpus.json').read_text())
    corpus['provenance']['release_version'] = version
    (out / 'e2e-corpus.json').write_text(json.dumps(corpus))
    # The comparison file is copied along with the measurements.
    (out / 'comparison-pmtiles.json').write_text(json.dumps({'provenance': corpus['provenance']}))
    pin = re.search(r'^DEFAULT_TAG="([^"]+)"', (root / 'benchmarks/e2e/setup_tippecanoe.sh').read_text(), re.M)[1]
    tip = root / f'.tools/tippecanoe-{pin}/bin/tippecanoe'
    setup = root / 'benchmarks/e2e/setup_tippecanoe.sh'
    setup.write_text(f'''#!/bin/sh
DEFAULT_TAG="{pin}"
mkdir -p "{tip.parent}"
printf '#!/bin/sh\\necho "tippecanoe v%s"\\n' "$DEFAULT_TAG" > "{tip}"
chmod +x "{tip}"
''')
    setup.chmod(0o755)
    tools = data.parent / 'bin'
    gpio_pin = tomllib.loads((root / 'benchmarks/coalesce/bench.toml').read_text())['tippecanoe']['gpio'].split('==')[1]
    (tools / 'uvx').write_text(f'#!/bin/sh\necho "geoparquet-io, version {gpio_pin}"\n')
    (tools / 'uvx').chmod(0o755)
    result = subprocess.run(['bash', str(root / 'benchmarks/release.sh'), '--data', str(data),
                             '--skip', 'africa', '--only', 'tippecanoe docs'], cwd=root,
                            capture_output=True, text=True, env={**os.environ, 'TEST_RC': '1',
                            'SKIP': '', 'ONLY': '', 'PATH': f'{tools}:{os.environ["PATH"]}'})
    assert result.returncode == 0, result.stderr + result.stdout
    assert africa.read_bytes() == original
    page = (root / 'docs/benchmarks.md').read_text()
    assert old['commit'] in page and old['date'][:10] in page
    assert (out / 'docs.done').exists()
    assert (root / 'benchmarks/results/comparison-pmtiles.json').exists()
