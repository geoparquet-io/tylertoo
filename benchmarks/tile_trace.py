#!/usr/bin/env python3
"""Summarize a `TYLERTOO_TILE_TRACE` export trace (#876).

    uv run python -I benchmarks/tile_trace.py trace.jsonl [--top 10] [--json out.json]

Prints, for each export in the trace (one per `start` event):

(a) per zoom: wall (first tile start to last tile end), tiles, and the
    longest tiles with their members, vertices, passes, stolen and foreign
    time, and their valve passes' stage times (core-seconds, and the wall
    the tile's own thread spent in each stage);
(b) per zoom: mean occupancy (threads in traced tile work, not waiting) and
    its timeline;
(c) scheduler holds: admission, two-wave window, drain, commit waits, serial
    levels, store fills, and each group's barrier idle: the time from its
    last wave spawn to its end while occupancy was under half the pool;
(d) per nested wait site: the owner's wall in the call and the core time of
    its closure bodies on every thread;
(e) per band collect (a store fill, or a wave's own read): where its driving
    thread's wall went, and its reader threads' busy and blocked time;

and with `--json` writes the same as one JSON object. Standard library only.
`context/PROFILING.md` lists the trace's events and fields.
"""

import argparse
import json
import sys
from collections import defaultdict

STAGES = ["coalesce", "build", "quantize", "union", "finalize", "gzip"]
NS = 1e9


def segments(path):
    """The trace's events, split into one list per export."""
    out = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            ev = json.loads(line)
            if ev["ev"] == "start" or not out:
                out.append([])
            out[-1].append(ev)
    return out


def secs(ns):
    return round(ns / NS, 4)


def occ_between(occ, t0, t1):
    """The occupancy samples in [t0, t1], as (t_ns, busy)."""
    return [(t, b) for t, b in occ if t0 <= t <= t1]


def time_below(occ, t0, t1, limit):
    """Nanoseconds of [t0, t1] during which occupancy was under `limit`,
    each sample holding until the next one."""
    total = 0
    for (t, b), nxt in zip(occ, occ[1:] + [(None, None)]):
        end = nxt[0] if nxt[0] is not None else t1
        lo, hi = max(t, t0), min(end, t1)
        if hi > lo and b < limit:
            total += hi - lo
    return total


def summarize(events, top):
    by = defaultdict(list)
    for e in events:
        by[e["ev"]].append(e)
    start = by["start"][0] if by["start"] else {}
    threads = start.get("threads") or 0
    occ = sorted((e["t_ns"], e["busy"] or 0) for e in by["occ"])

    passes = defaultdict(list)
    for p in by["pass"]:
        if "z" in p:
            passes[(p["z"], p["x"], p["y"])].append(p)

    zooms = {}
    for z in sorted({t["z"] for t in by["tile"]}):
        tiles = [t for t in by["tile"] if t["z"] == z]
        t0 = min(t["start_ns"] for t in tiles)
        t1 = max(t["end_ns"] for t in tiles)
        longest = sorted(tiles, key=lambda t: t["end_ns"] - t["start_ns"], reverse=True)[:top]
        rows = []
        for t in longest:
            ps = passes.get((t["z"], t["x"], t["y"]), [])
            kinds = defaultdict(int)
            for p in ps:
                kinds[p["kind"]] += 1
            rows.append({
                "tile": f"{t['z']}/{t['x']}/{t['y']}",
                "wall_s": secs(t["end_ns"] - t["start_ns"]),
                "start_s": secs(t["start_ns"]),
                "level": t.get("level"), "wave": t.get("wave"), "part": t.get("part"),
                "members": t["members"], "vertices": t["vertices"], "passes": t["passes"],
                "stolen_s": secs(t["stolen_ns"]), "foreign_s": secs(t["foreign_ns"]),
                "pass_kinds": dict(kinds),
                "pass_wall_s": secs(sum(p["end_ns"] - p["start_ns"] for p in ps)),
                "stage_core_s": {s: secs(sum(p["core_ns"][s] for p in ps)) for s in STAGES},
                "stage_wall_s": {s: secs(sum(p["wall_ns"][s] for p in ps)) for s in STAGES},
                "union_hits": sum(p["union_hits"] for p in ps),
                "union_misses": sum(p["union_misses"] for p in ps),
                "repaired": sum(p["repaired"] for p in ps),
                "largest_component_vertices": max((p["largest_component_vertices"] for p in ps), default=0),
            })
        window = occ_between(occ, t0, t1)
        n = 20
        step = max((t1 - t0) / n, 1)
        timeline = []
        for i in range(n):
            b = [x for t, x in window if t0 + i * step <= t < t0 + (i + 1) * step]
            timeline.append(round(sum(b) / len(b), 1) if b else None)
        zooms[z] = {
            "wall_s": secs(t1 - t0), "start_s": secs(t0), "end_s": secs(t1),
            "tiles": len(tiles),
            "tile_core_s": secs(sum(t["end_ns"] - t["start_ns"] - t["stolen_ns"] - t["foreign_ns"] for t in tiles)),
            "stolen_s": secs(sum(t["stolen_ns"] for t in tiles)),
            "foreign_s": secs(sum(t["foreign_ns"] for t in tiles)),
            "occupancy_mean": round(sum(x for _, x in window) / len(window), 2) if window else None,
            "occupancy_timeline": timeline,
            "longest": rows,
        }

    def total(ev):
        return secs(sum(e["end_ns"] - e["start_ns"] for e in by[ev]))

    groups = []
    for g in by["group"]:
        levels = set(g["levels"])
        spawns = [w["t_ns"] for w in by["wave"] if w["stage"] == "spawned" and w["level"] in levels]
        last = max(spawns) if spawns else g["start_ns"]
        groups.append({
            "zooms": g["zooms"], "wall_s": secs(g["end_ns"] - g["start_ns"]),
            "tail_s": secs(g["end_ns"] - last),
            "barrier_idle_s": secs(time_below(occ, last, g["end_ns"], threads / 2)),
        })
    holds = {
        "admit_wait_s": total("admit_wait"),
        "window_wait_s": total("window_wait"),
        "drain_wait_s": total("drain_wait"),
        "commit_wait_s": total("commit_wait"),
        "serial_level_s": total("serial_level"),
        "serial_levels": [e["zoom"] for e in by["serial_level"]],
        "fill_s": total("fill"),
        "group_barrier_idle_s": round(sum(g["barrier_idle_s"] for g in groups), 4),
        "groups": groups,
    }

    sites = defaultdict(lambda: defaultdict(int))
    for w in by["wait"]:
        s = sites[w["site"]]
        for k in ("calls", "wall_ns", "core_ns", "bodies"):
            s[k] += w[k]
        s["max_wall_ns"] = max(s["max_wall_ns"], w["max_wall_ns"])
    waits = {
        name: {
            "calls": s["calls"], "bodies": s["bodies"],
            "owner_wall_s": secs(s["wall_ns"]), "body_core_s": secs(s["core_ns"]),
            "max_call_s": secs(s["max_wall_ns"]),
        }
        for name, s in sorted(sites.items(), key=lambda kv: -kv[1]["wall_ns"])
    }
    collects = []
    for c in sorted(by["collect"], key=lambda c: c["start_ns"]):
        c0, c1 = c["start_ns"], c["end_ns"]
        fill = next((f for f in by["fill"] if f.get("level") == c["level"]
                     and f["start_ns"] <= c0 and c1 <= f["end_ns"]), None)
        reads = [r for r in by["read"] if c0 <= r["start_ns"] and r["end_ns"] <= c1]
        wall = c1 - c0
        window = occ_between(occ, c0, c1)
        row = {
            "kind": "fill" if fill else "wave", "zoom": c["zoom"], "level": c["level"],
            "start_s": secs(c0), "wall_s": secs(wall),
            "fill_extra_s": secs(fill["end_ns"] - fill["start_ns"] - wall) if fill else None,
            "groups": c["groups"], "recv_s": secs(c["recv_ns"]), "join_s": secs(c["join_ns"]),
            "inject_s": secs(c["inject_ns"]), "clip_wall_s": secs(c["clip_ns"]),
            "clip_core_s": secs(c["clip_core_ns"]), "finish_s": secs(c["finish_ns"]),
            "serial_s": secs(c["serial_ns"]),
            "rest_s": secs(wall - c["recv_ns"] - c["join_ns"] - c["serial_ns"]),
            "occupancy_mean": round(sum(x for _, x in window) / len(window), 1) if window else None,
            "readers": sum(r["workers"] for r in reads),
            "reader_wall_s": secs(sum(r["workers"] * (r["end_ns"] - r["start_ns"]) for r in reads)),
            "reader_busy_s": secs(sum(sum(r["read_ns"]) for r in reads)),
            "reader_send_blocked_s": secs(sum(sum(r["send_ns"]) for r in reads)),
            "merge_wait_s": secs(sum(r["merge_wait_ns"] for r in reads)),
            "merge_deliver_s": secs(sum(r["deliver_ns"] for r in reads)),
        }
        collects.append(row)
    end = by["end"][0]["t_ns"] if by["end"] else max(e.get("t_ns", 0) for e in events)
    levels = [e["t_ns"] for e in by["phase"] if e["phase"] == "levels"]
    return {
        "threads": threads, "export_s": secs(end),
        "levels_start_s": secs(levels[0]) if levels else None,
        "occupancy_mean": round(sum(b for _, b in occ) / len(occ), 2) if occ else None,
        "zooms": zooms, "holds": holds, "waits": waits, "collects": collects,
    }


def report(s):
    p = print
    p(f"threads {s['threads']}  export {s['export_s']:.2f}s  levels from {s['levels_start_s']}s  "
      f"mean occupancy {s['occupancy_mean']}")
    p("\n(a) per zoom: wall, tiles, longest tiles")
    for z, d in s["zooms"].items():
        p(f"z{z}: wall {d['wall_s']:.3f}s [{d['start_s']:.2f}..{d['end_s']:.2f}]  tiles {d['tiles']}  "
          f"tile own {d['tile_core_s']:.2f}s  stolen {d['stolen_s']:.2f}s  foreign {d['foreign_s']:.2f}s")
        for r in d["longest"]:
            core = " ".join(f"{k}={v:.2f}" for k, v in r["stage_core_s"].items() if v)
            wall = " ".join(f"{k}={v:.2f}" for k, v in r["stage_wall_s"].items() if v)
            p(f"   {r['tile']:>14} {r['wall_s']:7.3f}s @{r['start_s']:.2f} m={r['members']} v={r['vertices']} "
              f"passes={r['passes']} {r['pass_kinds']} stolen={r['stolen_s']:.3f} foreign={r['foreign_s']:.3f}")
            p(f"   {'':>14} core[{core}] own-wall[{wall}] hits={r['union_hits']} misses={r['union_misses']} "
              f"repaired={r['repaired']} largest_comp={r['largest_component_vertices']}")
    p("\n(b) per zoom: occupancy mean and timeline (20 slices of the zoom's wall)")
    for z, d in s["zooms"].items():
        tl = " ".join("." if x is None else f"{x:g}" for x in d["occupancy_timeline"])
        p(f"z{z}: mean {d['occupancy_mean']}  [{tl}]")
    h = s["holds"]
    p("\n(c) scheduler holds (seconds)")
    p(f"admit_wait {h['admit_wait_s']}  window_wait {h['window_wait_s']}  drain_wait {h['drain_wait_s']}  "
      f"commit_wait {h['commit_wait_s']}  serial_level {h['serial_level_s']} {h['serial_levels']}  "
      f"fill {h['fill_s']}  group_barrier_idle {h['group_barrier_idle_s']}")
    for g in h["groups"]:
        p(f"   group z{g['zooms'][0]}..z{g['zooms'][-1]}: wall {g['wall_s']}s, tail after last spawn "
          f"{g['tail_s']}s, idle under half the pool {g['barrier_idle_s']}s")
    p("\n(d) wait sites: owner wall in the call vs closure-body core time")
    p(f"{'site':>16} {'calls':>8} {'bodies':>9} {'owner_wall_s':>12} {'body_core_s':>11} {'max_call_s':>10}")
    for name, w in s["waits"].items():
        p(f"{name:>16} {w['calls']:>8} {w['bodies']:>9} {w['owner_wall_s']:>12.3f} "
          f"{w['body_core_s']:>11.3f} {w['max_call_s']:>10.3f}")
    p("\n(e) band collects (seconds): driving thread = recv + join + serial + rest;"
      " join = inject + max(finish, clip); readers: busy / blocked in send of their summed wall")
    p(f"{'kind':>4} {'z':>3} {'start':>6} {'wall':>6} {'xtra':>5} {'grp':>4} {'recv':>6} {'join':>6} "
      f"{'inject':>6} {'clip':>6} {'core':>7} {'finish':>6} {'serial':>6} {'rest':>5} {'occ':>5} "
      f"{'rd':>3} {'rd_wall':>7} {'rd_busy':>7} {'rd_send':>7} {'mrg_wait':>8} {'mrg_dlv':>7}")
    for c in s["collects"]:
        xtra = "" if c["fill_extra_s"] is None else f"{c['fill_extra_s']:.2f}"
        p(f"{c['kind']:>4} {c['zoom']:>3} {c['start_s']:>6.2f} {c['wall_s']:>6.2f} {xtra:>5} {c['groups']:>4} "
          f"{c['recv_s']:>6.2f} {c['join_s']:>6.2f} {c['inject_s']:>6.2f} {c['clip_wall_s']:>6.2f} "
          f"{c['clip_core_s']:>7.2f} {c['finish_s']:>6.2f} {c['serial_s']:>6.2f} {c['rest_s']:>5.2f} "
          f"{c['occupancy_mean'] if c['occupancy_mean'] is not None else '-':>5} {c['readers']:>3} "
          f"{c['reader_wall_s']:>7.2f} {c['reader_busy_s']:>7.2f} {c['reader_send_blocked_s']:>7.2f} "
          f"{c['merge_wait_s']:>8.2f} {c['merge_deliver_s']:>7.2f}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("trace")
    ap.add_argument("--top", type=int, default=10)
    ap.add_argument("--json", metavar="PATH", help="also write the summary as JSON ('-' for stdout)")
    a = ap.parse_args()
    out = [summarize(seg, a.top) for seg in segments(a.trace)]
    for i, s in enumerate(out):
        if len(out) > 1:
            print(f"=== export {i + 1} of {len(out)} ===")
        report(s)
    if a.json:
        text = json.dumps(out if len(out) > 1 else out[0], indent=1)
        if a.json == "-":
            sys.stdout.write(text + "\n")
        else:
            with open(a.json, "w") as f:
                f.write(text + "\n")


if __name__ == "__main__":
    main()
