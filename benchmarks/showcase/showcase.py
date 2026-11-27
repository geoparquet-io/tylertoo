# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Pin Overture inputs and collect the Africa streaming release run.

Footer reads for pin and collect-stream use pyarrow; pin checks use only
anonymous HTTPS requests. Run the release tier with benchmarks/release.sh.
"""
from __future__ import annotations

import argparse
import json
import re
import statistics
import subprocess
import sys
import tomllib
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(HERE.parent))
import run_manifest as rm

MANIFEST = HERE / "manifest-africa.toml"
with MANIFEST.open("rb") as f:
    _manifest = tomllib.load(f)
PREFIX = _manifest["cells"]["ovt-s3"]["args"][0]
RELEASE = re.search(r"/release/([^/]+)/", PREFIX)[1]
BUCKET = urllib.parse.urlsplit(PREFIX).netloc
PARTS_TSV = ROOT / _manifest["inputs"]["overture-listing"]["path"]
RESULTS = ROOT / "benchmarks/results"


def region_bbox() -> tuple:
    """The manifest owns the run's bbox as well as its release."""
    cell = rm.load_manifest(MANIFEST)["cells"]["ovt-s3"]
    value = next(f for f in cell["flags"] if f.startswith("--bbox="))
    return tuple(map(float, value.split("=", 1)[1].split(",")))


def read_tsv(path: Path) -> list[list[str]]:
    """Rows of a tab-separated file, `#` comment lines skipped."""
    return [ln.split("\t") for ln in path.read_text().splitlines() if ln and not ln.startswith("#")]


def read_parts(path: Path = PARTS_TSV) -> list[dict]:
    """The pinned parts: url, bytes, etag, rows, bbox (its row groups' union)."""
    return [{"url": r[0], "bytes": int(r[1]), "etag": r[2], "rows": int(r[3]),
             "bbox": tuple(map(float, r[4].split(",")))} for r in read_tsv(path)]


def intersects(a: tuple, b: tuple) -> bool:
    """Two xmin,ymin,xmax,ymax boxes share at least a point."""
    return a[0] <= b[2] and b[0] <= a[2] and a[1] <= b[3] and b[1] <= a[3]


def parts_touching(bbox: tuple, parts: list[dict]) -> list[dict]:
    """The parts whose bbox intersects `bbox`: the ones a run may read."""
    return [p for p in parts if intersects(p["bbox"], bbox)]


def bbox_arg(bbox: tuple) -> str:
    """xmin,ymin,xmax,ymax as the CLIs take it."""
    return ",".join(f"{v:g}" for v in bbox)


def https_url(url: str) -> str:
    parsed = urllib.parse.urlsplit(url)
    return f"https://{parsed.netloc}.s3.us-west-2.amazonaws.com{urllib.parse.quote(parsed.path, safe='/=')}"


def head_object(url: str) -> tuple[int, str]:
    with urllib.request.urlopen(urllib.request.Request(https_url(url), method="HEAD"), timeout=60) as response:
        if response.status != 200:
            raise OSError(f"HEAD failed: {url}: {response.status}")
        return int(response.headers["Content-Length"]), response.headers["ETag"].strip('"')


def check_pins() -> None:
    try:
        parts = read_parts(PARTS_TSV)
        for part in (parts[0], parts[-1]):
            if head_object(part["url"]) != (part["bytes"], part["etag"]):
                raise ValueError(f"input changed: {part['url']}")
    except (OSError, ValueError, KeyError, IndexError) as exc:
        raise ValueError(f"Overture pin check failed: {exc}; run `showcase.py pin <release>`") from exc
    print(f"Overture pins match {RELEASE}")


def list_objects(prefix: str) -> list[dict]:
    """Anonymous ListObjectsV2 with pagination, with no AWS credentials."""
    parsed = urllib.parse.urlsplit(prefix)
    records = []
    token = None
    ns = {"s": "http://s3.amazonaws.com/doc/2006-03-01/"}
    while True:
        query = {"list-type": "2", "prefix": parsed.path.lstrip("/")}
        if token:
            query["continuation-token"] = token
        url = f"https://{parsed.netloc}.s3.us-west-2.amazonaws.com/?{urllib.parse.urlencode(query)}"
        with urllib.request.urlopen(url, timeout=60) as response:
            listing = ET.fromstring(response.read())
        for entry in listing.findall("s:Contents", ns):
            key = entry.findtext("s:Key", namespaces=ns)
            if key.endswith(".parquet"):
                records.append({"url": f"s3://{parsed.netloc}/{key}",
                                "bytes": int(entry.findtext("s:Size", namespaces=ns)),
                                "etag": entry.findtext("s:ETag", namespaces=ns).strip('"')})
        token = listing.findtext("s:NextContinuationToken", namespaces=ns)
        if not token:
            return sorted(records, key=lambda r: r["url"])


def footer_groups(url: str) -> list[tuple]:
    """Existing footer reader: bbox, rows, compressed bytes per row group."""
    import pyarrow.fs as pafs
    import pyarrow.parquet as pq

    s3 = pafs.S3FileSystem(anonymous=True, region="us-west-2")
    md = pq.ParquetFile(url.removeprefix("s3://"), filesystem=s3).metadata
    names = [md.schema.column(i).path for i in range(md.num_columns)]
    ix = [names.index(f"bbox.{k}") for k in ("xmin", "ymin", "xmax", "ymax")]
    groups = []
    for g in range(md.num_row_groups):
        rg = md.row_group(g)
        st = [rg.column(i).statistics for i in ix]
        box = (st[0].min, st[1].min, st[2].max, st[3].max)
        groups.append((box, rg.num_rows,
                       sum(rg.column(i).total_compressed_size for i in range(rg.num_columns))))
    return groups


def pin_release(release: str) -> None:
    """Refresh keys, object facts, footer facts, checksum, and manifest."""
    from concurrent.futures import ThreadPoolExecutor

    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", release):
        raise ValueError(f"invalid Overture release: {release!r}")
    prefix = re.sub(r"/release/[^/]+/", f"/release/{release}/", PREFIX)
    parts = list_objects(prefix)
    if not parts:
        raise ValueError(f"no parts in {prefix}")
    with ThreadPoolExecutor() as pool:
        groups = list(pool.map(footer_groups, [p["url"] for p in parts]))
    lines = [f"# Overture buildings {release}: pinned keys, bytes, ETag, rows, bbox.",
             "# url<TAB>bytes<TAB>etag<TAB>rows<TAB>bbox"]
    for part, rows in zip(parts, groups):
        box = (min(r[0][0] for r in rows), min(r[0][1] for r in rows),
               max(r[0][2] for r in rows), max(r[0][3] for r in rows))
        lines.append("\t".join([part["url"], str(part["bytes"]), part["etag"],
                                str(sum(r[1] for r in rows)), bbox_arg(box)]))
    tsv = MANIFEST.parent / f"overture-buildings-{release}.tsv"
    tsv.write_text("\n".join(lines) + "\n")
    digest = rm.sha256_file(tsv)
    tsv.with_suffix(".tsv.sha256").write_text(f"{digest}  {tsv.name}\n")
    text = MANIFEST.read_text()
    text = text.replace(PREFIX, prefix)
    text = re.sub(r'path = "[^"]+\.tsv"',
                  lambda m: f'path = "{Path(_manifest["inputs"]["overture-listing"]["path"]).parent / tsv.name}"', text)
    text = re.sub(r'sha256 = "[a-f0-9]+"', f'sha256 = "{digest}"', text)
    text = re.sub(r"# Region `africa`: bbox .*",
                  f"# Region `africa`: bbox {bbox_arg(region_bbox())} of Overture buildings {release}.", text)
    MANIFEST.write_text(text)
    print(f"Pinned {release}: {tsv}")


def bytes_fetched(profile: Path) -> int | None:
    """`pass0.bytes_fetched` of a TYLERTOO_PROFILE_JSON file (remote input)."""
    if not profile.is_file():
        return None
    for ln in profile.read_text().splitlines():
        if ln.strip():
            p0 = json.loads(ln).get("pass0")
            if p0:
                return p0.get("bytes_fetched")
    return None


def scan_counts(log: Path) -> dict:
    """tylertoo's own log lines: features kept after --bbox (pass 1 reads
    whole row groups, so its row count includes rows outside the bbox) and
    the row groups the bbox filter read."""
    text = log.read_text(errors="replace") if log.is_file() else ""
    out = {}
    if m := re.search(r"scan complete: (\d+) feature\(s\) from (\d+) row\(s\)", text):
        out["features"], out["rows_read"] = int(m[1]), int(m[2])
    if m := re.search(r"bbox filter: reading (\d+)/(\d+) input row groups", text):
        out["row_groups_read"], out["row_groups_total"] = int(m[1]), int(m[2])
    return out


def ok_runs(results: Path, cell: str, binary: str) -> list[dict]:
    """The ok run rows of one cell and binary in a run_manifest results dir,
    with each run's fetched bytes and scan counts."""
    rows = [json.loads(p.read_text()) for p in sorted((results / cell).glob(f"{binary}-r*.json"))]
    for r in rows:
        stem = results / cell / f"{binary}-r{r['round']}"
        r["bytes_fetched"] = bytes_fetched(stem.with_suffix(".profile.jsonl"))
        r["scan"] = scan_counts(stem.with_suffix(".log"))
    return [r for r in rows if rm.run_status(r) == "ok"]


def median(xs: list[float]) -> float | None:
    """The median, or None for no values."""
    return statistics.median(xs) if xs else None


def recorded_argv(argv: list[str]) -> list[str]:
    """The command as the page shows it: no host paths for the binary or the
    output archive, which use the manifest's `tylertoo` and `{out}` forms."""
    if not argv:
        return []
    return ["tylertoo", *(f"{{out}}/{a.rsplit('/', 1)[1]}" if a.startswith("/") and a.endswith(".pmtiles")
                          else a for a in argv[1:])]


def tool_row(label: str, runs: list[dict], note: str | None = None, archive: dict | None = None) -> dict:
    """One streaming result row, plus its recorded command and environment."""
    # The higher of the sampler's peak and GNU time's exact high-water mark:
    # the sampler can miss a spike between ticks.
    peak = [max(rm.true_peak(r) or 0, r["peak_rss_kib"] / 1024) * (1 << 20) for r in runs]
    cores = [r["cpu_s"] / r["wall_s"] for r in runs if r.get("cpu_s") and r["wall_s"]]
    fetched = [r["bytes_fetched"] for r in runs if r.get("bytes_fetched")]
    sizes = sorted({r["output"]["bytes"] for r in runs if r["output"]["bytes"]})
    shas = sorted({r["output"]["sha256"] for r in runs if r["output"]["sha256"]})
    # A kept archive describes this row only if the timed runs produced it.
    if archive and archive["sha256"] not in shas:
        archive = None
    return {"argv": recorded_argv(runs[0].get("argv", [])) if runs else [],
            "env": runs[0].get("env", runs[0].get("binary_env", {})) if runs else {},
            "label": label, "runs_wall_s": [r["wall_s"] for r in runs],
            "peak_rss_bytes": int(max(peak)) if peak else None,
            "effective_cores": round(median(cores), 1) if cores else None,
            "archive_bytes": sizes[-1] if sizes else None,
            "archive_sha256": shas,
            "peak_disk_bytes": max(r["scratch_peak_mib"] for r in runs) * (1 << 20) if runs else None,
            "fetched_bytes": int(median(fetched)) if fetched else None,
            "archive": archive, "note": note}


def archive_facts(path: Path, tylertoo: str, cap: int = 500_000) -> dict:
    """sha256 and `tylertoo stats --json` of a kept archive: per-zoom tile
    counts and sizes, and tiles over the cap among the 1000 largest."""
    stats = json.loads(subprocess.check_output(
        [tylertoo, "stats", "--json", "--largest", "1000", str(path)], text=True))
    return {"bytes": path.stat().st_size, "sha256": rm.sha256_file(path), "stats": stats,
            "tiles": sum(z["tile_count"] for z in stats["per_zoom"]),
            "max_tile_bytes": max((z["max_bytes"] for z in stats["per_zoom"]), default=None),
            "tiles_over_cap_in_1000_largest": sum(t["bytes"] > cap for t in stats["largest"])}


def run_provenance(run_dir: Path) -> dict:
    """Use only facts saved with the timed runs; unknown fields remain null."""
    saved = json.loads((run_dir / "provenance.json").read_text())
    return {**dict.fromkeys(rm.PROVENANCE_KEYS), **saved}


def bbox_row_group_bytes(bbox: tuple, parts: list[dict]) -> tuple[int, int]:
    from concurrent.futures import ThreadPoolExecutor

    with ThreadPoolExecutor() as pool:
        groups = pool.map(footer_groups, [p["url"] for p in parts_touching(bbox, parts)])
        touched = [g for part in groups for g in part if intersects(g[0], bbox)]
    return sum(g[2] for g in touched), sum(g[1] for g in touched)


def build_stream_doc(region: str, bbox: tuple, s3: list[dict], parts: list[dict], prov: dict,
                     in_scope: tuple[int, int], archive: dict | None = None) -> dict:
    """The record of a streaming-only run: tylertoo reads the parts in place
    and no other tool takes part (tippecanoe cannot read s3:// GeoParquet)."""
    touched = parts_touching(bbox, parts)
    scan = next((r["scan"] for r in s3 if r.get("scan", {}).get("features")), {})
    # pass-1 rows include rows outside the bbox, so they are only the fallback
    feats = scan.get("features") or next(
        (c["pass1_rows"] for r in s3 if (c := r["profile"].get("convert") or {}).get("pass1_rows")), None)
    inputs = [{"name": f"overture-buildings-{RELEASE}", "release": RELEASE, "path": PREFIX,
               "bbox": list(bbox), "sha256": rm.sha256_file(PARTS_TSV), "sha256_of": PARTS_TSV.name,
               "parts": [{"url": p["url"], "etag": p["etag"], "bytes": p["bytes"]} for p in touched]}]
    # No other tool ran: drop the showcase's tippecanoe and gpio pins.
    return {"provenance": {**prov, "tool_versions": {}, "inputs": inputs},
            "dataset": f"Overture buildings {RELEASE}, {region} (bbox {bbox_arg(bbox)})",
            "features": feats, "region": region, "bbox": list(bbox),
            "release_bytes": sum(p["bytes"] for p in parts), "parts_touched": len(touched),
            "parts_bytes": sum(p["bytes"] for p in touched),
            "in_scope_bytes": in_scope[0], "in_scope_rows": in_scope[1],
            "rows_read": scan.get("rows_read"), "row_groups_read": scan.get("row_groups_read"),
            "row_groups_total": scan.get("row_groups_total"),
            "release": RELEASE,
            "rows": [tool_row("tylertoo, s3:// in place", s3, note="reads the parts in place; no extract",
                              archive=archive)]}


def history_entry(previous: dict) -> dict:
    """Snapshot the previous default run without relabeling its provenance."""
    prov = {**dict.fromkeys(rm.PROVENANCE_KEYS), **previous["provenance"]}
    if not prov.get("git_describe") and prov.get("commit"):
        prov["git_describe"] = rm.git_describe(prov["commit"])
    row = next(r for r in previous["rows"] if "bounded" not in r.get("label", ""))
    match = re.search(r"\d{4}-\d{2}-\d{2}\.\d+", previous.get("dataset", ""))
    release = previous.get("release") or (match[0] if match else None)
    return {"provenance": prov, "release": release, "wall_s": statistics.median(row["runs_wall_s"])}


def collect_stream(opts) -> None:
    data = Path(opts.data).expanduser().resolve()
    bbox = region_bbox()
    run_dir = data / "results" / opts.name
    runs = ok_runs(run_dir, "ovt-s3", opts.binary)
    if not runs:
        raise ValueError(f"no successful Africa run in {run_dir}")
    archive = archive_facts(Path(opts.archive), opts.tylertoo) if opts.archive else None
    parts = read_parts()
    doc = build_stream_doc(opts.region, bbox, runs, parts,
                           run_provenance(run_dir), bbox_row_group_bytes(bbox, parts), archive)
    if opts.history_from:
        previous = json.loads(Path(opts.history_from).read_text())
        doc["history"] = [*previous.get("history", []), history_entry(previous)]
    out = Path(opts.out or RESULTS / f"overture-buildings-{opts.region}.json")
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(doc, indent=2) + "\n")
    print(f"wrote {out}")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("check-pins")
    pin = sub.add_parser("pin")
    pin.add_argument("release")
    collect = sub.add_parser("collect-stream")
    collect.add_argument("region", choices=["africa"])
    collect.add_argument("--data", required=True)
    collect.add_argument("--name", default="africa-s3")
    collect.add_argument("--binary", default="head")
    collect.add_argument("--tylertoo", default=str(ROOT / "target/release/tylertoo"))
    collect.add_argument("--archive")
    collect.add_argument("--history-from")
    collect.add_argument("--out")
    args = ap.parse_args(argv)
    try:
        if args.cmd == "check-pins":
            check_pins()
        elif args.cmd == "pin":
            pin_release(args.release)
        else:
            collect_stream(args)
    except (OSError, ValueError) as exc:
        ap.exit(1, f"{exc}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
