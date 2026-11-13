# Visual gate: does tylertoo draw the source as well as tippecanoe?

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
```

## What it measures

Both tools tile the same source: tylertoo with `tiles --preset tippecanoe`,
tippecanoe 2.79.0 with its defaults and `--drop-densest-as-needed`. Each
archive is then measured **against the source**, on the same tiles, and the
gate compares the two errors:

    d = tylertoo's error - tippecanoe's error

The two archives are never compared with each other. Where tylertoo keeps
features tippecanoe drops, a direct comparison would call that a difference;
against the source it is a smaller error.

Every error is a share from 0 (the tile draws what the source has) upward:

| Source | Error | Meaning |
|---|---|---|
| Polygons | `missing` | Source pixels the tile does not draw, over source pixels |
| Polygons | `added` | Pixels the tile draws where the source has nothing, over source pixels |
| Polygons with `color_attr` | `attr_mismatch` | Of the pixels both draw, the share where the tile's feature has another attribute value than the source's |
| Lines | `src_uncovered` | Source length further than 1 pixel from any drawn line, over source length |
| Lines | `drawn_stray` | Drawn length further than 1 pixel from any source line, over source length |
| Points | `count_error` | On a 16×16 grid per tile, the summed difference of drawn and source counts, over the source count |
| Points, `--cluster` | `point_count_sum` | Sum of `point_count` over a zoom. Must equal the source count exactly |
| Polygons, per zoom | `edge_gap` | Source-covered pixels in a 2-pixel band along a shared tile edge that neither tile draws |
| Polygons, per zoom | `excess_change` | Attribute changes across a tile edge beyond those across a line inside the tile |

A pixel is 1/256 of a tile side. Decoded tile geometry is rasterized
directly: no map renderer, so no anti-aliasing and no style.

A defect is local, and a mean hides it. Per zoom the gate takes the worst
tiles of each error: the `max`, `p99` and `p95` over the sampled tiles, as
`missing_max`, `missing_p99`, and so on.

Limits:

- A tile is scored only when the source fills at least 1% of it (16 pixels
  of line, 8 points). A sliver scores near 1 whatever the tool does.
- Up to 100 tiles per zoom are sampled, from the tiles that hold source
  vertices, with a fixed seed. A defect outside the sample is not seen;
  `--cap` raises it.
- Lines and points have no seam metric. A line cut at a tile edge shows as
  `src_uncovered` in that tile.
- At coarse zooms both tools thin points heavily and `count_error` is near 1
  for both. It says little there.
- The source must be lon/lat and of one geometry kind.

## Reading a breach

```
madagascar-adm4: 43 cells, 1 outside their band
  BREACH   madagascar-adm4 z8 missing_max: tylertoo 0.2581 tippecanoe 0.0024 d +0.2557 band [-0.0101, 0.0099] tile 8/158/144 png /home/you/bench-data/visual/png/madagascar-adm4_z8_missing_max.png
re-run only these: uv run benchmarks/visual/gate.py --datasets madagascar-adm4 --zooms 8
```

In tile 8/158/144 tylertoo fails to draw 25.8% of the source's area and
tippecanoe 0.2%. The PNG shows that tile three times: the source, tylertoo,
tippecanoe. Grey is drawn, red is source the tile does not draw, blue is
drawn where the source has nothing, orange is drawn with another feature's
attribute.

`IMPROVED` instead of `BREACH` means `d` is below its band: tylertoo got
better. That also exits 1, until the band is tightened.

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
- On a breach, open the PNG first, then fix the code. Re-run with the
  printed command; it covers only the failing cells.
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
