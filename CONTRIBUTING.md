# Contributing to tylertoo

Everyone who takes part in this project agrees to follow the
[Code of Conduct](https://github.com/geoparquet-io/tylertoo/blob/main/CODE_OF_CONDUCT.md).
To report a security issue privately, follow
[SECURITY.md](https://github.com/geoparquet-io/tylertoo/blob/main/SECURITY.md).

To file a bug or a feature request, use the
[issue templates](https://github.com/geoparquet-io/tylertoo/issues/new/choose).
They ask for the details a maintainer needs to act on the report.

## Development setup

```bash
git clone https://github.com/geoparquet-io/tylertoo.git
cd tylertoo
git config core.hooksPath .githooks
cargo build
```

[DEVELOPMENT.md](https://github.com/geoparquet-io/tylertoo/blob/main/DEVELOPMENT.md)
covers the rest of the setup, the day-to-day workflow, and how to run
each CI gate locally.

## Commit convention

Commit messages follow
[Conventional Commits](https://www.conventionalcommits.org/):

| Type | Use for |
|------|---------|
| `feat` | A new feature (bumps minor) |
| `fix` | A bug fix (bumps patch) |
| `docs` | Documentation only |
| `perf` | A performance improvement |
| `refactor` | A code change that is neither a feature nor a fix |
| `test` | Tests only |
| `chore` | Maintenance |

## Pull request process

1. Branch from `main`. Branch protection rejects direct pushes.
2. Run the gates locally. CI enforces each one as a required check. At
   minimum, run `cargo fmt --all --check`, `cargo clippy`, `cargo shear`,
   and targeted tests, plus the `uv run` suite for Python changes.
   [DEVELOPMENT.md → CI gates](https://github.com/geoparquet-io/tylertoo/blob/main/DEVELOPMENT.md#ci-gates--and-how-to-run-them-locally)
   lists every gate with its command.
3. Open the PR with the
   [PR template](https://github.com/geoparquet-io/tylertoo/blob/main/.github/PULL_REQUEST_TEMPLATE.md).
   Never bypass the pre-commit hook with `--no-verify`.

One check is advisory. The diff-scoped mutation run lists the mutants in
your change that no test catches, in its job summary and in a sticky PR
comment. Treat it as a hint about a missing test, not as a failure.
`scripts/mutants-diff.sh` reproduces it locally.

## Docstrings and help text

Public docstrings, CLI help, and `///` docs follow one standard:

- Open with a one-line summary that ends with a period.
- Describe each parameter in at most two sentences, on what it does for
  the user.
- Leave out issue numbers, references to sweeps or decision files, restated
  defaults, internal crate names, memory formulas, and tuning history.
- Give a performance-only knob one sentence and a link to the
  [scaling guide](https://geoparquet-io.github.io/tylertoo/guides/scaling/).
  Put tippecanoe comparisons in the
  [tippecanoe guide](https://geoparquet-io.github.io/tylertoo/guides/tippecanoe/).
- Keep one short runnable example per function. Move removed material
  that users still need to `docs/OVERVIEW_TUNING.md`, a guide, or
  `CHANGELOG.md`.

## Releasing (maintainers)

### Prerequisites

1. Install Commitizen: `uv tool install commitizen`.
2. Configure the GitHub secrets:
   - `CARGO_REGISTRY_TOKEN` from [crates.io/settings/tokens](https://crates.io/settings/tokens)
   - PyPI trusted publishing at [pypi.org](https://pypi.org/manage/project/tylertoo/settings/publishing/)

### Release workflow

```bash
# 1. Branch from an up-to-date main
git checkout main
git pull
git checkout -b release/vX.Y.Z

# 2. Bump the version from the repo root, never
#    from crates/python (or PATCH, or MAJOR)
uv run cz bump --increment MINOR --changelog
# cz also tags this commit locally. Drop that tag:
# the workflow tags the merged commit instead
git tag -d vX.Y.Z

# 3. Check the build
cargo check

# 4. Curate CHANGELOG.md (see below) and commit it

# 5. Push the branch and open the PR
git push -u origin release/vX.Y.Z
gh pr create --title "Release vX.Y.Z" \
  --body "Release vX.Y.Z"

# 6. After the merge, start the release from main
gh workflow run release.yml --ref main
```

`release.yml` reads the version from `Cargo.toml` on `main`, tags that
commit `vX.Y.Z`, and publishes to crates.io and PyPI. It also creates a
GitHub release with the prebuilt CLI binaries.

A manual run tags the newest commit on `main`. If other PRs merged after
the release PR, tag the release PR's merge commit yourself instead.
Pushing the tag starts the same workflow:

```bash
git fetch origin
git tag vX.Y.Z <merge-commit>
git push origin vX.Y.Z
```

The workflow's first job checks the source before anything builds. It
stops the release when:

- the commit is not on `main`
- a pushed tag does not match the `Cargo.toml` version
- the run started on any ref other than `main` or the `vX.Y.Z` tag
- the `vX.Y.Z` tag already exists on a different commit

### Curating the changelog

`cz bump --changelog` writes a raw dump of the commits. Before the
release, rewrite the new section of `CHANGELOG.md` for users:

- Group the entries under **Added**, **Changed**, **Fixed**, and
  **Performance**.
- Give each user-visible change one line with its PR reference.
- Fold the development noise, such as clippy, fmt, CI, and dependency
  bumps, into one "Internal" line.
- Leave older sections alone.

`docs/changelog.md` is a symlink to `CHANGELOG.md`, so the docs site
picks up the edit with no second copy to sync.

### What Commitizen updates

`.cz.toml` at the repo root holds the config and the single list of
`version_files`. A bump updates:

| File | Pattern |
|------|---------|
| `Cargo.toml` | `version = "X.Y.Z"` (workspace) |
| `Cargo.toml` | `tylertoo-core = { ..., version = "X.Y.Z" }` in `[workspace.dependencies]` |
| `crates/python/pyproject.toml` | `version = "X.Y.Z"` |
| `.cz.toml` | its own `version` field |

It does **not** update `crates/python/uv.lock`, which pins the Python
project's own version. The pre-commit hook runs `uv lock` and stages the
result, so commit the bump with the hook enabled. Without the hook, run
`cd crates/python && uv lock` yourself. The pre-commit hook and the CI
Version Consistency job both fail when these versions drift.

### Recovery

If the workflow fails after it pushed the tag, fix the cause and run it
again on the tag. It finds the tag without a release, skips the steps
that already ran, and publishes the rest:

```bash
gh workflow run release.yml --ref vX.Y.Z
```

If the tag points at the wrong commit, delete it before you rerun:

```bash
git push origin :refs/tags/vX.Y.Z
```

### Common issues

| Problem | Cause | Fix |
|---------|-------|-----|
| `failed to select version for tylertoo-core` | The workspace dependency version did not move | Check that the `[workspace.dependencies]` entry in `Cargo.toml` moved with the bump |
| `cz: command not found` | Commitizen is not installed | `uv tool install commitizen` |
| Version Consistency job fails | Someone edited one version file by hand | Rerun `uv run cz bump` from the repo root |
| `uv.lock needs to be updated, but --locked was provided` | The bump did not refresh the lock file | `cd crates/python && uv lock`, then commit `uv.lock` |
| `No pyproject.toml found` from `uv lock` | `uv lock` ran from the repo root | Run it in `crates/python` |
