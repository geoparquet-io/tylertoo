# Gate canaries

Each `*.patch` here injects one regression that a CI gate must catch. The
nightly `gate-canary.yml` applies each patch to a clean checkout and requires
its gate to go red; a canary that stays green means the gate has gone
vacuous (#678, #695: gates that silently stopped checking anything).

## G1 structural gates (#779)

The `g1-*.patch` files belong to the structural gates in the Check job of
`.github/workflows/ci.yml`. Each one is a plain `git diff`, applied with
`git apply` from the repository root. Its preamble (the text before the
first `diff --git` line, which `git apply` ignores) holds three headers:

- `Gate:` the gate the patch must turn red.
- `Expect-fail:` one shell command, run from the repository root after the
  patch is applied, that must exit non-zero. A canary passes when this
  command fails.
- `Catches:` the regression or issue the gate stands guard over.

The patches add new files or touch only lines that this effort owns (the
end of `crates/core/src/lib.rs`, `deny.toml`'s skip list), so they keep
applying as the rest of the tree changes. If one stops applying, regenerate
it: make the edit by hand, `git diff > ci/canaries/g1-<gate>.patch`, put
the preamble back, and confirm the command fails.

| Patch | Gate | Injected regression | Catches |
|---|---|---|---|
| `g1-clippy-disallowed.patch` | clippy `disallowed-methods` | a production `ParquetRecordBatchReaderBuilder::try_new` | #577 → #606/#607 |
| `g1-architecture-ratchet.patch` | `crates/core/tests/architecture.rs` | a new `bail!` in the CLI | #458, #709 (validation outside core) |
| `g1-cargo-deny.patch` | `cargo deny check bans` | a duplicate crate version not in the skip list | #649 |
| `g1-similarity.patch` | `scripts/ci/similarity_ratchet.py` | two near-identical functions in one file | #507 / 8576c61 class |
| `g1-mirrored-hunks.patch` | `scripts/ci/mirrored_hunks.py` | one block pasted into two files in one commit | #507 / 8576c61, #743 |

To run one by hand:

```bash
git apply ci/canaries/g1-cargo-deny.patch
sed -n 's/^Expect-fail: //p' ci/canaries/g1-cargo-deny.patch | sh   # must exit non-zero
git apply -R ci/canaries/g1-cargo-deny.patch
```

The mirrored-hunks canary's command commits the patch first (the check
compares commits) and then resets that commit, leaving the tree as it was.
