#!/usr/bin/env python3
"""Format duckdb_knobs_results.json (issue #203) as markdown tables.

Emits the two compact tables used in the RESULTS.md §2b addendum:
  1. cold per-knob wall/requests/bytes vs defaults
  2. session behavior (cold / repeat / adjacent pan) for the PR #200
     symmetric config vs the real-user config

Run:
  python3 format_duckdb_knobs.py
"""
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))

CONFIG_LABELS = [
    ("defaults", "defaults"),
    ("threads_64", "`threads=64`"),
    ("no_parquet_prefetch", "`disable_parquet_prefetching`"),
    ("prefetch_all", "`prefetch_all_parquet_files`"),
    ("keepalive_off", "`http_keep_alive=false`"),
    ("metadata_caches_on", "metadata caches on"),
    ("stack", "recommended stack"),
]


def fmt_bytes(b):
    if b >= 1024 ** 2:
        return f"{b / 1024 ** 2:.2f} MB"
    if b >= 1024:
        return f"{b / 1024:.0f} KB"
    return f"{b:.0f} B"


def main():
    with open(os.path.join(HERE, "duckdb_knobs_results.json")) as f:
        r = json.load(f)

    cells = [
        (ds, vp, r["datasets"][ds][vp])
        for ds in r["datasets"]
        for vp in ("world", "regional", "street")
    ]

    print("### Cold, per knob (median wall ms / requests / bytes)\n")
    hdr = "| dataset | viewport |" + "".join(
        f" {label} |" for _, label in CONFIG_LABELS)
    print(hdr)
    print("|---|---|" + "---|" * len(CONFIG_LABELS))
    for ds, vp, e in cells:
        row = f"| {ds} | {vp} |"
        for key, _ in CONFIG_LABELS:
            m = e["cold"][key]
            row += (f" {m['wall_ms']:,.0f} / "
                    f"{m['head'] + m['get']} / "
                    f"{fmt_bytes(m['bytes'])} |")
        print(row)

    print("\n### Session behavior (same process)\n")
    print("| dataset | viewport | config | cold | repeat | adjacent pan |")
    print("|---|---|---|---|---|---|")
    for ds, vp, e in cells:
        for cfg in ("sym200", "real"):
            s = e["session"][cfg]
            row = f"| {ds} | {vp} | {cfg} |"
            for q in ("cold", "repeat", "adjacent"):
                m = s[q]
                row += (f" {m['wall_ms']:,.0f} ms / "
                        f"{m['head'] + m['get']} req / "
                        f"{fmt_bytes(m['bytes'])} |")
            print(row)


if __name__ == "__main__":
    main()
