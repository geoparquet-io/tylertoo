# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""
run.py — the coalesce benchmark (#755) in one command: fetch the pinned
inputs, build each setting's archive with per-stage wall time and peak RSS,
score what the archives draw against their source, and write report.json and
report.md.

    uv run benchmarks/coalesce/run.py --profile small
    uv run benchmarks/coalesce/run.py --profile small \\
        --compare benchmarks/coalesce/baseline/small.json

Profiles, datasets, settings, windows and noise floors are in bench.toml.
The steps, each cached under --work so a rerun picks up where it stopped:

  fetch       the profile's inputs from the bench-coalesce-v1 release and the
              pinned MapLibre / pmtiles.js, each checked against its sha256.
  build       `cargo build --release --bin tylertoo`, unless --bin is given.
  tile        convert then export, per dataset × setting, `repeats` times
              each (run_matrix.run_cell). Cached per binary and settings.
  tippecanoe  the pinned tippecanoe on the datasets bench.toml lists (full
              profile). Cached per tippecanoe commit, input and arguments.
  score       render_maplibre.py per window, render_coverage.py and
              tile_metrics.py per check.
  report      report-<profile>.json and .md in <work>/runs/<binary>/, and
              the latest as report.json and report.md in <work>/.

`--only tile,report` runs just those steps; the others must be cached.
"""

from __future__ import annotations

import argparse
import glob
import hashlib
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import time
import tomllib
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE))

import run_matrix  # noqa: E402

STEPS = ("fetch", "build", "tile", "tippecanoe", "score", "report")
REFERENCE = "before-745"
MINUS = "−"


class ConfigError(ValueError):
    pass


class FetchError(RuntimeError):
    pass


# --- config -----------------------------------------------------------------

DATASET_KEYS = ("label", "asset", "sha256", "max_zoom", "max_tile_size",
                "max_compressed_tile_size", "wall_s", "rss_gib")


def load_config(path: Path) -> dict:
    cfg = tomllib.loads(Path(path).read_text())
    validate_config(cfg)
    return cfg


def validate_config(cfg: dict) -> None:
    datasets = cfg.get("datasets", {})
    settings = cfg.get("settings", {})
    for name, ds in datasets.items():
        for key in DATASET_KEYS:
            if key not in ds:
                raise ConfigError(f"dataset {name!r}: missing {key!r}")
    for name, st in settings.items():
        if not st.get("external") and ("convert" not in st or "valve" not in st):
            raise ConfigError(f"setting {name!r}: needs 'convert' and 'valve'")
    for section in ("windows", "coverage", "metrics"):
        for entry in cfg.get(section, []):
            if entry.get("dataset") not in datasets:
                raise ConfigError(f"{section} {entry.get('name')!r}: unknown dataset "
                                  f"{entry.get('dataset')!r}")
    for ds in cfg.get("tippecanoe", {}).get("datasets", {}):
        if ds not in datasets:
            raise ConfigError(f"tippecanoe: unknown dataset {ds!r}")
    for pname, prof in cfg.get("profiles", {}).items():
        for ds in prof.get("datasets", []):
            if ds not in datasets:
                raise ConfigError(f"profile {pname!r}: unknown dataset {ds!r}")
        for st in prof.get("settings", []):
            if st not in settings:
                raise ConfigError(f"profile {pname!r}: unknown setting {st!r}")


def profile_plan(cfg: dict, name: str) -> dict:
    """What a profile runs: its datasets and settings, and the windows,
    checks and tippecanoe references whose dataset is among them."""
    if name not in cfg["profiles"]:
        raise ConfigError(f"unknown profile {name!r} (have {', '.join(cfg['profiles'])})")
    prof = cfg["profiles"][name]
    datasets = list(prof["datasets"])
    settings = list(prof["settings"])

    def in_profile(section):
        return [e for e in cfg.get(section, []) if e["dataset"] in datasets]

    tippecanoe = ([d for d in cfg.get("tippecanoe", {}).get("datasets", {}) if d in datasets]
                  if "tippecanoe" in settings else [])
    return {
        "name": name, "datasets": datasets, "settings": settings,
        "internal": [s for s in settings if not cfg["settings"][s].get("external")],
        "tippecanoe": tippecanoe, "windows": in_profile("windows"),
        "coverage": in_profile("coverage"), "metrics": in_profile("metrics"),
        "repeats": prof.get("repeats", 1), "disk_gb": prof.get("disk_gb", 0),
    }


# --- cache keys and fetching ------------------------------------------------


def digest(obj) -> str:
    return hashlib.sha256(json.dumps(obj, sort_keys=True).encode()).hexdigest()


def tippecanoe_key(tippecanoe_sha: str, input_sha: str, args: list[str]) -> str:
    return digest({"tippecanoe": tippecanoe_sha, "input": input_sha, "args": args})[:16]


def cell_key(*, binary_sha: str, input_sha: str, convert: list[str], valve: str,
             max_zoom: int, max_tile_size: int, max_compressed_tile_size: int,
             repeats: int) -> str:
    return digest(locals())[:16]


def fetch(url: str, dest: Path, sha: str) -> Path:
    """dest, downloaded from url unless a copy with this sha256 is already
    there. A download that does not match is deleted, not kept."""
    if dest.exists():
        if run_matrix.sha256(dest) == sha:
            return dest
        dest.unlink()
    dest.parent.mkdir(parents=True, exist_ok=True)
    part = dest.with_name(dest.name + ".part")
    try:
        print(f"[fetch] {url}", flush=True)
        r = subprocess.run(["curl", "-fL", "-sS", "--retry", "3", "-o", str(part), url])
        if r.returncode != 0:
            raise FetchError(f"{url}: curl exited {r.returncode}")
        got = run_matrix.sha256(part)
        if got != sha:
            raise FetchError(f"{url}: sha256 {got}, expected {sha}")
        part.replace(dest)
    finally:
        part.unlink(missing_ok=True)
    return dest


# --- per-cell summaries -----------------------------------------------------


def median(vals):
    vals = [v for v in vals if v is not None]
    return statistics.median(vals) if vals else None


def stage_summary(runs: list[dict]) -> dict:
    ok = [r for r in runs if r["status"] == "ok"]
    return {"wall_s": [r["wall_s"] for r in runs],
            "median_s": median([r["wall_s"] for r in ok]),
            "cpu_s": median([r.get("cpu_s") for r in ok]),
            "peak_rss_kib": median([r["peak_rss_kib"] for r in ok])}


def summarize_cell(convert_runs: list[dict], exports: list[dict],
                   overview_bytes: int | None) -> dict:
    """Medians per stage over the repeats, plus what the last export made."""
    export_runs = [e["export_run"] for e in exports]
    status = "ok"
    for stage, runs in (("convert", convert_runs), ("export", export_runs)):
        bad = next((r for r in runs if r["status"] != "ok"), None)
        if bad and status == "ok":
            status = f"{stage} {bad['status']}"
    conv, exp = stage_summary(convert_runs), stage_summary(export_runs)
    total = None
    if status == "ok" and conv["median_s"] is not None and exp["median_s"] is not None:
        total = round(conv["median_s"] + exp["median_s"], 3)
    last = exports[-1] if exports else {}
    shas = {e.get("pmtiles_sha256") for e in exports if e.get("pmtiles_sha256")}
    return {
        "status": status, "convert": conv, "export": exp, "total_s": total,
        "overview_bytes": overview_bytes,
        **{k: last.get(k) for k in ("pmtiles_bytes", "pmtiles_sha256", "merged_features",
                                    "valve_dropped_features", "oversized_tiles", "zooms")},
        # Every repeat should write the same bytes.
        "repeats_identical": len(shas) == 1 if shas else None,
    }


# --- table cells ------------------------------------------------------------


def signed(x: float, digits: int = 1) -> str:
    x = round(x, digits)
    return f"+{x:.{digits}f}" if x >= 0 else f"{MINUS}{abs(x):.{digits}f}"


def flag(deltas: list[float], floor: float) -> str:
    if any(round(d, 1) > floor for d in deltas):
        return " ▲"
    if any(round(d, 1) < -floor for d in deltas):
        return " ▼"
    return ""


def coverage_cell(cur: dict | None, base: dict | None = None, floor_pp: float = 3.0) -> str:
    """Missing share, with the added share in parentheses from 0.5%; against
    a baseline, the change in percentage points, marked beyond the floor."""
    if not cur or cur.get("missing") is None:
        return ""
    m, a = cur["missing"], cur.get("added") or 0.0
    text = f"{m * 100:.1f}%"
    if a >= 0.005:
        text += f" (+{a * 100:.0f}%)"
    if base and base.get("missing") is not None:
        dm = (m - base["missing"]) * 100
        da = (a - (base.get("added") or 0.0)) * 100
        note = signed(dm)
        if abs(round(da, 1)) >= 0.5:
            note += f", added {signed(da)}"
        text += f" ({note}){flag([dm, da], floor_pp)}"
    return text


def pct_cell(v: float | None, base: float | None = None, floor_pp: float = 3.0,
             digits: int = 1) -> str:
    if v is None:
        return ""
    text = f"{v * 100:.{digits}f}%"
    if base is not None:
        d = (v - base) * 100
        text += f" ({signed(d, digits)}){flag([d], floor_pp)}"
    return text


def time_cell(cur: float | None, base: float | None = None, floor: float = 0.10) -> str:
    if cur is None:
        return ""
    text = f"{cur:.2f} s"
    if base:
        d = (cur / base - 1) * 100
        text += f" ({signed(d, 0)}%){flag([d], floor * 100)}"
    return text


def gib(kib) -> str:
    return f"{kib / 1048576:.2f} GiB" if kib else ""


def mb(n) -> str:
    return f"{n / 1e6:,.0f} MB" if n else ""


def count(n) -> str:
    return f"{n:,}" if n is not None else ""


def row(cells) -> str:
    return "| " + " | ".join(cells) + " |"


def render_markdown(report: dict, baseline: dict | None = None) -> str:
    noise = report["noise"]
    floor_pp, floor_t = noise["coverage_pp"], noise["time_ratio"]
    settings = report["settings"]
    host, binary = report["host"], report["binary"]
    out = [f"# Coalesce benchmark: {report['profile']} profile", ""]
    out.append(f"tylertoo `{binary.get('git')}` · {host.get('cpu')}, {host.get('cores')} cores, "
               f"{host.get('mem_gib')} GiB RAM · {report.get('created', '')}")
    out.append("")
    out.append(f"**Noise floor.** Coverage differences under {floor_pp:g} percentage points "
               f"(missing or added share) and timing differences under {floor_t:.0%} are not "
               "regressions. At full detail, small fields show about 2.5% missing and 2.5% "
               "added area from tile-grid snapping alone. Timings depend on the host; "
               "compare ratios across hosts, not seconds.")
    if baseline:
        out += ["", f"Compared with `{baseline['binary'].get('git')}` "
                    f"({baseline.get('created', '')}, {baseline['host'].get('cpu')}): "
                    "changes in parentheses, ▲/▼ beyond the noise floor."]

    def base_get(*keys):
        node = baseline
        for k in keys:
            if not isinstance(node, dict) or k not in node:
                return None
            node = node[k]
        return node

    out += ["", "## Performance", "",
            "Median of the repeats per stage. End to end is convert plus export.", ""]
    for ds, dmeta in report["datasets"].items():
        out += [f"### {dmeta['label']} ({count(dmeta.get('features'))} features)", "",
                row(["Setting", "Convert", "Convert RSS", "Export", "Export RSS", "End to end",
                     f"vs {settings.get(REFERENCE, {}).get('label', REFERENCE)}", "Overview",
                     "PMTiles", "Merged", "Oversized"]),
                row(["---"] * 11)]
        ref = report["cells"].get(f"{ds}/{REFERENCE}", {}).get("total_s")
        for st, smeta in settings.items():
            c = report["cells"].get(f"{ds}/{st}")
            b = base_get("cells", f"{ds}/{st}") or {}
            if c is None:
                t = report.get("tippecanoe", {}).get(ds)
                if st == "tippecanoe" and t and t.get("status", "ok") != "ok":
                    out.append(row([smeta["label"], f"failed: {t['status']} after "
                                    f"{t['wall_s']:.0f} s ({t.get('error', '')})"] + [""] * 9))
                elif st == "tippecanoe" and t:
                    bt = base_get("tippecanoe", ds) or {}
                    ratio = f"{t['wall_s'] / ref:.2f}×" if ref else ""
                    out.append(row([smeta["label"], "", "", "", gib(t["peak_rss_kib"]),
                                    time_cell(t["wall_s"], bt.get("wall_s"), floor_t), ratio,
                                    "", mb(t["pmtiles_bytes"]), "", ""]))
                continue
            if c["status"] != "ok":
                out.append(row([smeta["label"], c["status"]] + [""] * 9))
                continue
            ratio = f"{c['total_s'] / ref:.2f}×" if ref else ""
            out.append(row([
                smeta["label"],
                time_cell(c["convert"]["median_s"], (b.get("convert") or {}).get("median_s"),
                          floor_t),
                gib(c["convert"]["peak_rss_kib"]),
                time_cell(c["export"]["median_s"], (b.get("export") or {}).get("median_s"),
                          floor_t),
                gib(c["export"]["peak_rss_kib"]),
                time_cell(c["total_s"], b.get("total_s"), floor_t),
                ratio, mb(c.get("overview_bytes")), mb(c.get("pmtiles_bytes")),
                count(c.get("merged_features")), count(c.get("oversized_tiles")),
            ]))
        if "tippecanoe" in settings and ds in report.get("tippecanoe", {}):
            out += ["", "tippecanoe runs as one stage: its wall time is under End to end and "
                        "its peak RSS under Export RSS."]
        out.append("")

    if report.get("windows"):
        names = list(settings)
        out += ["## Rendered coverage (MapLibre)", "",
                "Share of the source area missing in each window, drawn by MapLibre GL JS in "
                "headless Chromium; added area in parentheses when at least 0.5%.", "",
                row(["Window", "Zoom"] + [settings[s]["label"] for s in names]),
                row(["---"] * (2 + len(names)))]
        for wname, w in report["windows"].items():
            zooms = w.get("report_zooms") or sorted(int(z) for z in w["zooms"])
            for i, z in enumerate(zooms):
                at = w["zooms"].get(str(z), {})
                cells = [coverage_cell(at.get(s), base_get("windows", wname, "zooms", str(z), s),
                                       floor_pp) for s in names]
                out.append(row([w["label"] if i == 0 else "", str(z)] + cells))
        out.append("")

    if report.get("coverage"):
        out += ["## Tile coverage (render_coverage.py)", "",
                row(["Check", "Setting", "Missing", "Added", "Tile features"]),
                row(["---"] * 5)]
        for cname, c in report["coverage"].items():
            for i, (st, v) in enumerate(c["settings"].items()):
                b = base_get("coverage", cname, "settings", st) or {}
                out.append(row([c["label"] if i == 0 else "", settings[st]["label"],
                                pct_cell(v["missing"], b.get("missing"), floor_pp, 2),
                                pct_cell(v["added"], b.get("added"), floor_pp, 3),
                                count(v.get("features"))]))
        out.append("")

    if report.get("metrics"):
        out += ["## Drawn share per tile (tile_metrics.py)", "",
                "Per zoom: median drawn share of the sampled tiles, worst in parentheses. "
                "Edge holes: share of the source along tile seams that neither tile draws. "
                "Seam excess: colour changes across seams minus those across a control line.",
                ""]
        for mname, m in report["metrics"].items():
            zooms = [str(z) for z in m["zooms"]]
            named = m.get("named") or []
            out += [f"### {m['label']}", "",
                    row(["Setting"] + [f"z{z}" for z in zooms] + named
                        + ["Edge holes", "Seam excess"]),
                    row(["---"] * (3 + len(zooms) + len(named)))]
            for st, v in m["settings"].items():
                cells = [settings[st]["label"]]
                for z in zooms:
                    zv = v["zooms"].get(z)
                    if not zv or zv.get("median") is None:
                        cells.append("")
                        continue
                    bz = base_get("metrics", mname, "settings", st, "zooms", z) or {}
                    cells.append(f"{pct_cell(zv['median'], bz.get('median'), floor_pp)} "
                                 f"({zv['min'] * 100:.1f}%)")
                for t in named:
                    cells.append(pct_cell(v.get("named", {}).get(t),
                                          base_get("metrics", mname, "settings", st, "named", t),
                                          floor_pp))
                for key in ("edge_gap", "excess_change"):
                    cells.append(" / ".join(
                        f"z{z} {v['zooms'][z][key]:.2f}" for z in zooms
                        if v["zooms"].get(z, {}).get(key) is not None))
                out.append(row(cells))
            out.append("")
    return "\n".join(out).rstrip() + "\n"


# --- steps ------------------------------------------------------------------


def sh(cmd, **kw) -> str:
    try:
        return subprocess.run(cmd, capture_output=True, text=True, **kw).stdout.strip()
    except OSError:
        return ""


def host_info() -> dict:
    cpu = next((ln.split(":", 1)[1].strip() for ln in sh(["lscpu"]).splitlines()
                if ln.startswith("Model name")), platform.processor())
    mem_kib = next((int(ln.split()[1]) for ln in Path("/proc/meminfo").read_text().splitlines()
                    if ln.startswith("MemTotal")), 0)
    return {"cpu": cpu, "arch": platform.machine(), "cores": os.cpu_count(),
            "mem_gib": round(mem_kib / 1048576, 1), "kernel": platform.release(),
            "python": platform.python_version()}


def binary_info(binary: Path) -> dict:
    git = sh(["git", "rev-parse", "--short", "HEAD"], cwd=REPO)
    # Dirty only if the source the binary is built from has changed.
    if sh(["git", "status", "--porcelain", "--untracked-files=no", "--",
           "crates", "Cargo.toml", "Cargo.lock"], cwd=REPO):
        git += "-dirty"
    return {"path": str(binary), "sha256": run_matrix.sha256(binary), "git": git,
            "version": sh([str(binary), "--version"]), "file": sh(["file", "-b", str(binary)]),
            "rustc": sh(["rustc", "-V"])}


def preflight(plan: dict, steps: set, work: Path) -> None:
    problems = []
    if steps & {"tile", "tippecanoe"} and not Path("/usr/bin/time").exists():
        problems.append("GNU time is missing (apt install time)")
    for tool in ("curl", "uv"):
        if not shutil.which(tool):
            problems.append(f"{tool} is not on PATH")
    work.mkdir(parents=True, exist_ok=True)
    free_gb = shutil.disk_usage(work).free / 1e9
    if steps & {"fetch", "tile", "tippecanoe"} and free_gb < plan["disk_gb"]:
        problems.append(f"{work}: {free_gb:.0f} GB free, the {plan['name']} profile needs "
                        f"about {plan['disk_gb']} GB")
    browsers = os.environ.get("PLAYWRIGHT_BROWSERS_PATH",
                              str(Path.home() / ".cache" / "ms-playwright"))
    if "score" in steps and plan["windows"] and not glob.glob(
            os.path.join(browsers, "chromium_headless_shell-*")):
        problems.append("Playwright Chromium is missing: uvx --from playwright==1.64.0 "
                        "playwright install --with-deps chromium-headless-shell")
    if problems:
        raise SystemExit("preflight:\n  " + "\n  ".join(problems))


def build_binary(args, steps: set) -> Path:
    if args.bin:
        return args.bin.expanduser().resolve()
    binary = REPO / "target" / "release" / "tylertoo"
    if "build" in steps:
        print("[build] cargo build --release --bin tylertoo", flush=True)
        subprocess.run(["cargo", "build", "--release", "--bin", "tylertoo"], cwd=REPO,
                       check=True)
    if not binary.exists():
        raise SystemExit(f"{binary} is missing: run the build step or pass --bin")
    return binary


def read_cached(path: Path, key: str) -> dict | None:
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return None
    return data if data.get("key") == key else None


def tile_cells(cfg, plan, args, steps, binary, binfo, inputs: Path, run_dir: Path) -> dict:
    tiles = run_dir / "tiles"
    tiles.mkdir(parents=True, exist_ok=True)
    results = run_dir / "results.jsonl"
    cells = {}
    for ds_name in plan["datasets"]:
        ds = cfg["datasets"][ds_name]
        repeats = args.repeats or ds.get("repeats") or plan["repeats"]
        for st_name in plan["internal"]:
            st = cfg["settings"][st_name]
            name = f"{ds_name}.{st_name}"
            key = cell_key(binary_sha=binfo["sha256"], input_sha=ds["sha256"],
                           convert=st["convert"], valve=st["valve"], max_zoom=ds["max_zoom"],
                           max_tile_size=ds["max_tile_size"],
                           max_compressed_tile_size=ds["max_compressed_tile_size"],
                           repeats=repeats)
            summary_path = tiles / f"{name}.json"
            pm = tiles / f"{name}.{st['valve']}.pmtiles"
            cached = read_cached(summary_path, key)
            if cached and pm.exists():
                cells[f"{ds_name}/{st_name}"] = cached
                continue
            if "tile" not in steps:
                raise SystemExit(f"{name} is not tiled for this binary: run the tile step")

            def record(rec, ds_name=ds_name, st_name=st_name):
                with results.open("a") as f:
                    f.write(json.dumps({"dataset": ds_name, "setting": st_name,
                                        "git": binfo["git"], **rec}) + "\n")

            records = run_matrix.run_cell(
                str(binary), inputs / ds["asset"], tiles, name, st["convert"], [st["valve"]],
                max_zoom=ds["max_zoom"], max_tile_size=str(ds["max_tile_size"]),
                max_compressed_tile_size=str(ds["max_compressed_tile_size"]),
                wall_s=ds["wall_s"], rss_gib=ds["rss_gib"], repeats=repeats,
                convert_repeats=repeats, record=record)
            conv = records[0]
            summary = summarize_cell(conv["convert_runs"], records[1:],
                                     conv["convert_run"].get("overview_bytes"))
            summary.update(key=key, dataset=ds_name, setting=st_name, repeats=repeats,
                           pmtiles=pm.name)
            summary_path.write_text(json.dumps(summary, indent=2))
            cells[f"{ds_name}/{st_name}"] = summary
    return cells


def tippecanoe_refs(cfg, plan, steps, inputs: Path, work: Path) -> dict:
    refs = {}
    if not plan["tippecanoe"]:
        return refs
    tcfg = cfg["tippecanoe"]
    setup = REPO / "benchmarks" / "e2e" / "setup_tippecanoe.sh"
    # Builds once, verifies the pinned commit, and writes the lock file.
    subprocess.run(["bash", str(setup)], check=True)
    lock = json.loads((REPO / "benchmarks" / "e2e" / "tippecanoe.lock.json").read_text())
    for ds_name in plan["tippecanoe"]:
        ds = cfg["datasets"][ds_name]
        targs = tcfg["datasets"][ds_name]["args"]
        key = tippecanoe_key(lock["sha"], ds["sha256"], targs)
        d = work / "tippecanoe" / f"{ds_name}-{key}"
        meta = d / "run.json"
        cached = read_cached(meta, key)
        # A failed run is a result too, and is not retried: delete the
        # directory to try again.
        if cached and (cached.get("status", "ok") != "ok"
                       or (d / "archive.pmtiles").exists()):
            print(f"[tippecanoe] {ds_name}: cached, {cached.get('status', 'ok')} ({d})",
                  flush=True)
            refs[ds_name] = {**cached, "path": str(d / "archive.pmtiles")}
            continue
        if "tippecanoe" not in steps:
            raise SystemExit(f"tippecanoe {ds_name} is not cached: run the tippecanoe step")
        d.mkdir(parents=True, exist_ok=True)
        fgb = d / "input.fgb"
        if not fgb.exists():
            tmp = d / "input.tmp.fgb"
            print(f"[tippecanoe] {ds_name}: gpio convert -> FlatGeobuf", flush=True)
            subprocess.run(["uvx", "--from", tcfg["gpio"], "gpio", "convert", "flatgeobuf",
                            str(inputs / ds["asset"]), str(tmp)], check=True)
            tmp.replace(fgb)
        pm = d / "archive.pmtiles"
        print(f"[tippecanoe] {ds_name}: {' '.join(targs)} (this takes a while)", flush=True)
        run = run_matrix.run_capped([lock["binary"], "-o", str(pm), "-f", *targs, str(fgb)],
                                    tcfg["wall_s"], tcfg["rss_gib"])
        if run["status"] != "ok":
            tail = (run["stderr_tail"] or "").split("\tCommand being timed")[0].splitlines()
            code = next((ln.rsplit(" ", 1)[1] for ln in tail
                         if ln.startswith("Command exited with non-zero status")), None)
            rec = {"key": key, "tippecanoe": lock, "args": targs,
                   "status": f"exit {code}" if code else run["status"],
                   "error": next((ln for ln in reversed(tail)
                                  if ln.strip() and not ln.startswith("Command exited")), ""),
                   "wall_s": run["wall_s"], "cpu_s": run["cpu_s"],
                   "peak_rss_kib": run["peak_rss_kib"], "pmtiles_bytes": None}
            meta.write_text(json.dumps(rec, indent=2))
            pm.unlink(missing_ok=True)
            fgb.unlink()
            print(f"[tippecanoe] {ds_name}: {rec['status']} after {rec['wall_s']:.0f} s: "
                  f"{rec['error']}", flush=True)
            refs[ds_name] = {**rec, "path": str(pm)}
            continue
        rec = {"key": key, "tippecanoe": lock, "args": targs, "wall_s": run["wall_s"],
               "cpu_s": run["cpu_s"], "peak_rss_kib": run["peak_rss_kib"],
               "pmtiles_bytes": pm.stat().st_size, "pmtiles_sha256": run_matrix.sha256(pm)}
        meta.write_text(json.dumps(rec, indent=2))
        # The archive is the cache; the FlatGeobuf is only its input.
        fgb.unlink()
        refs[ds_name] = {**rec, "path": str(pm)}
    return refs


def uv_script(script: str, *argv) -> list[str]:
    return ["uv", "run", "-q", str(HERE / script), *map(str, argv)]


def run_scorers(pending: list[tuple], steps: set, jobs: int) -> None:
    """Run each (out, key, what, cmd), `jobs` at a time; each writes its
    output to out/score.log and, when it succeeds, out/done.json."""
    if "score" not in steps:
        raise SystemExit(f"{pending[0][2]} is not scored: run the score step")

    def one(job):
        out, key, what, cmd = job
        out.mkdir(parents=True, exist_ok=True)
        print(f"[score] {what}", flush=True)
        t0 = time.monotonic()
        r = subprocess.run(cmd, capture_output=True, text=True)
        log = out / "score.log"
        log.write_text(r.stdout + r.stderr)
        if r.returncode != 0:
            raise SystemExit(f"{what}: exit {r.returncode}, see {log}\n{r.stderr[-2000:]}")
        (out / "done.json").write_text(json.dumps({"key": key}))
        print(f"[score] {what}: {time.monotonic() - t0:.0f} s", flush=True)

    with ThreadPoolExecutor(max_workers=jobs) as pool:
        list(pool.map(one, pending))


def score(cfg, plan, steps, inputs: Path, js: Path, run_dir: Path, archives: dict,
          jobs: int = 4) -> dict:
    """archives: (dataset, setting) -> (path, sha256). The scorers that are
    not cached run in parallel, then every result is read."""
    pending: list[tuple] = []

    def queue(out: Path, key: str, what: str, cmd: list[str]) -> bool:
        if read_cached(out / "done.json", key):
            return True
        pending.append((out, key, what, cmd))
        return False

    res = collect_scores(cfg, plan, inputs, js, run_dir, archives, queue)
    if pending:
        run_scorers(pending, steps, jobs)
        pending.clear()
        res = collect_scores(cfg, plan, inputs, js, run_dir, archives, queue)
    return res


def collect_scores(cfg, plan, inputs: Path, js: Path, run_dir: Path, archives: dict,
                   queue) -> dict:
    """Each scorer's command, passed to `queue`; the results of those it
    reports cached."""
    res = {"windows": {}, "coverage": {}, "metrics": {}}

    def labels(ds):
        return [(s, archives[(ds, s)]) for s in plan["settings"] if (ds, s) in archives]

    for w in plan["windows"]:
        ds = cfg["datasets"][w["dataset"]]
        out = run_dir / "score" / "windows" / w["name"]
        lab = labels(w["dataset"])
        cmd = uv_script("render_maplibre.py", "--source", inputs / ds["asset"],
                        "--zoom", w["zoom"], "--tile", ",".join(map(str, w["tile"])),
                        "--window", ",".join(map(str, w["window"])),
                        "--draw-zoom", ",".join(map(str, w["draw_zooms"])),
                        "--js-dir", js, "--out", out,
                        *[a for s, (p, _) in lab for a in ("--label", f"{s}={p}")])
        if not queue(out, digest([w, [(s, sha) for s, (_, sha) in lab], cfg["js"]]),
                     f"window {w['name']}", cmd):
            continue
        images = json.loads((out / "maplibre.json").read_text())["images"]
        zooms = {}
        for dz in w["draw_zooms"]:
            zooms[str(dz)] = {s: {"missing": images[f"z{dz}-{s}"]["missing_share"],
                                  "added": images[f"z{dz}-{s}"]["added_share"]}
                              for s, _ in lab if f"z{dz}-{s}" in images}
        res["windows"][w["name"]] = {"label": w["label"], "dataset": w["dataset"],
                                     "report_zooms": w["report_zooms"], "zooms": zooms}

    for c in plan["coverage"]:
        ds = cfg["datasets"][c["dataset"]]
        out = run_dir / "score" / "coverage" / c["name"]
        lab = labels(c["dataset"])
        cmd = uv_script("render_coverage.py", "--source", inputs / ds["asset"],
                        "--zoom", c["zoom"], "--center", ",".join(map(str, c["center"])),
                        "--span", c["span"], "--out", out,
                        *[a for s, (p, _) in lab for a in ("--label", f"{s}={p}")])
        if not queue(out, digest([c, [(s, sha) for s, (_, sha) in lab]]),
                     f"coverage {c['name']}", cmd):
            continue
        data = json.loads((out / "coverage.json").read_text())
        res["coverage"][c["name"]] = {
            "label": c["label"], "zoom": c["zoom"], "tiles": data["tiles"],
            "settings": {s: {"missing": v["missing_share"], "added": v["added_share"],
                             "features": v.get("features_in_view")}
                         for s, v in data["images"].items()}}

    for m in plan["metrics"]:
        ds = cfg["datasets"][m["dataset"]]
        out_dir = run_dir / "score" / "metrics" / m["name"]
        per = {}
        for s, (p, sha) in labels(m["dataset"]):
            out = out_dir / s
            cmd = uv_script("tile_metrics.py", p, "--source", inputs / ds["asset"],
                            "--zooms", ",".join(map(str, m["zooms"])),
                            "--max-tiles", m["max_tiles"],
                            "--raw-limit", ds["max_tile_size"],
                            "--gz-limit", ds["max_compressed_tile_size"],
                            "--json", out / "metrics.json")
            if m.get("color_attr"):
                cmd += ["--color-attr", m["color_attr"]]
            if m.get("named"):
                cmd += ["--named", ",".join(m["named"])]
            if not queue(out, digest([m, sha]), f"metrics {m['name']} {s}", cmd):
                continue
            data = json.loads((out / "metrics.json").read_text())
            per[s] = {"zooms": {z: {"median": v["drawn_share"]["median"],
                                    "min": v["drawn_share"]["min"],
                                    "edge_gap": v.get("edge_gap"),
                                    "excess_change": v.get("excess_change")}
                                for z, v in data["zooms"].items()},
                      "named": data.get("named", {}), "sizes": data.get("sizes")}
        res["metrics"][m["name"]] = {"label": ds["label"], "dataset": m["dataset"],
                                     "zooms": m["zooms"], "named": m.get("named", []),
                                     "settings": per}
    return res


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--profile", default="small")
    ap.add_argument("--config", type=Path, default=HERE / "bench.toml")
    ap.add_argument("--work", type=Path, default=Path("~/bench-data/coalesce"),
                    help="inputs, archives and reports (default ~/bench-data/coalesce)")
    ap.add_argument("--bin", type=Path, help="tylertoo binary (default: build target/release)")
    ap.add_argument("--repeats", type=int, help="override every dataset's repeats")
    ap.add_argument("--compare", type=Path, help="a previous report.json to compare against")
    ap.add_argument("--only", help=f"comma list of steps ({','.join(STEPS)})")
    ap.add_argument("--jobs", type=int, default=4, help="scorers run in parallel")
    ap.add_argument("--load-max", type=float, default=run_matrix.LOAD_MAX,
                    help="wait for the 1-minute load average to drop below this before each run")
    args = ap.parse_args()

    cfg = load_config(args.config)
    plan = profile_plan(cfg, args.profile)
    steps = set(args.only.split(",")) if args.only else set(STEPS)
    if steps - set(STEPS):
        raise SystemExit(f"unknown steps: {', '.join(sorted(steps - set(STEPS)))}")
    run_matrix.LOAD_MAX = args.load_max
    work = args.work.expanduser().resolve()
    preflight(plan, steps, work)
    t0 = time.monotonic()

    inputs, js = work / "inputs", work / "js"
    for ds_name in plan["datasets"]:
        ds = cfg["datasets"][ds_name]
        dest = inputs / ds["asset"]
        if "fetch" in steps:
            fetch(f"{cfg['release']}/{ds['asset']}", dest, ds["sha256"])
        elif not dest.exists():
            raise SystemExit(f"{dest} is missing: run the fetch step")
    for name, spec in cfg["js"].items():
        fname = Path(spec["url"]).name
        if "fetch" in steps:
            fetch(spec["url"], js / fname, spec["sha256"])

    binary = build_binary(args, steps)
    binfo = binary_info(binary)
    run_dir = work / "runs" / binfo["sha256"][:12]
    print(f"[run] {plan['name']} profile, tylertoo {binfo['git']} -> {run_dir}", flush=True)

    cells = tile_cells(cfg, plan, args, steps, binary, binfo, inputs, run_dir)
    refs = tippecanoe_refs(cfg, plan, steps, inputs, work)
    archives = {}
    for cid, c in cells.items():
        ds_name, st_name = cid.split("/")
        if c["status"] == "ok":
            archives[(ds_name, st_name)] = (run_dir / "tiles" / c["pmtiles"], c["pmtiles_sha256"])
    for ds_name, r in refs.items():
        if r.get("status", "ok") == "ok":
            archives[(ds_name, "tippecanoe")] = (Path(r["path"]), r["pmtiles_sha256"])
    scores = score(cfg, plan, steps, inputs, js, run_dir, archives, args.jobs)

    report = {
        "schema": 1, "profile": plan["name"],
        "created": datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC"),
        "config_sha256": run_matrix.sha256(args.config),
        "noise": cfg["noise"], "host": host_info(),
        "binary": {k: v for k, v in binfo.items() if k != "path"},
        "datasets": {d: {k: cfg["datasets"][d].get(k)
                         for k in ("label", "asset", "sha256", "features", "max_zoom",
                                   "max_tile_size", "max_compressed_tile_size")}
                     for d in plan["datasets"]},
        "settings": {s: {k: v for k, v in cfg["settings"][s].items()}
                     for s in plan["settings"]},
        "cells": cells,
        # Machine-local paths stay out, so a report can be committed as a baseline.
        "tippecanoe": {d: {**{k: v for k, v in r.items() if k != "path"},
                           "tippecanoe": {k: v for k, v in r["tippecanoe"].items()
                                          if k != "binary"}}
                       for d, r in refs.items()},
        **scores,
        "elapsed_s": round(time.monotonic() - t0, 1),
    }
    baseline = json.loads(args.compare.read_text()) if args.compare else None
    md = render_markdown(report, baseline)
    # Per profile in the run's directory; the latest of any profile in --work.
    for d, stem in ((run_dir, f"report-{plan['name']}"), (work, "report")):
        (d / f"{stem}.json").write_text(json.dumps(report, indent=2) + "\n")
        (d / f"{stem}.md").write_text(md)
    print(md)
    print(f"[run] done in {report['elapsed_s']:.0f} s: {work / 'report.md'}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
