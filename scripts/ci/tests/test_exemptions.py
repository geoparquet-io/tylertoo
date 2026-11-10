"""ci/exemptions.toml is the one exemptions file for every check (#810).

Each entry is `[<check>.<name>]` with a `reason`. The count is pinned here so
that an exemption is a reviewed two-file change, never a quiet one-liner.
"""

import tomllib
from pathlib import Path

EXEMPTIONS = Path(__file__).resolve().parents[3] / "ci" / "exemptions.toml"

# Update with the entry you add or remove, and say why in the PR.
EXPECTED_COUNT = 18


def _entries():
    data = tomllib.loads(EXEMPTIONS.read_text())
    return [
        (check, name, entry)
        for check, table in data.items()
        for name, entry in table.items()
    ]


def test_count_is_pinned():
    assert len(_entries()) == EXPECTED_COUNT


def test_every_entry_has_a_reason():
    for check, name, entry in _entries():
        assert isinstance(entry, dict) and entry.get("reason", "").strip(), (
            f"[{check}.{name}] needs a reason"
        )
