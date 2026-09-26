# End-to-end benchmark: GeoParquet → PMTiles, tylertoo vs pinned tippecanoe

"Faster than tippecanoe for typical GeoParquet workflows" is the project's
stated goal. This harness is the evidence for (or against) it. Measured
numbers live in [RESULTS.md](RESULTS.md); this file is the method, including
every place the comparison is not apples to apples.

Issue: [#447](https://github.com/geoparquet-io/tylertoo/issues/447).
No CI wiring lives here — benchmarking in CI is [#448](https://github.com/geoparquet-io/tylertoo/issues/448)'s lane.

## Quick start

```bash
./benchmarks/e2e/setup_tippecanoe.sh        # builds the pinned tippecanoe
cargo build --release -p tylertoo
python3 benchmarks/e2e/run_e2e.py --repeat 3 --geojson --quality-matched
python3 benchmarks/e2e/format_results.py    # print RESULTS.md's tables
```

`run_e2e.py` is standard library only. The optional visual-parity renderer
needs plotting packages and runs under `uv`:

```bash
uv run --with pmtiles --with mapbox-vector-tile --with matplotlib \
    python3 benchmarks/e2e/render_parity.py A.pmtiles B.pmtiles \
        --zooms 6,9,12 --out /tmp/parity
```

## The four numbers, and why there are four

tippecanoe cannot read GeoParquet, and at defaults the two tools do not emit
the same map. Every "tylertoo vs tippecanoe" figure is therefore two decisions
— what to put on tippecanoe's side of the scale, and whether to hold output
constant — and those decisions are where this kind of benchmark usually lies.
The harness makes all four measurements and prints all four:

| | pipeline | what it answers |
|---|---|---|
| **A** | tylertoo, defaults: `parquet → pmtiles` | what a tylertoo user pays out of the box |
| **A′** | tylertoo, quality-matched: `parquet → pmtiles` with the thinning ladder off | what tylertoo pays to emit the *same features* tippecanoe does |
| **B** | tippecanoe e2e: `parquet → fgb → pmtiles` | what a tippecanoe user with a GeoParquet file pays |
| **C** | tippecanoe tile-only: `fgb → pmtiles` | is the *tiler* faster, given a free head start |

**A′ exists because A is not an output-parity comparison.** tylertoo's density
budget thins features at coarse and mid zooms. tippecanoe drops nothing but
points, so on a contiguous polygon coverage it emits every feature at every
zoom. On the madagascar fixture that is 865 of 17,465 admin polygons at z8
against tippecanoe's 17,465 — a visibly different map, not a subtler one, and
quoting A against it would be quoting a number for less work.

A′ runs `--verbatim --simplify-factor 1.0`, which switches the whole
thinning/visibility ladder off while keeping simplification (verbatim alone
would drop that too, and tippecanoe does simplify). It lands within a handful
of tiles of tippecanoe's per-zoom counts, which is how we know the comparison
is like-for-like. **A′ vs C is the number to quote to a skeptic.**

**A vs B is the product comparison.** Reading GeoParquet natively is the
product's thesis, not a benchmarking trick: a user who starts from GeoParquet
really does have to run that conversion, and it really does cost wall time and
disk. Claiming that number is fair. Claiming it *without* saying it includes a
format conversion is not.

**A vs C is the deliberately unflattering comparison.** It hands tippecanoe a
pre-converted FlatGeobuf for free and asks whether tylertoo's tiling engine is
faster on its own. Report both or report neither. RESULTS.md reports both, and
the conversion's share of B is printed as its own column so a reader can do
the subtraction themselves.

## The baseline is pinned

`setup_tippecanoe.sh` clones **felt/tippecanoe at tag `2.79.0`**, verifies the
commit is `68ab8dcc229f95b8b25877697d5e8d66783af503`, builds it into
`.tools/`, and writes `tippecanoe.lock.json`. `run_e2e.py` refuses to run
against a binary whose `--version` does not match the lockfile (override with
`TIPPECANOE_ALLOW_MISMATCH=1`, and then say so in the write-up).

The **pin itself** lives in `setup_tippecanoe.sh` (`TIPPECANOE_TAG` /
`TIPPECANOE_SHA`); the lockfile is a machine-local artifact naming a built
binary, so it is gitignored. The tag and SHA that were actually measured are
copied into `results.json`, which *is* committed — the record of a run travels
with the run, not with the toolchain.

Why not just use the one on `$PATH`: on the machine these numbers were taken
on, `which tippecanoe` resolved to a **conda-forge build of v2.31.0** — the
old mapbox lineage — while Homebrew had **2.79.0** installed but shadowed.
Two tippecanoes on one machine, years of tiling changes apart. A harness that
does not name its baseline's version is not reproducible.

Homebrew's `tippecanoe` formula currently *is* 2.79.0, so
`--tippecanoe /opt/homebrew/bin/tippecanoe` is a valid shortcut today. It will
drift the next time the formula moves; the source pin will not.

## Settings parity

tylertoo's `tiles` defaults, mapped onto the nearest tippecanoe flag. The
harness passes exactly this and records the full argv of both tools in
`results.json`.

| knob | tylertoo (default) | tippecanoe flag passed | status |
|---|---|---|---|
| zoom range | `--min-zoom 0 --max-zoom 14` | `-Z 0 -z 14` | **matched** |
| layer name | `--layer-name <id>` | `-l <id>` | **matched** |
| tile buffer | `--tile-buffer 8` | `-b 8` (tippecanoe default is 5) | **matched**, by moving tippecanoe to tylertoo's value |
| per-tile byte cap | `--max-tile-size 500K` | `--maximum-tile-bytes 500000` | **matched** (same number is also tippecanoe's default) |
| oversize-tile valve | single non-iterative drop pass (always on) | `--drop-fraction-as-needed` | **approximated** — see asymmetries |
| drop rate | `--drop-rate 1.65` | *not passed* (tippecanoe default 2.5) | **not matched on purpose** — see asymmetries |
| gamma | `--drop-gamma 1.5` | *not passed* (tippecanoe default 0) | **not matched on purpose** |
| simplification | `--simplify-factor 1.0` (× level GSD, metres) | *not passed* (tippecanoe `-S` default 1, tile units) | **not matched on purpose** |

The buffer is moved on tippecanoe's side rather than tylertoo's because the
buffer changes output *size*: letting tippecanoe write thinner tiles from a
smaller buffer would flatter tylertoo on the archive-size column.

`-r`, `-g` and `-S` are deliberately left at each tool's own default. tylertoo's
`--drop-rate 1.65` is anchored on the full canonical feature count (every
feature exists at the finest level); tippecanoe's `-r 2.5` is anchored on a
per-tile basezoom count. Passing `-r 1.65` to tippecanoe would look like
parity and would in fact be cargo-culting a numeral across two different
denominators. Same for gamma (per-super-cell in tylertoo, per-tile in
tippecanoe) and simplification (metres-of-GSD vs tile units). These show up as
output-shape differences instead, and RESULTS.md reports them.

## Asymmetries that flags cannot close

These are real and are not tuned away in either direction.

| # | asymmetry | which way it cuts |
|---|---|---|
| 0 | **Defaults do not produce the same map.** tylertoo's density budget thins features at coarse and mid zooms; tippecanoe drops only points. For a contiguous polygon coverage tylertoo's default output has visible holes where tippecanoe's is complete. | **favours tylertoo heavily in A.** This is why A′ exists; quote A′ vs C for output parity. |
| 1 | **Input format.** tippecanoe cannot read GeoParquet; a conversion step is unavoidable. | favours tylertoo in A-vs-B. Neutralised in A-vs-C. |
| 2 | **Generalization model.** tylertoo generalizes in world space, once per level, into a reusable overview file; tippecanoe generalizes in tile space, per tile, at encode time. They are not the same algorithm producing the same tiles faster — they are different algorithms with comparable *output intent*. Even in A′ the simplification is metres-of-GSD vs tile units, so vertex counts differ. | unquantifiable; this is why the visual-parity renderer exists |
| 3 | **Oversize-tile valve.** tylertoo does one non-iterative drop pass; `--drop-fraction-as-needed` re-encodes the tile in a loop until it fits. | favours tylertoo on time, tippecanoe on tile-fill quality |
| 4 | **Per-tile feature limit.** tippecanoe caps a tile at 200,000 features by default (`-pf` disables); tylertoo has no such cap. Left at tippecanoe's default — adding `-pf` would be a non-default setting. | favours tippecanoe on time at very dense zooms |
| 5 | **Coarse-zoom emptiness.** With everything at defaults, tylertoo's visibility gate can generalize a small-extent dataset to nothing at z0–z4 and emit no tile there; tippecanoe emits a (nearly empty) tile at every zoom in range. Visible in the per-zoom counts. | favours tylertoo on tile count and archive size |
| 6 | **tylertoo writes an intermediate.** `tiles` materialises the overview GeoParquet to a temp file and exports from it. That write is inside tylertoo's measured wall (it is not free, and it is not hidden), but it also means tylertoo's number includes producing an artifact tippecanoe never produces. | favours tippecanoe on time; favours tylertoo on what you get |
| 7 | **Converter start-up.** `gpio` is a Python CLI (~0.3 s interpreter start-up); `ogr2ogr` is a C binary. On a 1,000-feature fixture the start-up *is* the conversion time. The harness times **both** converters and the e2e number uses the **faster**. A converter that fails on a dataset is recorded and skipped, not fatal — `gpio convert flatgeobuf` currently fails on the madagascar fixture (`NULL geometry not supported with spatial index`, although the parquet has no null, empty or invalid geometry), so `ogr2ogr` supplied that input. | neutralised |
| 8 | **Thread counts.** Both tools use all cores; neither is pinned. tylertoo's export wave width is `auto` (memory-preflighted). Numbers are machine-specific, which is why `results.json` records CPU, core count and RAM. | neutral, but not portable |
| 9 | **Warm cache.** A discarded warm-up run precedes the timed repeats for every stage, on both sides. No cold-cache numbers are published — macOS cannot drop caches without root. | neutral |

## Metrics

Recorded per dataset, per stage, in `results.json`:

- **wall** — `time.perf_counter()` around the process, median of `--repeat`
  (default 3) timed runs after one discarded warm-up; `min`, `max` and every
  individual run are kept.
- **peak RSS** — from `/usr/bin/time` (`-l` on Darwin, `-v` on GNU), i.e. the
  kernel's `ru_maxrss`, not sampled. For the two-process tippecanoe pipeline
  the reported figure is the **larger of the two stages**, not their sum,
  because they never run concurrently.
- **output archive size** — bytes on disk.
- **per-zoom tile counts and stored bytes** — from `tylertoo stats --json`,
  which reads PMTiles directory entries only. The *same code* reads both
  archives, so this column is not self-graded.
- **tylertoo phase split** — `tiles --report` gives `convert` (parquet read +
  generalization ladder) and `export` (MVT encode + archive write) wall
  separately, plus row-groups read/total and feature counts.
- **visual parity** — `render_parity.py`, separately, by eye.

## Datasets

The committed set is the in-repo real-data fixture set, so `git clone && run`
reproduces the table:

| id | features | size | class |
|---|---|---|---|
| `madagascar-adm4` | 17,465 | 28 MB | MultiPolygon (admin boundaries) |
| `open-buildings` | 1,000 | 143 KB | Polygon |
| `road-detections` | 1,000 | 90 KB | LineString |

Fetch them with `gh release download fixtures-v1 --dir tests/fixtures/realdata/`.

**Two of the three are ~1,000 features and are not the headline.** At that
size both tools are dominated by process start-up and pyramid scaffolding and
the ratio says nothing about tiling throughput. They are kept as the
geometry-class smoke test. RESULTS.md says so in the table itself.

For a dataset that actually exercises the engines, pass your own:

```bash
python3 run_e2e.py --dataset planet-buildings=/data/buildings.parquet \
                   --only planet-buildings --max-zoom 13 --repeat 3
```

Everything — flags, parity rules, metrics — is identical for a supplied
dataset. That is the path for cluster-scale runs.

## Files

| file | what it is |
|---|---|
| `setup_tippecanoe.sh` | clones + verifies + builds the pinned tippecanoe; writes `tippecanoe.lock.json` |
| `run_e2e.py` | the harness (stdlib only); writes `results.json` and a markdown table |
| `format_results.py` | prints RESULTS.md's tables from `results.json`, so no number is retyped |
| `render_parity.py` | side-by-side PNG renders of two archives at chosen zooms, with distinct-feature counts |
| `results.json` | the recorded run behind RESULTS.md (machine, argv, every repeat) |
| `RESULTS.md` | measured numbers, machine, caveats |
| `tippecanoe.lock.json` | local resolution of the pin (gitignored; written by setup) |
| `.tools/` | built tippecanoe (gitignored) |

## Useful flags

| flag | what for |
|---|---|
| `--quality-matched` | adds the A′ run (thinning ladder off) — the output-parity comparison |
| `--geojson` | adds the ldGeoJSON + `-P` leg, evidence for "fgb is tippecanoe's best input" |
| `--repeat N` | timed runs per measurement (median); default 3 |
| `--dataset ID=PATH` | register your own GeoParquet; cluster-scale path |
| `--tippecanoe-extra="…"` | probe a single tippecanoe flag's cost, recorded in `results.json` (use `=`, or argparse eats a leading `-`) |
| `--work DIR --keep` | keep intermediates and archives, e.g. to feed `render_parity.py` |
| `--tippecanoe PATH` | use a different tippecanoe (version must match the lockfile) |
