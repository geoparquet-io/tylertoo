# Contributing to tylertoo

Contributors agree to follow the
[Code of Conduct](https://github.com/geoparquet-io/tylertoo/blob/main/CODE_OF_CONDUCT.md).
Report security issues privately through
[SECURITY.md](https://github.com/geoparquet-io/tylertoo/blob/main/SECURITY.md).

For bugs and feature requests, use the
[issue templates](https://github.com/geoparquet-io/tylertoo/issues/new/choose).

## Development setup

```bash
git clone https://github.com/geoparquet-io/tylertoo.git
cd tylertoo
git config core.hooksPath .githooks
cargo build
```

[DEVELOPMENT.md](https://github.com/geoparquet-io/tylertoo/blob/main/DEVELOPMENT.md)
covers setup, daily development, and local CI checks.

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
2. Run CI checks locally. At minimum, run `cargo fmt --all --check`,
   `cargo clippy`, `cargo shear`, and targeted tests, plus the `uv run`
   suite for Python changes. CI requires these checks to pass.
   [DEVELOPMENT.md → CI gates](https://github.com/geoparquet-io/tylertoo/blob/main/DEVELOPMENT.md#ci-gates--and-how-to-run-them-locally)
   lists every gate with its command.
3. Open the PR with the
   [PR template](https://github.com/geoparquet-io/tylertoo/blob/main/.github/PULL_REQUEST_TEMPLATE.md).
   Never bypass the pre-commit hook with `--no-verify`.

The diff-scoped mutation check is advisory. Its job summary and sticky PR
comment list mutants in your change that tests missed. Use these to find
test gaps. Run `scripts/mutants-diff.sh` to reproduce the check locally.

## Docstrings and help text

For public docstrings, CLI help, and `///` docs:

- Open with a one-line summary that ends with a period.
- Describe what each parameter does for the user in at most two sentences.
- Leave out issue numbers, references to sweeps or decision files, restated
  defaults, internal crate names, memory formulas, and tuning history.
- Give a performance-only knob one sentence and a link to the
  [scaling guide](https://geoparquet-io.github.io/tylertoo/guides/scaling/).
  Put tippecanoe comparisons in the
  [tippecanoe guide](https://geoparquet-io.github.io/tylertoo/guides/tippecanoe/).
- Keep one short runnable example per function. Move removed material
  users still need to `docs/OVERVIEW_TUNING.md`, a guide, or
  `CHANGELOG.md`.

## Releasing (maintainers)

### Prerequisites

1. Install Commitizen: `uv tool install commitizen`.
2. Set the GitHub secret `CARGO_REGISTRY_TOKEN` from
   [crates.io/settings/tokens](https://crates.io/settings/tokens).
3. Configure [PyPI trusted publishing](https://pypi.org/manage/project/tylertoo/settings/publishing/).

### Release workflow

1. Branch from an up-to-date `main`:

   ```bash
   git checkout main
   git pull
   git checkout -b release/vX.Y.Z
   ```

2. Bump the version from the repo root, never from `crates/python`.
   Choose `MINOR`, `PATCH`, or `MAJOR`. Delete Commitizen's local tag so
   the workflow can tag the merged commit:

   ```bash
   uv run cz bump --increment MINOR --changelog
   git tag -d vX.Y.Z
   ```

3. Check the build:

   ```bash
   cargo check
   ```

4. Curate `CHANGELOG.md` (see below) and commit it.
5. Push the branch and open the PR:

   ```bash
   git push -u origin release/vX.Y.Z
   gh pr create --title "Release vX.Y.Z" \
     --body "Release vX.Y.Z"
   ```

6. After the merge, start the release from `main`:

   ```bash
   gh workflow run release.yml --ref main
   ```

`release.yml` reads the version from `Cargo.toml` on `main`, tags that
commit `vX.Y.Z`, publishes to crates.io and PyPI, and creates a GitHub
release with prebuilt CLI binaries.

A manual run tags the latest commit on `main`. If other PRs merged after
the release PR, tag the release PR's merge commit directly.
Pushing the tag starts the same workflow:

```bash
git fetch origin
git tag vX.Y.Z <merge-commit>
git push origin vX.Y.Z
```

Before building, the workflow stops the release if:

- the commit is not on `main`
- a pushed tag does not match the `Cargo.toml` version
- the run started on any ref other than `main` or the `vX.Y.Z` tag
- the `vX.Y.Z` tag already exists on a different commit

### Curating the changelog

`cz bump --changelog` generates entries from commits. Before releasing,
edit the new section of `CHANGELOG.md` for users:

- Group the entries under **Added**, **Changed**, **Fixed**, and
  **Performance**.
- Give each user-visible change one line with its PR reference.
- Combine clippy, fmt, CI, dependency bumps, and other maintenance into
  one "Internal" line.
- Leave older sections alone.

`docs/changelog.md` links to `CHANGELOG.md`; the docs site uses the same file.

### What Commitizen updates

The root `.cz.toml` defines Commitizen's config and `version_files`.
A bump updates:

| File | Pattern |
|------|---------|
| `Cargo.toml` | `version = "X.Y.Z"` (workspace) |
| `Cargo.toml` | `tylertoo-core = { ..., version = "X.Y.Z" }` in `[workspace.dependencies]` |
| `crates/python/pyproject.toml` | `version = "X.Y.Z"` |
| `.cz.toml` | its own `version` field |

Commitizen leaves `crates/python/uv.lock` unchanged. The pre-commit hook
runs `uv lock` and stages the updated project version. Keep the hook
enabled when committing the bump. If the hook has not run, use
`cd crates/python && uv lock`. The hook and CI Version Consistency job
fail if the versions disagree.

### Recovery

If the workflow fails after pushing the tag, fix the cause and rerun it
on that tag. It detects the tag without a release, skips completed steps,
and publishes the rest:

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
