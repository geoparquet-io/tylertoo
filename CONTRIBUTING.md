# Contributing to gpq-tiles

## Development Setup

```bash
git clone https://github.com/geoparquet-io/gpq-tiles.git
cd gpq-tiles
git config core.hooksPath .githooks
cargo build && cargo check
```

See [DEVELOPMENT.md](https://github.com/geoparquet-io/gpq-tiles/blob/main/DEVELOPMENT.md) for the day-to-day workflow,
Python setup, and how to run every CI gate locally.

## Commit Convention

[Conventional Commits](https://www.conventionalcommits.org/):

| Type | Description |
|------|-------------|
| `feat` | New feature (bumps minor) |
| `fix` | Bug fix (bumps patch) |
| `docs` | Documentation only |
| `perf` | Performance improvement |
| `refactor` | Code change (no feature/fix) |
| `test` | Tests only |
| `chore` | Maintenance |

## Pull Request Process

1. Branch from `main` (it is protected — no direct pushes).
2. Run the gates locally — CI enforces all of them as required checks:
   `cargo fmt --all --check`, `cargo clippy --all-targets
   --all-features -- -D warnings`, `cargo machete`, targeted tests,
   and (for Python changes) the ruff/mypy/stubtest/vulture/xenon/pytest
   suite via `uv run`. The full list with commands:
   [DEVELOPMENT.md → CI Gates](https://github.com/geoparquet-io/gpq-tiles/blob/main/DEVELOPMENT.md#ci-gates--and-how-to-run-them-locally).
3. Submit the PR; never bypass the pre-commit hooks (`--no-verify` is
   forbidden).

## Releasing (Maintainers)

### Prerequisites

1. **Commitizen** installed globally: `uv tool install commitizen`
2. **GitHub secrets** configured:
   - `CARGO_REGISTRY_TOKEN` from [crates.io/settings/tokens](https://crates.io/settings/tokens)
   - PyPI trusted publishing at [pypi.org](https://pypi.org/manage/project/gpq-tiles/settings/publishing/)

### Release Workflow

```bash
# 1. Create release branch from main
git checkout main && git pull
git checkout -b release/vX.Y.Z

# 2. Bump version (from repo root, NOT crates/python)
uv run cz bump --increment MINOR --changelog   # or PATCH/MAJOR

# 3. Verify build works
cargo check

# 4. Push and create PR
git push -u origin release/vX.Y.Z
gh pr create --title "Release vX.Y.Z" --body "Automated release"

# 5. Merge PR → release.yml auto-publishes
```

### What Commitizen Updates

The config lives in `.cz.toml` at the repo root (the single source of
truth for `version_files`). A bump updates:

| File | Pattern |
|------|---------|
| `Cargo.toml` | `version = "X.Y.Z"` (workspace) |
| `Cargo.toml` | `gpq-tiles-core = { ..., version = "X.Y.Z" }` in `[workspace.dependencies]` |
| `crates/python/pyproject.toml` | `version = "X.Y.Z"` |
| `.cz.toml` | its own `version` field |

The pre-commit hook and the CI Version Consistency job both fail if
these drift.

### Recovery

```bash
# If release fails, delete orphan tag
git push origin :refs/tags/vX.Y.Z

# Re-trigger manually
gh workflow run release.yml --ref main
```

### Common Issues

| Problem | Cause | Fix |
|---------|-------|-----|
| `failed to select version for gpq-tiles-core` | workspace dependency version not updated | Ensure the `[workspace.dependencies]` entry in `Cargo.toml` moved with the bump |
| `cz: command not found` | Commitizen not installed | `uv tool install commitizen` |
| Version Consistency job fails | Manual edit to one version file | Re-run `uv run cz bump` from the repo root |
