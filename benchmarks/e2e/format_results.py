#!/usr/bin/env python3
"""Shim: the renderers live in scripts/gen_bench_docs.py (#810).

    python3 format_results.py [results.json] [--section NAME] [--dataset ID]

Prints the tables a results file would put in the docs; the docs themselves
are rewritten by `uv run python scripts/gen_bench_docs.py`.
"""

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
import gen_bench_docs as g  # noqa: E402


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("results", type=Path, nargs="?",
                    default=Path(__file__).parent / "results.json")
    ap.add_argument("--section", choices=[*g.SECTIONS, "per-zoom"], default=None)
    ap.add_argument("--dataset", default=None, help="for --section per-zoom")
    args = ap.parse_args()
    r = json.loads(args.results.read_text())
    r["_source_file"] = args.results.name
    ds = args.dataset or next(iter(r["datasets"]))
    if args.section == "per-zoom":
        print(g.per_zoom(r, ds))
    elif args.section:
        print(g.RENDERERS[args.section][0](r, None))
    else:
        for name in g.SECTIONS:
            print(f"\n### {name}\n")
            print(g.RENDERERS[name][0](r, None))
        for did in r["datasets"]:
            print(f"\n### per-zoom: {did}\n")
            print(g.per_zoom(r, did))
    return 0


if __name__ == "__main__":
    sys.exit(main())
