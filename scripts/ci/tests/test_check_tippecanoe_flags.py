"""Check documented options against a fake tippecanoe installation."""

import subprocess
import sys
from pathlib import Path

import pytest

CHECKER = Path(__file__).resolve().parents[1] / "check_tippecanoe_flags.py"


def check(tmp_path, help_text, rows, *, stderr=False, missing=None):
    """Create a fake installation and check its documented mappings.

    Returns:
        The checker process result, including its output and exit code.
    """
    binary = tmp_path / "tippecanoe"
    binary.write_text(
        "#!/bin/sh\n"
        '[ "$1" = --help ] || exit 2\n'
        f"cat <<'HELP'{' >&2' if stderr else ''}\n{help_text}\nHELP\n"
    )
    binary.chmod(0o755)
    for program in ("tile-join", "tippecanoe-decode"):
        if program != missing:
            sibling = tmp_path / program
            sibling.write_text("#!/bin/sh\nexit 0\n")
            sibling.chmod(0o755)
    tables = tmp_path / "tables.md"
    tables.write_text(
        "<!-- --8<-- [start:flags] -->\n"
        "| Tippecanoe | tylertoo | Notes |\n| --- | --- | --- |\n"
        f"{rows}\n<!-- --8<-- [end:flags] -->\n"
        "<!-- --8<-- [start:preset] -->\n"
        "| `--not-a-tippecanoe-flag` |\n<!-- --8<-- [end:preset] -->\n"
    )
    return subprocess.run(
        [sys.executable, str(CHECKER), str(binary), str(tables)],
        capture_output=True,
        text=True,
        check=False,
    )


@pytest.mark.parametrize("stderr", [False, True])
def test_tippecanoe_flags_match_help_tokens(tmp_path, stderr):
    """Read either output stream and inspect only foreign flags."""
    result = check(
        tmp_path,
        "[--maximum-zoom=...] [--drop-lines]",
        "| `--maximum-zoom=g` | `tiles --max-zoom auto` | |\n"
        "| `--drop-lines` | `tiles --drop-lines` | |\n"
        "| `tile-join` | `merge`, `pyramid` | |\n"
        "| `tippecanoe-decode` | `decode` | |",
        stderr=stderr,
    )
    assert result.returncode == 0, result.stderr


@pytest.mark.parametrize(
    "help_text", ["--buffer-extra", "--no-buffer", "--buffer_size"]
)
def test_tippecanoe_flags_reject_partial_tokens(tmp_path, help_text):
    """Reject partial matches in longer option names."""
    result = check(tmp_path, help_text, "| `--buffer` | `tiles --tile-buffer` | |")
    assert result.returncode != 0
    assert "--buffer" in result.stderr


@pytest.mark.parametrize("missing", ["tile-join", "tippecanoe-decode"])
def test_tippecanoe_flags_require_sibling_programs(tmp_path, missing):
    """Require both companion programs in the binary's directory."""
    result = check(tmp_path, "--buffer", "| `--buffer` | | |", missing=missing)
    assert result.returncode != 0
    assert missing in result.stderr


def test_tippecanoe_flags_reject_empty_table(tmp_path):
    """Reject a table with no options to check."""
    result = check(tmp_path, "--buffer", "")
    assert result.returncode != 0
    assert "flags table has no options" in result.stderr
