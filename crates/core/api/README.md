# Public API baseline

`tylertoo-core.txt` is the committed snapshot of every item `tylertoo-core`
exports. CI regenerates it on each PR and fails when the two differ, so a
change to the public surface shows up in review as a diff rather than as a
silent export.

The gate does not forbid growth. It asks for the growth to be deliberate. When
a PR changes the API on purpose, regenerate the baseline in that same PR:

```bash
cd crates/core
cargo +nightly-2026-09-15 public-api \
  --simplified > api/tylertoo-core.txt
```

`cargo-public-api` reads rustdoc JSON, which only nightly emits. The
`--simplified` flag drops auto-derived trait impls, which otherwise dominate
the diff for any new type.

The nightly is pinned, and the pin is the `NIGHTLY` variable in the
`public-api` job in `.github/workflows/ci.yml`. Rustdoc renders paths
differently from one nightly to the next, so a floating toolchain reports
drift that no source change caused: `std::io::Error` began printing as
`core::io::error::Error`, and every line mentioning it turned into a diff.
Bump the pin on purpose, and regenerate this baseline in the same commit.

Breaking changes are a separate gate: `cargo semver-checks` runs in the
`audit` job and compares against main. Its baseline is a git revision, not
crates.io, because tylertoo-core is unpublished.
