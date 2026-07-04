#!/usr/bin/env python3
"""Render parallel_reader_results.json + remote_access_results.json
into the RESULTS.md "latency floor" comparison table (issue #201)."""
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))


def fmt_bytes(b):
    if b >= 1024 * 1024:
        return f"{b / (1024 * 1024):.2f} MB"
    return f"{b / 1024:.0f} KB"


def main():
    with open(os.path.join(HERE, "parallel_reader_results.json")) as f:
        pr = json.load(f)
    with open(os.path.join(HERE, "remote_access_results.json")) as f:
        ra = json.load(f)

    print(
        "| dataset | viewport | z | DuckDB cold | req | "
        "parallel cold | req | footer-cached | req | "
        "PMTiles cold | req |"
    )
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    for ds, vps in pr["datasets"].items():
        for vp, e in vps.items():
            c, w = e["cold"], e["footer_cached"]
            b = ra["datasets"][ds][vp]
            oc = b["overview"]["cold"]
            pm = b["pmtiles"]["cold"]
            print(
                f"| {ds} | {vp} | {e['zoom']} "
                f"| {oc['wall_ms']:,.0f} ms | {oc['head'] + oc['get']} "
                f"| **{c['wall_ms']:,.0f} ms** | {c['requests']} "
                f"| **{w['wall_ms']:,.0f} ms** | {w['requests']} "
                f"| {pm['wall_ms']:,.0f} ms | {pm['requests']} |"
            )

    print()
    print(
        "| dataset | viewport | cold footer | cold fetch | "
        "cold decode | cached bytes |"
    )
    print("|---|---|---|---|---|---|")
    for ds, vps in pr["datasets"].items():
        for vp, e in vps.items():
            c, w = e["cold"], e["footer_cached"]
            print(
                f"| {ds} | {vp} "
                f"| {c['footer_ms']:.0f} ms | {c['fetch_ms']:.0f} ms "
                f"| {c['decode_ms']:.0f} ms | {fmt_bytes(w['bytes'])} |"
            )


if __name__ == "__main__":
    main()
