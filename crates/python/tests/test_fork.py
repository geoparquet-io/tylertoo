"""A process forked after `import tylertoo` must still be able to convert.

Importing must not start the worker pool: a forked child inherits a started
pool but none of its threads, and its first parallel job then waits forever.
The parent runs in a fresh interpreter, because a pool this test process
already used would hang a forked child whatever import does.
"""

import os
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

BUILDINGS = (
    Path(__file__).parent.parent.parent.parent
    / "tests"
    / "fixtures"
    / "realdata"
    / "open-buildings.parquet"
)

PARENT = textwrap.dedent(
    """
    import multiprocessing, sys
    import tylertoo

    def child(src, dst):
        tylertoo.overview(src, dst, min_zoom=0, max_zoom=8)

    p = multiprocessing.get_context("fork").Process(target=child, args=sys.argv[1:3])
    p.start()
    p.join(120)
    if p.is_alive():
        p.kill()
        sys.exit("hung")
    sys.exit(p.exitcode)
    """
)


@pytest.mark.skipif(
    not hasattr(os, "fork") or sys.platform == "darwin",
    reason="fork start method is Linux-only here",
)
@pytest.mark.skipif(
    not BUILDINGS.exists(), reason="open-buildings.parquet fixture not available"
)
def test_a_child_forked_after_import_can_convert(tmp_path):
    result = subprocess.run(
        [sys.executable, "-c", PARENT, str(BUILDINGS), str(tmp_path / "out.parquet")],
        check=False,
        capture_output=True,
        text=True,
        timeout=180,
    )
    assert result.returncode == 0, (
        "a child forked after import failed or hung on its first conversion: "
        + result.stderr[-2000:]
    )
