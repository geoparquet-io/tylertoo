"""`CI passed` gates every job in ci.yml (#810)."""

from pathlib import Path

import yaml

CI = Path(__file__).resolve().parents[3] / ".github" / "workflows" / "ci.yml"


def test_ci_passed_needs_every_job():
    jobs = yaml.safe_load(CI.read_text())["jobs"]
    needs = set(jobs["ci-passed"]["needs"])
    assert needs == set(jobs) - {"ci-passed"}
