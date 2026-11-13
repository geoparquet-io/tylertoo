# Visual gate: do tylertoo's tiles look like tippecanoe's?

`compare_tippecanoe.py` gates counts and bytes. Those do not see a moved
border, a hole at a seam, or a merge that carries the wrong attribute. This
gate does (#869). It is a development tool: run it after a change to
generalization, the size valve, or the tippecanoe preset. CI runs only its
unit tests.

```bash
cargo build --release --package tylertoo
benchmarks/e2e/setup_tippecanoe.sh          # once: the pinned tippecanoe
gh release download fixtures-v1 --dir tests/fixtures/realdata/ --clobber   # once

uv run benchmarks/visual/gate.py            # every dataset, exit 0 or 1
uv run benchmarks/visual/gate.py --datasets open-buildings --zooms 14
uv run benchmarks/visual/gate.py --dataset mine=/data/mine.parquet
uv run benchmarks/visual/gate.py --self-test
uv run benchmarks/visual/gate.py --tylertoo-flags='--preset tippecanoe --simplify-factor 4'
```

A run of all six fixtures takes about 2.5 minutes the first time and about
1 minute after that. `--help` lists every flag; `--work` moves the cache and
the PNGs, `--tolerances` reads another band file. Several runs can share one
`--work`: each keeps its tylertoo archives in a directory of its own. Exit 2
means the run could not be made (a missing or failing tool, an unknown
dataset, an unusable source, or a source with no tile to score at the
zooms asked for), not a breach.

## Distance to tippecanoe

Every run compares tylertoo's tile with tippecanoe's tile of the same
z/x/y directly, with no source, on the sampled tiles. 0 means the two tiles
draw the same:

| Source | Cell | Meaning |
|---|---|---|
| Polygons | `pair_iou_loss` | 1 − IoU of the area the two tools draw |
| Polygons with a `color_attr` | `pair_attr` | Share of the area both draw where the attribute differs |
| Lines | `pair_apart` | Share of the two tools' line length more than one pixel from the other's |
| Points | `pair_count` | Share of points not matched per cell of a 16×16 grid; 1 means no cell in common |

Each has `_max`, `_p99`, `_p95` and `_mean` cells per zoom, with bands like
the others. The mean pools the zoom's tiles, each counted by how much is
drawn in it, so a tile holding ten pixels of data does not weigh as much as
a full one. The max and percentiles are per tile, over the tiles where the
two tools draw at least 1% of the tile between them (16 pixels of line, 8
points): with less, one pixel decides the share. The direct comparison takes every sampled tile, also those the
comparison against the source skips for holding too little of it (small
polygons at low zooms).

Two more cells per zoom say how much each tool keeps in the sampled tiles:
`features_ratio` and `bytes_ratio`, tylertoo's features and stored tile
bytes over tippecanoe's. 1 means the same amount; above 1 tylertoo keeps
more. Use them with the distance when tuning what tylertoo drops. After each dataset the run prints one line, the mean over its
zooms of the mean tile:

```
madagascar-adm4: 83 cells, 0 outside their band
  distance to tippecanoe (mean tile, 0 = same): pair_attr 0.0036, pair_iou_loss 0.0082  [IoU 0.9918]
  kept, tylertoo over tippecanoe (1 = same): features x1.02, tile bytes x1.00
```

`--json` holds the same numbers under `distance`. To look for settings that
come closer to tippecanoe, pass them with `--tylertoo-flags` and compare
that line or that JSON key between runs: lower is closer. The flags replace
`--preset tippecanoe`, so repeat it when the settings build on the preset.
With `--tylertoo-flags` the bands are not applied (they were measured for
the preset); the run reports and exits 0.

`--cap N` sets how many tiles are sampled per zoom and dataset (default
100, with a fixed seed); `--cap 0` takes every tile that holds source data.
A larger sample sees more and takes longer. The bands were measured at the
default, so with another `--cap` the run also only reports.

Measured for the preset, over the scored zooms:

| Fixture | Distance | Features kept | Tile bytes |
|---|---|---|---|
| madagascar-adm4 | IoU 0.9918, `pair_attr` 0.0036 | ×1.02 | ×1.00 |
| open-buildings | IoU 0.9487, `pair_attr` 0.0001 | ×0.90 | ×0.95 |
| ftw-moldova-z10 | IoU 0.9930 | ×0.92 | ×0.90 |
| fieldmaps-boundaries | IoU 0.9362, `pair_attr` 0.0000 | ×0.98 | ×0.97 |
| road-detections | `pair_apart` 0.0001 | ×0.89 | ×0.98 |
| vd-points | `pair_count` 0.5915 | ×1.13 | ×1.04 |

The points are far apart at z8 to z12 (`pair_count` 1.0 on the worst tile):
both tools thin the points there, and they keep different ones. At z14 both
keep every point and the distance is 0.

These fixtures are small. On dense data the tools differ much more where
tippecanoe drops features: 1.36M buildings overlap at IoU 0.02 at z10 and
0.49 at z12, because tylertoo keeps buildings tippecanoe drops. Run a
larger source with `--dataset ID=PATH` to see that range.

## What it measures against the source

Both tools tile the same source: tylertoo with `tiles --preset tippecanoe`,
tippecanoe 2.79.0 with its defaults and `--drop-densest-as-needed`. Each
archive is then measured **against the source**, on the same tiles, and the
gate compares the two errors:

    d = tylertoo's error - tippecanoe's error

The two archives are never compared with each other. Where tylertoo keeps
features tippecanoe drops, a direct comparison would call that a difference;
against the source it is a smaller error.

Every per-tile error is a share from 0 (the tile draws what the source has)
upward. `excess_change` is a difference of two shares and can be negative;
`point_count_sum` is a count:

| Source | Error | Meaning |
|---|---|---|
| Polygons | `missing` | Source pixels the tile does not draw, over source pixels |
| Polygons | `added` | Pixels the tile draws where the source has nothing, over source pixels |
| Polygons with `color_attr` | `attr_mismatch` | Of the pixels both draw, the share where the tile's feature has another attribute value than the source's |
| Lines | `src_uncovered` | Source length further than 1 pixel from any drawn line, over source length |
| Lines | `drawn_stray` | Drawn length further than 1 pixel from any source line, over source length |
| Points | `count_error` | On a 16×16 grid per tile, the summed difference of drawn and source counts, over the source count |
| Points | `point_count_sum` | Sum of `point_count` over a zoom, in a second tylertoo archive made with `tiles --cluster`. Must equal the source count exactly |
| Polygons, per zoom | `edge_gap` | Source-covered pixels in a 2-pixel band along a shared tile edge that neither tile draws |
| Polygons, per zoom | `excess_change` | Attribute changes across a tile edge beyond those across a line inside the tile |

A pixel is 1/256 of a tile side. Decoded tile geometry is rasterized
directly: no map renderer, so no anti-aliasing and no style.

A defect is local, and a mean hides it. Per zoom the gate takes the worst
tiles of each error: the `max`, `p99` and `p95` over the sampled tiles, as
`missing_max`, `missing_p99`, and so on. One dataset, zoom and such a name
is a cell, and each measured cell has its own band. A cell without one (a
`--dataset ID=PATH` source, or a zoom outside the dataset's list) takes
`default_band`: it fails when tylertoo is more than 0.02 worse than
tippecanoe, and has no lower side.

Limits:

- The cover floor: a tile is scored only when the source fills at least 1%
  of it (16 pixels of line, 8 points). A sliver scores near 1 whatever the
  tool does.
- Up to 100 tiles per zoom are sampled, from the tiles that hold source
  vertices, with a fixed seed. A defect outside the sample is not seen;
  `--cap` widens it. A tile wholly inside one polygon holds no vertex, and
  is sampled only when it holds that polygon's interior point. None of the fixtures has such a tile at its scored zooms
  (fieldmaps-boundaries would at z12).
- `excess_change` needs a `color_attr`; a dataset without one has only
  `edge_gap` at its seams.
- Lines and points have no seam metric. A line cut at a tile edge shows as
  `src_uncovered` in that tile.
- At coarse zooms both tools thin points heavily and `count_error` is near 1
  for both. It says little there.
- The source must be lon/lat and of one geometry kind.

## Reading a breach

```
madagascar-adm4: 43 cells, 1 outside their band
  BREACH   madagascar-adm4 z8 missing_max: tylertoo 0.2581 tippecanoe 0.0024 d +0.2557 band [-0.0101, 0.0099] tile 8/158/144 png /home/you/bench-data/visual/png/madagascar-adm4_z8_missing_8-158-144.png
re-run only this: uv run benchmarks/visual/gate.py --datasets madagascar-adm4 --zooms 8
```

tylertoo's worst tile at z8 fails to draw 25.8% of the source's area, and
tippecanoe's worst tile 0.2%. Each value is that tool's own max (or p99, or
p95) over the sampled tiles, so the two can come from different tiles. The
tile named, 8/158/144 (zoom/x/y, as in a tile URL), is tylertoo's single
worst for that error, also on the p99 and p95 lines. The PNG shows that tile
three times, left to right: the source, tylertoo, tippecanoe, at twice the size
the gate measures. Grey is drawn, white is empty, red is source the tile
does not draw, blue is drawn where the source has nothing, orange is drawn
with another feature's attribute.

A band has two sides, and both exit 1:

- `BREACH`: `d` is above its band. tylertoo draws worse than it did when
  the band was measured. `d` can still be negative: better than tippecanoe,
  and worse than before.
- `IMPROVED`: `d` is below its band. tylertoo draws better than it did, and
  the band is stale until it is moved.

A tile and a PNG come with a breach of a per-tile error. `edge_gap`,
`excess_change` and `point_count_sum` are per zoom and have neither. For the
first two, re-run the zoom with `--verbose` and read the per-tile cells. A
`point_count_sum` line gives the sum and the source count; make its archive
again with `tylertoo tiles --preset tippecanoe --cluster` on the source. The
PNG is written anew on every run, and runs that share `--work` share the
PNG folder. A band
whose metric has no value any more (a tool draws nothing it can compare)
also fails, and says so.

The tippecanoe archive and its scores are cached under `--work`
(`~/bench-data/visual`), keyed by the source, the tippecanoe version and
flags, and `metrics.py`. A re-run after a tylertoo change tiles and scores
tylertoo only.

## Bands

`visual_tolerances.toml` holds one band on `d` per dataset, zoom and metric,
with the two measured values in its comment. Its header states the rule. To
change a band, run the gate, copy the measured values from the output into
the comment, and set the band to `d ± 0.01`. A band is never widened to make
a run pass.

## The gate's own test

`--self-test` takes tylertoo's Madagascar archive and makes six copies
(`mutate.py`): one unchanged, and one each with 5% of the polygons removed,
every geometry shifted 2 pixels, every geometry simplified at 4 pixels, one
tile left out, and attributes swapped between neighbours. The unchanged copy
must pass and each of the others must breach a band. It takes about five
minutes.

## Handing it to an agent

The gate is built so that a task can be given as "make this change and keep
the visual gate green":

- Green is `uv run benchmarks/visual/gate.py` exiting 0.
- On a breach, open the PNG first (when the line names one), then fix the
  code. Re-run with the printed command; it covers that dataset at its
  failing zooms.
- Do not widen a band, raise the cover floor, or lower `--cap` to pass.
  If a band has to move because the change is meant to draw differently,
  say so in the PR with the measurement.
- `IMPROVED` is the one case where editing the tolerance file is the fix.

## Files

| File | Role |
|---|---|
| `metrics.py` | The measurements. `benchmarks/coalesce/tile_metrics.py` and `render_coverage.py` read, decode and rasterize through it too |
| `gate.py` | Tiles both tools, scores both, applies the bands, writes the PNGs |
| `mutate.py` | The defective copies for `--self-test` |
| `visual_tolerances.toml` | The bands, each dataset's scored zooms and compared attribute |
| `test_metrics.py`, `test_gate.py` | Unit tests on synthetic tiles; the command is in `test_metrics.py` |
| `test_properties.py` | Hypothesis properties: each error against an answer computed another way (exact areas, dropped shares), the band rule, and the unchanged copy of `mutate.py` |
