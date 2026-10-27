# /// script
# requires-python = ">=3.11"
# dependencies = ["pyyaml==6.0.2"]
# ///
"""Gate canaries (#779): prove each CI gate can still go red.

A canary is ci/canaries/<name>.patch: a patch that injects the regression a
gate exists to catch, headed by comment lines that name the gate. The
nightly .github/workflows/gate-canary.yml applies each one to main, runs
its gate, and fails when the gate PASSES: a gate that cannot see its own
regression is vacuous (#419/#677: the mutation sweep red for 15 weeks;
#695: clippy's exit status swallowed by a pipe). The format is documented
in ci/canaries/README.md.

Subcommands:
  list                 print the canaries as a JSON matrix for the workflow
  run PATCH            apply, commit, run the gate; exit 0 iff it went red
                       for the stated reason
  ci-step WF JOB STEP  run the `run:` script of one step of a workflow in
                       .github/workflows, the way the runner would (bash -e
                       -o pipefail), so a canary exercises the gate exactly
                       as CI configures it, not a copy of its command

Run from the repository root with a clean tree: `run` commits on top of
HEAD (git identity required) and leaves the commit in place.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CANARY_DIR = ROOT / "ci" / "canaries"
WORKFLOW_DIR = ROOT / ".github" / "workflows"

# Optional setup a canary can ask the workflow for (`# setup:`).
KNOWN_SETUP = {"tippecanoe", "go-pmtiles"}
HEADER_RE = re.compile(r"^#\s*([a-z-]+):\s*(.*?)\s*$")


@dataclass
class Canary:
    name: str
    path: Path
    gate: str
    expect: str | None = None
    issue: str | None = None
    commit: str | None = None
    setup: list[str] = field(default_factory=list)

    def matrix_entry(self) -> dict:
        patch = self.path.relative_to(ROOT) if self.path.is_relative_to(ROOT) else self.path
        entry = {"name": self.name, "patch": str(patch)}
        for s in sorted(KNOWN_SETUP):
            entry[s.replace("-", "_")] = s in self.setup
        return entry


class CanaryError(ValueError):
    pass


def parse_header(path: Path) -> Canary:
    """Read the `# key: value` lines that precede the patch body."""
    fields: dict[str, str] = {}
    for line in path.read_text().splitlines():
        if not line.startswith("#"):
            break
        m = HEADER_RE.match(line)
        if not m:
            continue
        key, value = m.groups()
        if key in fields and key != "setup":
            raise CanaryError(f"{path.name}: duplicate `# {key}:` header")
        fields[key] = (fields.get(key, "") + " " + value).strip() if key == "setup" else value
    if not fields.get("gate"):
        raise CanaryError(f"{path.name}: the first lines must include `# gate: <command>`")
    setup = fields.get("setup", "").split()
    unknown = set(setup) - KNOWN_SETUP
    if unknown:
        raise CanaryError(f"{path.name}: unknown `# setup:` value(s) {sorted(unknown)}; known: {sorted(KNOWN_SETUP)}")
    if fields.get("expect"):
        try:
            re.compile(fields["expect"])
        except re.error as e:
            raise CanaryError(f"{path.name}: `# expect:` is not a valid regex: {e}") from e
    return Canary(
        name=path.stem,
        path=path,
        gate=fields["gate"],
        expect=fields.get("expect") or None,
        issue=fields.get("issue") or None,
        commit=fields.get("commit") or None,
        setup=setup,
    )


def discover(directory: Path = CANARY_DIR) -> list[Canary]:
    return [parse_header(p) for p in sorted(directory.glob("*.patch"))]


def verdict(gate_exit: int, output: str, expect: str | None) -> tuple[bool, str]:
    """(canary passed, explanation). The canary passes when the gate fails
    for the stated reason; a gate that passes is the finding."""
    if gate_exit == 0:
        return False, "the gate PASSED with the regression applied: it is vacuous"
    if gate_exit in (126, 127):
        return False, f"the gate command could not run (exit {gate_exit}): the canary has rotted"
    if expect and not re.search(expect, output, re.MULTILINE):
        return False, (
            f"the gate failed (exit {gate_exit}) but its output does not match `# expect: {expect}`: "
            "it failed for another reason (a build error, a missing tool), so it did not prove anything"
        )
    return True, f"the gate went red as expected (exit {gate_exit})"


# --- ci-step ----------------------------------------------------------------


def find_step(workflow: str, job: str, step: str) -> dict:
    import yaml  # PEP 723 dependency; only this subcommand needs it

    path = WORKFLOW_DIR / workflow
    doc = yaml.safe_load(path.read_text())
    jobs = doc.get("jobs", {})
    if job not in jobs:
        raise CanaryError(f"{workflow}: no job {job!r} (jobs: {sorted(jobs)})")
    steps = [s for s in jobs[job].get("steps", []) if s.get("name") == step]
    if len(steps) != 1:
        names = [s.get("name") for s in jobs[job].get("steps", []) if s.get("name")]
        raise CanaryError(f"{workflow} job {job!r}: {len(steps)} steps named {step!r} (steps: {names})")
    found = steps[0]
    if "run" not in found:
        raise CanaryError(f"{workflow} {job}/{step!r} is a `uses:` step, not a `run:` script")
    for key in ("run", "env", "working-directory"):
        if "${{" in json.dumps(found.get(key, "")):
            raise CanaryError(
                f"{workflow} {job}/{step!r}: `{key}` holds a ${{{{ }}}} expression the canary cannot "
                "evaluate; name a plain command in `# gate:` instead"
            )
    return found


def run_ci_step(workflow: str, job: str, step: str) -> int:
    found = find_step(workflow, job, step)
    env = dict(os.environ)
    env.update({k: str(v) for k, v in (found.get("env") or {}).items()})
    cwd = ROOT / found.get("working-directory", ".")
    print(f"canary: running {workflow} {job}/{step!r}:\n{found['run']}", flush=True)
    # GitHub's default `run` shell on Linux.
    return subprocess.run(
        ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", found["run"]], cwd=cwd, env=env
    ).returncode


# --- run --------------------------------------------------------------------


def apply_and_commit(canary: Canary) -> None:
    check = subprocess.run(["git", "apply", "--check", str(canary.path)], cwd=ROOT, capture_output=True, text=True)
    if check.returncode != 0:
        raise CanaryError(
            f"{canary.path.name} no longer applies to this tree: the canary has rotted; "
            f"regenerate it (ci/canaries/README.md).\n{check.stderr}"
        )
    subprocess.run(["git", "apply", "--index", str(canary.path)], cwd=ROOT, check=True)
    message = canary.commit or f"canary: {canary.name}"
    subprocess.run(["git", "commit", "-q", "--no-verify", "-m", message], cwd=ROOT, check=True)


def run_canary(path: Path) -> int:
    canary = parse_header(path)
    print(f"canary {canary.name}: gate `{canary.gate}`" + (f" (proves {canary.issue})" if canary.issue else ""))
    apply_and_commit(canary)
    proc = subprocess.run(
        ["bash", "--noprofile", "--norc", "-o", "pipefail", "-c", canary.gate],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    sys.stdout.write(proc.stdout)
    ok, why = verdict(proc.returncode, proc.stdout, canary.expect)
    print(f"\ncanary {canary.name}: {'OK' if ok else 'FAIL'}: {why}")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(f"## Canary `{canary.name}`: {'red as expected' if ok else 'FAILED'}\n\n")
            f.write(f"Gate: `{canary.gate}`\n\n{why}\n")
    return 0 if ok else 1


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("list")
    r = sub.add_parser("run")
    r.add_argument("patch", type=Path)
    s = sub.add_parser("ci-step")
    s.add_argument("workflow")
    s.add_argument("job")
    s.add_argument("step")
    args = ap.parse_args(argv)
    try:
        if args.cmd == "list":
            canaries = discover()
            if not canaries:
                print(f"no canaries in {CANARY_DIR}", file=sys.stderr)
                return 1
            print(json.dumps({"include": [c.matrix_entry() for c in canaries]}))
            return 0
        if args.cmd == "run":
            return run_canary(args.patch.resolve())
        return run_ci_step(args.workflow, args.job, args.step)
    except CanaryError as e:
        print(f"canary error: {e}", file=sys.stderr)
        # A ci-step that names no runnable step is a gate that cannot run:
        # 127, so `verdict` calls the canary rotted instead of "red".
        return 127 if args.cmd == "ci-step" else 2


if __name__ == "__main__":
    sys.exit(main())
