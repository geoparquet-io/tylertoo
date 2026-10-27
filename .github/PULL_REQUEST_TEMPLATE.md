## What / Why

<!-- What does this change, and why is it needed? -->

Closes #

## Concept owner / sites

<!--
For changes to shared concepts, describe the owner in core and the callers
that consume it. Duplication and lint checks are listed in ci/README.md.
-->

- Concept:
- Owner:
- Sites routed through it (former copies removed):

## Checklist

Local CI gates (see [ci/README.md](https://github.com/geoparquet-io/tylertoo/blob/main/ci/README.md) for every gate and the command that runs it):

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] Targeted tests for the change pass (see DEVELOPMENT.md — never the full
      `cargo test` suite locally)
- [ ] Generated docs regenerated if CLI help/output changed:
      `cargo run -p tylertoo --features gen-docs -- gen-reference-docs > docs/reference/cli.md`
- [ ] Version files untouched (or bumped only via `uv run cz bump` from the
      repo root, per CONTRIBUTING.md), if applicable
- [ ] Rust/Python duplication checks pass (`npx --yes jscpd@4.0.5` and
      similarity-rs; see DEVELOPMENT.md)
- [ ] Diff-scoped cargo-mutants passes for core and CLI
      (`scripts/mutants-diff.sh origin/main`)
