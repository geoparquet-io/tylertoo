## What / Why

<!-- What does this change, and why is it needed? -->

Closes #

## Checklist

Local CI gates (see [DEVELOPMENT.md → CI Gates](https://github.com/geoparquet-io/tylertoo/blob/main/DEVELOPMENT.md#ci-gates--and-how-to-run-them-locally) for the full list):

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] Targeted tests for the change pass (see DEVELOPMENT.md — never the full
      `cargo test` suite locally)
- [ ] Generated docs regenerated if CLI help/output changed:
      `cargo run -p tylertoo --features gen-docs -- gen-reference-docs > docs/reference/cli.md`
- [ ] Version files untouched (or bumped only via `uv run cz bump` from the
      repo root, per CONTRIBUTING.md), if applicable
