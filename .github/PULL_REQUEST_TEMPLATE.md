## What / Why

<!-- What does this change, and why is it needed? -->

Closes #

## Concept owner / sites (required)

<!--
Which concept does this PR touch (an option, a counter, a reader, a
resolver, an error, a pipeline stage)? Name its one owner in crates/core,
and list every site that now goes through that owner. Find the sites with
`git grep` across core (both engines), the CLI, Python and pyramid.
If the concept lived in more than one place, this PR consolidates it; it
never patches one site. See CLAUDE.md, "Fix at the Owner".
"None" is a valid answer only for docs, CI or test-only changes.
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
- [ ] **Baseline-change:** if this PR loosens a band in
      `benchmarks/e2e/tippecanoe_tolerances.toml`, or changes
      `benchmarks/overview/ci_baseline.json`,
      `tests/fixtures/guard/*.golden.txt` or `tests/fixtures/golden/*`, a
      commit carries `Baseline-change: #NNN <measured evidence>`. Otherwise
      the Gate scripts job fails.
- [ ] **fix PRs:** every mutant on a changed line is caught, or annotated
      `// mutants: equivalent #NNN` (`scripts/mutants-diff.sh`; the
      Mutation gate blocks a `fix` PR on a missed one)
