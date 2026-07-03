#!/usr/bin/env python3
"""Render the access_results.json into the RESULTS.md markdown table."""
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(HERE, "access_results.json")) as f:
    R = json.load(f)
with open(os.path.join(HERE, "viewports.json")) as f:
    V = json.load(f)


def kb(b):
    return f"{b/1000:,.0f} KB" if b < 1e6 else f"{b/1e6:,.2f} MB"


print("### Access: bytes / requests / wall time per viewport\n")
print("| dataset | viewport | z | overview bytes | ov req | ov ms | ov feats "
      "| pmtiles bytes | pm req | pm ms | pm tiles | overview/pmtiles bytes |")
print("|" + "---|" * 12)
for ds, vps in R.items():
    for vp in ("world", "regional", "street"):
        d = vps[vp]
        ov, pm = d["overview"], d["pmtiles"]
        ratio = ov["bytes"] / pm["bytes"]
        print(
            f"| {ds} | {vp} | {d['zoom']} | {kb(ov['bytes'])} | "
            f"{ov['requests']} | {ov['wall_ms']:.0f} | {ov['features']:,} | "
            f"{kb(pm['bytes'])} | {pm['requests']} | {pm['wall_ms']:.0f} | "
            f"{pm['tiles_present']} | {ratio:.1f}x |"
        )

print("\n### Viewport rectangles (identical for both paths)\n")
print("| dataset | viewport | zoom | bbox [xmin,ymin,xmax,ymax] |")
print("|---|---|---|---|")
for ds, cfg in V.items():
    for vp in ("world", "regional", "street"):
        c = cfg["viewports"][vp]
        bb = ", ".join(f"{x:.4f}" for x in c["bbox"])
        print(f"| {ds} | {vp} | {c['zoom']} | [{bb}] |")
