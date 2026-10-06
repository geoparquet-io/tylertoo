# Reference

Lookup pages for each surface. CI regenerates the CLI and Python pages from
the source on every pull request and fails if the committed copies drift.

- [CLI reference](cli.md): every `tylertoo` subcommand, argument, and option,
  with defaults and possible values. Generated from the clap help strings.
- [Python reference](python.md): `overview`, `export_pmtiles`, `validate`,
  and the `convert` facade. Generated from the pyo3 docstrings.
- [Rust reference](rust.md): the entry points of `tylertoo-core`, with links
  to its rustdoc on docs.rs.
- [Tuning reference](../OVERVIEW_TUNING.md): what each generalization knob
  does, its default, and how the knobs interact.

The surfaces are not one to one. The CLI has `decode`, `stats`, `pyramid`,
`merge`, and `shard-plan`; the Python module does not expose them yet.

For a guided path through the main commands, start with the
[Madagascar tutorial](../tutorials/madagascar.md) or the
[Brazil tutorial](../tutorials/brazil.md).
