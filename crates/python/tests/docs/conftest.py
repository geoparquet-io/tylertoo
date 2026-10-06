"""Fixtures for the documentation tests.

Policy: if it is in the docs, it is tested.

- Tutorial steps live as real scripts under ``examples/``. The tests run
  them and check that each tutorial shows every script verbatim, followed
  by output that the run reproduces.
- The README quickstart runs as written, against the cached fixture.
- Every ``tylertoo`` command in a hand-written page is checked against the
  CLI's ``--help``, so a renamed or removed flag fails here.
- A Python block that no test runs must carry a ``# notest`` comment.

Run with ``uv run pytest -m docs tests/docs``. The default ``pytest`` run
deselects these tests: they need the release CLI, the fixture download, and
the ``docs`` dependency group.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest
from doc_blocks import MADAGASCAR, REPO_ROOT


def pytest_collection_modifyitems(items: list[pytest.Item]) -> None:
    """Mark every test in this directory with ``docs``."""
    here = Path(__file__).parent
    for item in items:
        if here in item.path.parents:
            item.add_marker(pytest.mark.docs)


@pytest.fixture(scope="session")
def tylertoo_bin() -> Path:
    """The CLI binary: ``$TYLERTOO_BIN``, else the release build."""
    path = Path(
        os.environ.get("TYLERTOO_BIN", REPO_ROOT / "target" / "release" / "tylertoo")
    )
    if not path.is_file():
        pytest.fail(
            f"{path} not found. Run `cargo build --release -p tylertoo` "
            "or set TYLERTOO_BIN."
        )
    return path


@pytest.fixture(scope="session")
def doc_env(tylertoo_bin: Path) -> dict[str, str]:
    """Environment with the CLI first on PATH, as a reader would have it."""
    env = dict(os.environ)
    env["PATH"] = os.pathsep.join([str(tylertoo_bin.parent), env["PATH"]])
    env["NO_COLOR"] = "1"
    return env


@pytest.fixture(scope="session")
def madagascar() -> Path:
    """The tutorials' input, from the ``fixtures-v1`` release."""
    path = REPO_ROOT / "tests" / "fixtures" / "realdata" / MADAGASCAR
    if not path.is_file():
        pytest.fail(
            f"{path} not found. Run `gh release download fixtures-v1 "
            "--dir tests/fixtures/realdata/`."
        )
    return path
