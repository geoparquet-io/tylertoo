#!/usr/bin/env python3
"""Render remote_access_results.json into the RESULTS.md markdown tables
(stdout) and the headline chart (remote_access_chart.svg).

Run:  python3 format_remote.py
"""
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))

DS_LABEL = {
    "points-nyc-medium": "NYC points",
    "lines-portland-medium": "Portland lines",
    "polygons-portland-medium": "Portland polygons",
    "polygons-ftw-moldova-large": "Moldova polygons (large)",
}


def fmt_bytes(n):
    if n >= 1024 ** 2:
        return f"{n / 1024 ** 2:.2f} MB"
    return f"{n / 1024:.0f} KB"


def main():
    with open(os.path.join(HERE, "remote_access_results.json")) as f:
        R = json.load(f)
    ds_all = R["datasets"]

    # ---- cold table --------------------------------------------------
    print(
        "| dataset | viewport | z | ov bytes | ov req | ov ms | "
        "ov feats | % of file | pm bytes | pm req | pm ms | % of file |"
    )
    print("|---|---|---|---|---|---|---|---|---|---|---|---|")
    rows = []
    for ds, e in ds_all.items():
        if not isinstance(e, dict) or "world" not in e:
            continue
        ovsz, pmsz = e["overview_file_bytes"], e["pmtiles_file_bytes"]
        for vp in ("world", "regional", "street"):
            v = e[vp]
            oc = v["overview"]["cold"]
            pc = v["pmtiles"]["cold"]
            ov_pct = 100 * oc["bytes"] / ovsz
            pm_pct = 100 * pc["bytes"] / pmsz
            rows.append((ds, vp, v["zoom"], oc, pc, ov_pct, pm_pct))
            print(
                f"| {ds} | {vp} | {v['zoom']} "
                f"| {fmt_bytes(oc['bytes'])} | {oc['head'] + oc['get']} "
                f"| {oc['wall_ms']:.0f} | {oc['features']:,} "
                f"| {ov_pct:.2f}% "
                f"| {fmt_bytes(pc['bytes'])} | {pc['requests']} "
                f"| {pc['wall_ms']:.0f} | {pm_pct:.2f}% |"
            )

    # ---- warm table ---------------------------------------------------
    print()
    print(
        "| dataset | viewport | ov warm ms | ov warm req | "
        "pm warm ms | pm warm req |"
    )
    print("|---|---|---|---|---|---|")
    for ds, e in ds_all.items():
        if not isinstance(e, dict) or "world" not in e:
            continue
        for vp in ("world", "regional", "street"):
            v = e[vp]
            ow = v["overview"]["warm"]
            pw = v["pmtiles"]["warm"]
            print(
                f"| {ds} | {vp} | {ow['wall_ms']:.0f} "
                f"| {ow['head'] + ow['get']} "
                f"| {pw['wall_ms']:.0f} | {pw['requests']} |"
            )

    write_chart(rows)
    print(f"\nwrote remote_access_chart.svg", flush=True)


# ---- headline chart (static SVG) -------------------------------------
SURFACE = "#fcfcfb"
INK = "#0b0b0b"
INK2 = "#52514e"
MUTED = "#898781"
GRID = "#e1e0d9"
BASE = "#c3c2b7"
C_OV = "#2a78d6"   # categorical slot 1 (blue)  — overview GeoParquet
C_PM = "#eda100"   # categorical slot 3 (yellow) — PMTiles
FONT = ("font-family='system-ui,-apple-system,Segoe UI,Helvetica,"
        "Arial,sans-serif'")


def write_chart(rows):
    left, right, top = 190, 70, 96
    plot_w = 520
    bar_h, pair_gap, row_gap, ds_head = 9, 2, 10, 26
    xmax = 7.0  # percent

    def x(p):
        return left + plot_w * (p / xmax)

    body = []
    y = top
    last_ds = None
    for ds, vp, zoom, oc, pc, ov_pct, pm_pct in rows:
        if ds != last_ds:
            y += 8
            body.append(
                f"<text x='{left - 180}' y='{y + 12}' {FONT} "
                f"font-size='12' font-weight='600' fill='{INK}'>"
                f"{DS_LABEL[ds]}</text>"
            )
            y += ds_head
            last_ds = ds
        # viewport label
        yc = y + bar_h + pair_gap / 2
        body.append(
            f"<text x='{left - 10}' y='{yc + 4}' {FONT} font-size='11' "
            f"fill='{INK2}' text-anchor='end'>{vp} z{zoom}</text>"
        )
        for i, (pct, byt, col) in enumerate(
            ((ov_pct, oc["bytes"], C_OV), (pm_pct, pc["bytes"], C_PM))
        ):
            by = y + i * (bar_h + pair_gap)
            w = max(x(pct) - left, 1.5)
            body.append(
                f"<rect x='{left}' y='{by}' width='{w:.1f}' "
                f"height='{bar_h}' rx='2' fill='{col}'/>"
            )
            body.append(
                f"<text x='{left + w + 6:.1f}' y='{by + bar_h - 1}' "
                f"{FONT} font-size='10' fill='{INK2}'>"
                f"{pct:.2f}% · {fmt_bytes(byt)}</text>"
            )
        y += 2 * bar_h + pair_gap + row_gap

    height = y + 40
    grid = []
    for gx in range(0, int(xmax) + 1):
        gtx = x(gx)
        grid.append(
            f"<line x1='{gtx:.1f}' y1='{top}' x2='{gtx:.1f}' "
            f"y2='{y}' stroke='{GRID}' stroke-width='1'/>"
        )
        grid.append(
            f"<text x='{gtx:.1f}' y='{y + 16}' {FONT} font-size='10' "
            f"fill='{MUTED}' text-anchor='middle'>{gx}%</text>"
        )
    grid.append(
        f"<line x1='{left}' y1='{top}' x2='{left}' y2='{y}' "
        f"stroke='{BASE}' stroke-width='1'/>"
    )

    legend = (
        f"<rect x='{left}' y='58' width='10' height='10' rx='2' "
        f"fill='{C_OV}'/>"
        f"<text x='{left + 16}' y='67' {FONT} font-size='11' "
        f"fill='{INK2}'>overview GeoParquet (DuckDB httpfs)</text>"
        f"<rect x='{left + 250}' y='58' width='10' height='10' rx='2' "
        f"fill='{C_PM}'/>"
        f"<text x='{left + 266}' y='67' {FONT} font-size='11' "
        f"fill='{INK2}'>PMTiles (range requests)</text>"
    )

    svg = (
        f"<svg xmlns='http://www.w3.org/2000/svg' "
        f"width='{left + plot_w + right}' height='{height}' "
        f"viewBox='0 0 {left + plot_w + right} {height}'>"
        f"<rect width='100%' height='100%' fill='{SURFACE}'/>"
        f"<text x='{left - 180}' y='26' {FONT} font-size='15' "
        f"font-weight='650' fill='{INK}'>A viewport touches a sliver "
        f"of the remote file</text>"
        f"<text x='{left - 180}' y='44' {FONT} font-size='11' "
        f"fill='{MUTED}'>% of the S3 object fetched to satisfy one "
        f"viewport, cold client · S3 us-east-2 · median of 3 runs"
        f"</text>"
        + legend + "".join(grid) + "".join(body) +
        f"<text x='{left - 180}' y='{height - 10}' {FONT} "
        f"font-size='10' fill='{MUTED}'>Files: overview GeoParquet "
        f"68–343 MB · PMTiles 41–147 MB. Bars labeled with % and "
        f"absolute bytes.</text>"
        "</svg>"
    )
    with open(os.path.join(HERE, "remote_access_chart.svg"), "w") as f:
        f.write(svg)


if __name__ == "__main__":
    main()
