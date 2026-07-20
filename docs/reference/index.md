# Reference

Complete, lookup-oriented API surface. Every page here is **generated from the
source** (CLI help strings, Python docstrings, Rust doc-comments) and kept in
sync by CI, so it cannot drift from the implementation.

- [CLI reference](cli.md) — every `tylertoo` subcommand, argument, and option,
  with defaults and possible values.
- [Python reference](python.md) — the `tylertoo` package: `overview`,
  `export_pmtiles`, `validate`, and the `convert` facade.
- [Rust reference](rust.md) — embedding `tylertoo-core`, with a pointer to the
  crate's rustdoc on docs.rs.

Coverage is complete per surface, but the three surfaces are not literally 1:1
(for example, the CLI exposes `decode`, which the Python module does not). Where
a capability exists on one surface but not another, that is stated rather than
hidden.

For the *why* behind the knobs — the mental model rather than the flag list —
see [Diving Deeper](../diving-deeper/index.md).
