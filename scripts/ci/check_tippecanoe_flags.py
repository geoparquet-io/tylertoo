#!/usr/bin/env python3
"""Check generated flag mappings against a tippecanoe installation."""

import argparse
import os
import re
import subprocess
from pathlib import Path


def check(binary: Path, tables: Path) -> None:
    """Check the options and companion programs.

    Raises:
        ValueError: An option, program, or table section is missing.
    """
    document = tables.read_text()
    start = "<!-- --8<-- [start:flags] -->"
    end = "<!-- --8<-- [end:flags] -->"
    if start not in document or end not in document:
        raise ValueError("Missing flags snippet section")
    section = document.split(start, 1)[1].split(end, 1)[0]
    flags = set()
    for line in section.splitlines():
        if line.startswith("|"):
            first_column = line.split("|", 2)[1]
            flags.update(re.findall(r"`(-{1,2}[\w-]+)(?:[^`]*)`", first_column))
    if not flags:
        raise ValueError("The flags table has no options")

    result = subprocess.run(
        [str(binary), "--help"], capture_output=True, text=True, check=True
    )
    help_flags = set(
        re.findall(r"(?<![\w-])-{1,2}[\w-]+", result.stdout + result.stderr)
    )
    missing = sorted(flags - help_flags)
    if missing:
        raise ValueError(f"Options absent from tippecanoe help: {', '.join(missing)}")
    for program in ("tile-join", "tippecanoe-decode"):
        sibling = binary.parent / program
        if not sibling.is_file() or not os.access(sibling, os.X_OK):
            raise ValueError(f"Missing executable next to tippecanoe: {sibling}")


def main() -> None:
    """Check the binary and tables supplied on the command line."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bin", type=Path)
    parser.add_argument("tables", type=Path)
    args = parser.parse_args()
    try:
        check(args.bin.absolute(), args.tables)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
