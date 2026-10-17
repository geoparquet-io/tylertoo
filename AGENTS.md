# Agent instructions

These rules apply to every coding agent that works in this repository.
Build, test, and commit conventions are in [CLAUDE.md](CLAUDE.md).

## Filing issues

Every issue an agent files states how much it matters. An unranked issue
costs the maintainer a triage pass, and nobody can sort a backlog
of unranked issues.

### Before you file

- Search open and closed issues for the problem. If an issue already
  covers it, comment there.
- Confirm that `main` still has the problem. Read the code, and search
  merged PRs with `gh pr list --state merged --search "<keyword>"`. A
  review follow-up is often fixed before anyone files it.

### Group small, related findings

Put small, related findings in one issue. Related means the same module,
the same review, or the same fix. Three nits from one review of
`auto_zoom.rs` make one issue, not three.

Two exceptions:

- A finding rated P0 or P1 gets its own issue, so its label stays accurate
  and the issue closes when its fix merges.
- A nit you can fix in the PR you are working on gets fixed there, with no
  issue.

A bundled issue takes the priority of its most severe item.

### Label every issue and open with its impact

Give every issue one priority label (`P0`–`P3`) and one kind label:

| Kind | Use for |
|------|---------|
| `bug` | The tool does the wrong thing. |
| `optimization` | The tool does the right thing, too slowly or with too much memory. |
| `enhancement` | New behavior, API changes, docs, and roadmap work. |

Start the body with an `Impact:` line that names the priority, who hits
the problem, how often, what breaks, and the workaround:

```
Impact: P1. Python callers on hosts that set TMPDIR (macOS, SLURM,
most CI images). convert() now writes its intermediate file to TMPDIR;
on a tmpfs /tmp the run is OOM-killed. Workaround: pass spill_dir.
No error or doc tells the user so.
```

### The priority scale

| Label | Meaning | Examples |
|-------|---------|----------|
| `P0` | Ordinary data fails or comes out wrong, and no documented workaround exists. | #675 (`SHAPE` column fails export, rule 2), #572 (default thinning fragments admin coverages, rule 2) |
| `P1` | Ordinary data fails or comes out wrong, and a documented workaround exists. | #709 (intermediate file moved to `$TMPDIR`, rule 1), #708 (CRS report wrong for multi-part input, rule 1) |
| `P2` | Performance problems, failures with a narrow trigger, and weak diagnostics. | #685 (quadratic polygon encode), #707 (guard drops features from corrupt input with no log, rule 1) |
| `P3` | Crafted or hostile input, nits, and cleanup. | #683 (crafted footer panics `arrow-ipc`), #436 (`#[non_exhaustive]` pass) |

Three rules adjust the base rating. The examples above name the rule
that moved them.

1. **Silent harm moves an issue up one level.** Wrong output with no error
   or warning is worse than a loud failure, and so is a behavior change
   that no changelog entry or message announces. Apply this rule only to
   harm you confirmed. A suspected silent failure keeps its base rating
   until a test or real data shows it.
2. **A workaround counts only when the user can find it.** The error
   message, the CLI help, or the docs must name it. A flag that would help,
   but that nothing points to, is no workaround.
3. **A run that cannot finish is a failure, not a slowdown.** An
   out-of-memory kill, or hours where minutes are normal, on ordinary
   data rates as P0 or P1.

Change the label and the `Impact:` line when the facts change. A
documented workaround moves a P0 to P1. A fix for the silent part moves
an issue down a level.

### When the remaining work moves

When a PR fixes part of an issue and moves the rest to a new issue, close
the original. Its closing comment names the PR that fixed it and links
the new issue. Write `Fixes #n` in a PR body only when the PR fixes all
of #n.
