# Gate canaries

A canary is a patch that injects the regression a CI gate exists to
catch. Every night, `.github/workflows/gate-canary.yml` applies each
canary to `main`, runs the canary's gate, and **fails if the gate
passes**. A gate that can't see its own regression is vacuous, and
nobody notices a vacuous gate until a bug walks through it:

- #695: the pre-commit clippy check went through `| tail`, so the
  check passed whatever clippy found.
- #419 and #677: the weekly mutation sweep was red for 15 weeks.
- #678: the bench gate ran once in 166 runs.

The workflow picks up every `ci/canaries/*.patch` with no edit to the
workflow. The runner is `scripts/ci/canary.py`.

## Format

A canary is the output of `git diff` with a header of `# key: value`
lines in front of it. `git apply` skips the header.

```text
# gate: uv run scripts/ci/canary.py ci-step ci.yml check "Check formatting"
# expect: Diff in .*crates/core/src/lib\.rs
# issue: #779 (the rustfmt step of the Check job)
diff --git a/crates/core/src/lib.rs b/crates/core/src/lib.rs
...
```

| Key | Required | Meaning |
|---|---|---|
| `gate` | yes, first line | The gate's command. It runs from the repository root through `bash -o pipefail`, after the patch has been applied and committed. A non-zero exit means the gate went red. |
| `expect` | no, but use it | A Python regex that the gate's combined stdout and stderr must match. Without it, a gate that fails for an unrelated reason, such as a build error or a missing tool, counts as a pass. |
| `issue` | no | The issue or gate the canary proves. It shows in the job log and summary. |
| `commit` | no | The commit message for the applied patch. The default is `canary: <name>`. Use it for gates that read commit messages, such as the baseline ratchet, whose trailer check must find no `Baseline-change:` line. |
| `setup` | no | Extra tools to install, separated by spaces: `tippecanoe` (the pinned build, on `PATH`) and `go-pmtiles` (the pinned `pmtiles` binary, on `PATH`). Every canary job already has stable Rust with rustfmt and clippy, protoc, uv, the `fixtures-v1` Parquet files, the submodule, and main's warm `target/` cache. |

The canary passes only when all of these hold:

1. The patch applies to the tree. A patch that no longer applies has
   rotted. Regenerate it, as described below.
2. The gate exits non-zero, and not with 126 or 127, which mean the
   command couldn't run.
3. The output matches `expect`, if the canary sets one.

The runner commits the patch before it runs the gate. So `HEAD~1..HEAD`
is the canary's change, both for gates that read the diff or the commit
log and for gates that read the working tree.

### Run the gate exactly as CI does

`canary.py ci-step <workflow> <job> <step name>` runs the `run:` script
of one workflow step through GitHub's default shell (`bash -e -o
pipefail`), with the step's `env:` and `working-directory:`. Prefer it
to a copy of the command. A canary that copies the command keeps
passing after someone breaks the gate's own step in the workflow, which
is how #695 happened. A canary that runs the step itself goes red
instead. `ci-step` refuses a step whose script contains a `${{ }}`
expression, because the runner can't evaluate one. For such a step,
name a plain command in `gate`. If the named step doesn't exist,
`ci-step` exits 127 and the canary reports itself as rotted.

## Add a canary

Name the file `<owner>-<gate>.patch`. The `g1-`, `g2-`, `g3-` and `g4-`
prefixes are the #779 tracks. Use a short gate name, such as
`g4-clippy`.

```bash
# On a clean checkout of main:
#   make the regression the gate must catch, then
git diff > /tmp/body.patch
git checkout -- .
{ printf '%s\n' \
    '# gate: <command>' \
    '# expect: <regex the failure prints>' \
    '# issue: #NNN'; cat /tmp/body.patch; } > ci/canaries/gN-name.patch

# Prove it goes red. `run` commits on top of HEAD, so use a
# scratch branch:
git switch -c canary-try
uv run scripts/ci/canary.py run ci/canaries/gN-name.patch   # exit 0 = red as expected
git switch - && git branch -D canary-try
```

A PR that adds or changes a canary runs `gate-canary.yml` on that PR,
so the canary is proven red before it lands. `uv run --no-project
--with pytest==8.3.4 --with pyyaml==6.0.2 pytest scripts/ci/tests`
parses every committed canary and checks that each `ci-step` names a
real step.

Keep the patch small, and anchor it on stable context, such as the end
of a file or a struct's first field. Each canary rots when its context
moves.

## When a canary fails

The nightly opens or updates one pinned issue labelled `gate-canary`.
The failing job's log says which of three things happened:

- **The gate passed.** The gate is vacuous. Fix the gate, not the
  canary.
- **The gate failed, but not on `expect`.** The canary proved nothing.
  Find out what broke first, usually the build or a tool. Then fix the
  canary or its setup.
- **The patch no longer applies.** Regenerate it against main with the
  same regression.

The issue closes only after a full green scheduled run, never after a
dispatched subset. The staleness job in the same workflow fails while
the issue stays open for more than 7 days.

## Canaries

| Canary | Gate | Proves |
|---|---|---|
| `g4-fmt` | ci.yml Check, "Check formatting" (`ci-step`) | rustfmt still fails a misformatted line |
| `g4-clippy` | ci.yml Check, "Run clippy" (`ci-step`) | `clippy -D warnings` still fails a lint (`len_zero`), not just prints it (#695) |
| `g4-e2e-baseline` | `ci_guard.py --check` (e2e.yml) | the structural guard still fails when `ci_baseline.json` disagrees with the output |
| `g4-band-widening` | `scripts/ci/baseline_ratchet.py` (ci.yml Gate scripts) | a widened tippecanoe band without a `Baseline-change:` trailer fails (09673f5) |
| `g4-cli-flag-parity` | `tiles_has_parity_with_two_step_flags` (ci.yml Test) | an `export-pmtiles` flag that `tiles` doesn't surface fails (#374, #600) |
| `g1-clippy-disallowed` | ci.yml Check, "Run clippy" (`ci-step`) | a parquet reader built outside its owner fails `disallowed-methods` in `clippy.toml` (#577, #606, #607) |
| `g1-architecture-ratchet` | ci.yml Check, "Check structural ratchets (architecture test)" (`ci-step`) | a new CLI `bail!` goes above its pin in `ci/ratchets.toml` (#458, #709) |
| `g1-blanket-allow` | ci.yml Check, "Check structural ratchets (architecture test)" (`ci-step`) | an `#[allow(clippy::all)]` that would hide a raw parquet reader from `disallowed-methods` fails |
| `g1-cargo-deny` | `cargo deny check bans` (security.yml) | a second version of a crate without a linked skip fails `multiple-versions = "deny"` (#649) |
| `g1-similarity` | ci.yml Check, "Check for duplicated code (similarity ratchet)" (`ci-step`) | a new near-duplicate function pair not in `ci/similarity_baseline.txt` fails (8576c61) |
| `g1-mirrored-hunks` | `scripts/ci/mirrored_hunks.py` (ci.yml Check, PRs only) | one block added in two files in one change fails (#507, 8576c61, #743) |

The other #779 tracks add their `g1-*`, `g2-*` and `g3-*` canaries to
this table.
