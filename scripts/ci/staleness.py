# /// script
# requires-python = ">=3.11"
# dependencies = ["pyyaml==6.0.2"]
# ///
"""Gate staleness (#779): a red or silent scheduled gate must not go unnoticed.

The weekly mutation sweep failed for 15 weeks (#419, #635, #677) and the
bench gate ran once in 166 runs (#678) without anyone noticing: a
scheduled gate's failure is an issue nobody owns and its silence is
nothing at all. This check, run nightly by gate-canary.yml, fails when:

  1. a scheduled workflow's last SUCCESSFUL scheduled run is older than
     --max-run-age days (default 8: one missed weekly run), or it has
     never succeeded on a schedule. A disabled schedule (GitHub turns
     them off after 60 days without repo activity) shows up here too;
  2. an issue carrying one of the auto-filed gate labels has been open
     longer than --max-issue-age days (default 7);
  3. a fuzz target held out of the nightly (FUZZ_HOLDOUTS in fuzz.yml)
     links a closed issue: that reason for holding it out is gone, so the
     entry drops it, and a target with no open reason left goes back in
     the matrix; or the hold-out list and the matrix
     disagree with fuzz/fuzz_targets/ (a target in neither list is fuzzed
     nowhere).

Usage (needs `gh` authenticated for the repo; GH_REPO selects it):
  uv run scripts/ci/staleness.py
  uv run scripts/ci/staleness.py --now 2026-10-27T13:00:00Z   # reproducible
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# Workflows whose `schedule` trigger is a gate. bench-regression and
# tippecanoe-compare also run on PRs; only their scheduled (weekly) runs
# count here.
SCHEDULED = ["fuzz.yml", "mutation-tests.yml", "security.yml", "bench-regression.yml", "tippecanoe-compare.yml"]
# Labels the scheduled workflows (and gate-canary.yml itself) file issues under.
LABELS = ["fuzz", "mutation-tests", "security-audit", "tippecanoe-parity", "gate-canary"]


@dataclass(frozen=True)
class Problem:
    kind: str
    detail: str

    def __str__(self) -> str:
        return f"[{self.kind}] {self.detail}"


def parse_time(stamp: str) -> datetime:
    return datetime.fromisoformat(stamp.replace("Z", "+00:00"))


# --- pure checks ------------------------------------------------------------


def stale_runs(last_success: dict[str, dict | None], now: datetime, max_age: timedelta) -> list[Problem]:
    """last_success: {workflow: {"createdAt", "url"} | None}."""
    out = []
    for wf, run in last_success.items():
        if run is None:
            out.append(Problem("run", f"{wf}: no successful scheduled run on record"))
            continue
        age = now - parse_time(run["createdAt"])
        if age > max_age:
            out.append(
                Problem(
                    "run",
                    f"{wf}: last successful scheduled run is {age.days}d {age.seconds // 3600}h old "
                    f"(limit {max_age.days}d): {run.get('url', '')}",
                )
            )
    return out


def stale_issues(issues: list[dict], now: datetime, max_age: timedelta) -> list[Problem]:
    """issues: [{"number", "title", "createdAt", "url", "label"}]; one entry per issue."""
    out, seen = [], set()
    for issue in sorted(issues, key=lambda i: i["number"]):
        if issue["number"] in seen:
            continue
        seen.add(issue["number"])
        age = now - parse_time(issue["createdAt"])
        if age > max_age:
            out.append(
                Problem(
                    "issue",
                    f"#{issue['number']} ({issue['label']}) open {age.days}d (limit {max_age.days}d): "
                    f"{issue['title']}",
                )
            )
    return out


def load_holdouts(fuzz_yml: Path) -> tuple[list[dict], list[str]]:
    """(FUZZ_HOLDOUTS entries, the fuzz job's matrix targets) from fuzz.yml."""
    import yaml

    doc = yaml.safe_load(fuzz_yml.read_text())
    holdouts = json.loads(doc["env"]["FUZZ_HOLDOUTS"])
    matrix = doc["jobs"]["fuzz"]["strategy"]["matrix"]
    targets = list(matrix.get("target", [])) + [e["target"] for e in matrix.get("include", [])]
    return holdouts, targets


def holdout_problems(
    holdouts: list[dict], matrix_targets: list[str], all_targets: set[str], issue_state: dict[int, str]
) -> list[Problem]:
    out = []
    held = [h["target"] for h in holdouts]
    for h in holdouts:
        issues = h.get("issues", [])
        if not issues:
            out.append(Problem("fuzz", f"hold-out {h['target']} links no issue"))
            continue
        if h["target"] not in all_targets:
            out.append(Problem("fuzz", f"hold-out {h['target']} is not a target in fuzz/fuzz_targets/"))
        closed = [n for n in issues if issue_state.get(n, "UNKNOWN") == "CLOSED"]
        if closed:
            refs = ", ".join(f"#{n}" for n in closed)
            action = (
                "re-enable it in fuzz.yml's matrix"
                if len(closed) == len(issues)
                else "drop the closed issue from its entry"
            )
            out.append(Problem("fuzz", f"hold-out {h['target']}: {refs} closed; {action}"))
    for t in sorted(set(held) & set(matrix_targets)):
        out.append(Problem("fuzz", f"{t} is both in the nightly matrix and held out"))
    for t in sorted(all_targets - set(held) - set(matrix_targets)):
        out.append(Problem("fuzz", f"{t} is in fuzz/fuzz_targets/ but neither fuzzed nightly nor held out"))
    return out


# --- gh ---------------------------------------------------------------------


def gh_json(*args: str):
    proc = subprocess.run(["gh", *args], check=True, capture_output=True, text=True)
    return json.loads(proc.stdout or "null")


def last_scheduled_success(workflow: str) -> dict | None:
    runs = gh_json(
        "run",
        "list",
        "--workflow",
        workflow,
        "--event",
        "schedule",
        "--status",
        "success",
        "--limit",
        "1",
        "--json",
        "createdAt,url",
    )
    return runs[0] if runs else None


def open_issues(label: str) -> list[dict]:
    issues = gh_json(
        "issue", "list", "--label", label, "--state", "open", "--limit", "200", "--json", "number,title,createdAt,url"
    )
    return [dict(i, label=label) for i in issues]


def issue_states(numbers: set[int]) -> dict[int, str]:
    return {n: gh_json("issue", "view", str(n), "--json", "state")["state"] for n in sorted(numbers)}


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--now", help="ISO time to measure ages against (default: now)")
    ap.add_argument("--max-run-age", type=float, default=8.0, help="days (default 8)")
    ap.add_argument("--max-issue-age", type=float, default=7.0, help="days (default 7)")
    args = ap.parse_args(argv)
    now = parse_time(args.now) if args.now else datetime.now(timezone.utc)

    problems: list[Problem] = []
    problems += stale_runs({wf: last_scheduled_success(wf) for wf in SCHEDULED}, now, timedelta(days=args.max_run_age))
    issues = [i for label in LABELS for i in open_issues(label)]
    problems += stale_issues(issues, now, timedelta(days=args.max_issue_age))

    holdouts, matrix_targets = load_holdouts(ROOT / ".github/workflows/fuzz.yml")
    all_targets = {p.stem for p in (ROOT / "fuzz/fuzz_targets").glob("*.rs")}
    states = issue_states({n for h in holdouts for n in h.get("issues", [])})
    problems += holdout_problems(holdouts, matrix_targets, all_targets, states)

    lines = [f"Gate staleness as of {now:%Y-%m-%d %H:%M} UTC"]
    lines += [f"- {p}" for p in problems] or ["- nothing stale"]
    report = "\n".join(lines)
    print(report)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write("## Gate staleness\n\n" + "\n".join(lines[1:]) + "\n")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
