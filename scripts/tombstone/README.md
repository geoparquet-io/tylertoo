# Tombstoning `gpq-tiles` / `gpq-tiles-core`

The project was renamed `gpq-tiles` → `tylertoo`. `tylertoo` and `tylertoo-core` are live on
crates.io and PyPI, but the **old names still sit at 0.6.0 with no forwarding address**:

```
crates.io  gpq-tiles       0.6.0  "CLI tool for converting GeoParquet to PMTiles vector tiles"
crates.io  gpq-tiles-core  0.6.0  "Core library for converting GeoParquet to PMTiles vector tiles"
PyPI       gpq-tiles       0.6.0  "Fast GeoParquet to PMTiles converter"
```

This directory holds everything needed to publish a **metadata-only 0.6.1 tombstone** so that
anyone who lands on the old package is pointed at tylertoo.

- [`gpq-tiles-0.6.1.patch`](gpq-tiles-0.6.1.patch) — the diff to apply on top of the `v0.6.0` tag.
- This file — the runbook.

Nothing here is wired into CI. It is run by hand, once, by a maintainer who holds the publishing
credentials. Tracking issue: **#409**.

## Design: why metadata-only

**0.6.1 must not break anyone pinned to `gpq-tiles ~0.6`.** So the tombstone is the *v0.6.0 source
tree* with only manifests and READMEs changed — no code, no dependency bumps, no API changes. A
user on `gpq-tiles = "0.6"` who picks up 0.6.1 gets byte-identical behaviour plus a rename notice.

**There is no Rust "deprecate the whole crate" mechanism.** `#[deprecated]` applies to items, not
crates; there is no crate-level attribute and no crates.io "this crate moved" field. Sprinkling
`#[deprecated]` over `gpq_tiles_core`'s public API would emit warnings in downstream builds —
noise, not a forwarding address, and it would mean touching code in a release whose whole promise
is that the code is unchanged. **The description and the README banner *are* the tombstone.**

## What the patch changes

| File | Change |
|---|---|
| `Cargo.toml` | workspace `version` 0.6.0 → 0.6.1; `gpq-tiles-core` dep version → 0.6.1; `repository`/`homepage` → `geoparquet-io/tylertoo` |
| `.cz.toml` | `version` → 0.6.1 (the old tree's version-consistency file) |
| `crates/python/pyproject.toml` | `version` → 0.6.1; `description` → rename notice; `Homepage`/`Repository` → tylertoo |
| `crates/core/Cargo.toml` | `description` → `RENAMED to tylertoo-core — …` |
| `crates/cli/Cargo.toml` | `description` → `RENAMED to tylertoo — … cargo install tylertoo …` |
| `crates/python/Cargo.toml` | `description` → rename notice (not published; kept consistent) |
| `crates/python/uv.lock` | the project's own pin 0.3.0 → 0.6.1 — it was already stale at v0.6.0; fixing it keeps `uv sync` in the scratch worktree from erroring out (same bug main fixed in #466) |
| `README.md`, `crates/{core,cli,python}/README.md` | blockquote banner under the H1: what happened, the one-line migration for both ecosystems, and "0.6.1 is metadata-only, your pin still works" |

The three crate-level READMEs are the ones crates.io and PyPI actually render
(`readme = "README.md"` is relative to each crate directory).

## Prerequisites

- crates.io token with publish rights on `gpq-tiles` and `gpq-tiles-core`
  (`cargo login`, or `CARGO_REGISTRY_TOKEN`).
- A PyPI credential for the `gpq-tiles` project — see [PyPI](#part-2--pypi) below.
- `protoc` on `PATH` (the old tree builds MVT protobufs via `prost-build`).
- A current stable Rust toolchain is fine. The old tree declares `rust-version = "1.75"`, but
  `cargo check --workspace` on the patched tree was **verified green on rustc 1.95.0 / libprotoc
  31.1**. No pinned old toolchain is needed. (v0.6.0 ships no `Cargo.lock`, so dependencies
  resolve fresh; if a future resolver picks a broken upgrade, add `--locked` after generating a
  lockfile, or pin with `cargo update -p <crate> --precise <ver>`.)

## Step 0 — scratch worktree at the v0.6.0 tree

Do **not** do this in your normal checkout; `main` is tylertoo, not gpq-tiles.

```bash
cd /path/to/tylertoo
git fetch --tags
git worktree add --detach /tmp/gpq-tombstone v0.6.0
cd /tmp/gpq-tombstone
git apply --check /path/to/tylertoo/scripts/tombstone/gpq-tiles-0.6.1.patch  # dry run
git apply         /path/to/tylertoo/scripts/tombstone/gpq-tiles-0.6.1.patch
git diff --stat   # expect 11 files changed, 69 insertions(+), 13 deletions(-)
```

The changes stay uncommitted — that is why every publish command below passes `--allow-dirty`.
Nothing gets committed to, or tagged in, the tylertoo repo.

Sanity check before publishing anything:

```bash
cargo check --workspace          # ~4 min cold
```

## Part 1 — crates.io

**Order matters.** `gpq-tiles` 0.6.1 depends on `gpq-tiles-core` **0.6.1**, so the library must be
on the index before the CLI will even package. (Verified: `cargo package -p gpq-tiles` fails with
`failed to select a version for the requirement gpq-tiles-core = "^0.6.1"` until core is up.)

```bash
cd /tmp/gpq-tombstone

# 1. library first
cargo publish -p gpq-tiles-core --allow-dirty

# 2. wait for the index to catch up (the old release.yml slept 30s; 60 is safer)
sleep 60

# 3. then the CLI
cargo publish -p gpq-tiles --allow-dirty
```

`gpq-tiles-python` is `publish`-irrelevant (it is a `cdylib` shipped through PyPI) — do not publish it.

Each `cargo publish` runs a full verification build. If that is too slow, `--no-verify` is safe
here *only* because `cargo check --workspace` already passed on this exact tree.

## Part 2 — PyPI

> [!IMPORTANT]
> **Ship wheels, not just an sdist.** 0.6.0 shipped three wheels
> (`cp311-macosx_11_0_arm64`, `cp311-win_amd64`, `cp38-manylinux_2_17_x86_64`) plus an sdist. If
> 0.6.1 is sdist-only, `pip install gpq-tiles` starts resolving to 0.6.1 and tries to **build from
> source** — which needs a Rust toolchain and `protoc`. That would turn a courtesy tombstone into
> a broken install for exactly the users we are trying to help. Reproduce the wheel set, or skip
> the PyPI tombstone entirely.

### Credentials: trusted publishing, or a token

v0.6.0's `release.yml` published with `pypa/gh-action-pypi-publish` over OIDC trusted publishing.
**Assume that is now broken for `gpq-tiles`**: a PyPI trusted publisher is pinned to
`owner/repository` + workflow filename, and the GitHub repo was renamed `gpq-tiles` → `tylertoo`,
so the OIDC claim no longer matches the stored publisher. (#409 notes trusted publishing works for
the *new* `tylertoo` project, which says nothing about the old one.) Two paths — check
<https://pypi.org/manage/project/gpq-tiles/settings/publishing/> to see which applies:

- **Trusted publishing still configured** (a publisher listed there whose repository is
  `geoparquet-io/tylertoo`, or you add one): publish from a GitHub Actions job with
  `permissions: id-token: write`. Easiest route is the tag path below, which reuses the old
  workflow verbatim.
- **No usable publisher** (the common case after a rename): create a
  [project-scoped API token](https://pypi.org/manage/account/token/) for `gpq-tiles` and upload by
  hand with twine (below). Delete the token afterwards.

### Option A — build locally, upload with twine

Builds only the wheel for *your* platform, so on its own this leaves the other platforms on the
sdist. Acceptable only if you also run Option B, or accept the caveat above.

```bash
cd /tmp/gpq-tombstone
uvx --from 'maturin>=1.0,<2.0' maturin sdist  --out dist -m crates/python/Cargo.toml
uvx --from 'maturin>=1.0,<2.0' maturin build  --release --out dist -m crates/python/Cargo.toml

uvx twine check dist/*
uvx twine upload dist/*          # __token__ / pypi-…  (or TWINE_USERNAME/TWINE_PASSWORD)
```

Verified locally: `maturin sdist` on the patched tree produces `gpq_tiles-0.6.1.tar.gz` whose
`PKG-INFO` carries `Summary: RENAMED to tylertoo — …`, `Home-Page:
https://github.com/geoparquet-io/tylertoo`, and the README banner as the long description.

### Option B — let the old CI build the full wheel matrix

The v0.6.0 tree's `.github/workflows/release.yml` fires on `push: tags: v[0-9]+.[0-9]+.[0-9]+`
and runs *the tagged tree's own workflow* — i.e. the old gpq-tiles release pipeline, with the
3-OS `PyO3/maturin-action` matrix and the PyPI publish job. Tagging the tombstone commit therefore
reproduces 0.6.0's exact distribution set:

```bash
cd /tmp/gpq-tombstone
git checkout -b release/gpq-tiles-0.6.1
# --no-verify: the v0.6.0 tree's own pre-commit hook would run current-toolchain
# clippy on the old tree and overwrite the subcrate READMEs via its sync step
git commit --no-verify -am "chore(release): tombstone gpq-tiles 0.6.1 -> tylertoo"
git push origin release/gpq-tiles-0.6.1
git tag -a v0.6.1 -m "gpq-tiles 0.6.1 (tombstone)" && git push origin v0.6.1
```

Caveats, all of them deliberate trade-offs — read before doing this:

- It creates a `v0.6.1` tag and a GitHub Release in the **tylertoo** repo, sorting oddly next to
  `v0.7.0`. Delete the release afterwards if it bothers you; leave the tag (it is a real
  published version).
- Its `release-rust` job re-runs `cargo publish`. If you already did Part 1, that job **fails** on
  "crate version already uploaded" — harmless: `build-wheels` / `build-sdist` / `publish-python`
  do not `needs:` it and still run. To let CI do everything, skip Part 1 and just tag.
- It needs `CARGO_REGISTRY_TOKEN` (repo secret, still present) and a working PyPI credential — see
  above. There is **no secret fallback**: the old workflow's publish step is
  `pypa/gh-action-pypi-publish` with no `password:` input, so a `PYPI_API_TOKEN` secret would
  never reach it. If trusted publishing is dead, either (re)add a PyPI trusted publisher for the
  `gpq-tiles` project pointing at `geoparquet-io/tylertoo` / `release.yml` before tagging, or let
  `publish-python` fail, download the run's wheel and sdist artifacts, and `twine upload` them
  manually.
- The workflow uses floating action tags (`actions/checkout@v6`, …), which today's `zizmor` gate on
  `main` would reject. That gate does not run on a tag push of an old tree, so it is not blocking —
  but do not "fix" the old workflow; leave the tagged tree as-is.

## Verification

```bash
curl -s https://crates.io/api/v1/crates/gpq-tiles \
  | python3 -c 'import json,sys;d=json.load(sys.stdin)["crate"];print(d["max_version"],"|",d["description"])'
# expect: 0.6.1 | RENAMED to tylertoo — …

curl -s https://crates.io/api/v1/crates/gpq-tiles-core \
  | python3 -c 'import json,sys;d=json.load(sys.stdin)["crate"];print(d["max_version"],"|",d["description"])'
# expect: 0.6.1 | RENAMED to tylertoo-core — …

curl -s https://pypi.org/pypi/gpq-tiles/json \
  | python3 -c 'import json,sys;d=json.load(sys.stdin)["info"];print(d["version"],"|",d["summary"])'
# expect: 0.6.1 | RENAMED to tylertoo — …

# wheel coverage matches 0.6.0 (3 wheels + sdist), not sdist-only
curl -s https://pypi.org/pypi/gpq-tiles/0.6.1/json \
  | python3 -c 'import json,sys;[print(u["packagetype"],u["filename"]) for u in json.load(sys.stdin)["urls"]]'
```

Then eyeball the rendered banner at <https://crates.io/crates/gpq-tiles> and
<https://pypi.org/project/gpq-tiles/>, and confirm existing pins are untouched — 0.6.0 must still
be listed and un-yanked in both `versions` listings above, and `pip download gpq-tiles==0.6.0`
must still fetch the 0.6.0 wheel.

Finally, close **#409** and tear down the worktree:

```bash
cd /path/to/tylertoo && git worktree remove --force /tmp/gpq-tombstone
```

## Rollback

- **crates.io: versions can be yanked, never deleted.** `cargo yank --version 0.6.1 gpq-tiles`
  (and `gpq-tiles-core`) stops new dependency resolution from selecting 0.6.1 while leaving it
  downloadable for anyone who already locked it. Un-yank with `--undo`. The 0.6.1 *number* is
  burned either way — a corrected tombstone has to go out as 0.6.2.
- **PyPI: yank first, delete only if you must.** Yank via
  <https://pypi.org/manage/project/gpq-tiles/releases/> — `pip` then ignores 0.6.1 unless someone
  pins it exactly. Deleting a release frees nothing: the filename and version can never be
  re-uploaded.
- **Blast radius is small by construction.** The tombstone ships v0.6.0's code verbatim, so the
  worst realistic failure is cosmetic (bad wording, a broken link) — or the wheel-coverage
  regression called out above, which is the one thing actually worth rolling back for.
