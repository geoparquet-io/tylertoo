# Performance

tylertoo converts GeoParquet into multi-resolution overviews and
PMTiles. This page characterises how that conversion scales on real
multi-GB inputs, measured on current `main`.

- Raw numbers and methodology history:
  [`benchmarks/overview/PROFILE.md`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/overview/PROFILE.md).
- Head-to-head against the GeoJSON → tippecanoe pipeline: see the
  [Demo](demo.md) (Germany buildings, 59 M features: **13 m 11 s** vs the
  incumbent pipeline failing at 57 m 30 s). We don't chase feature-level
  tippecanoe parity — the demo settles the comparison; this page is about
  how tylertoo scales on its own terms.

## How these were measured

- **Machine:** 16-core AMD Ryzen 7040 laptop, 54 GiB RAM, release build.
- **Inputs:** gpio-optimised (`gpio sort hilbert --add-bbox`) Overture and
  fieldmaps extracts. Optimising the input is not optional — Hilbert
  ordering and right-sized row groups are what make the numbers below (and
  the remote pruning further down) possible.
- **Convert:** `tylertoo overview <in> <out> --min-zoom 0 --max-zoom 14`,
  default knobs, wall/RSS/CPU from `/usr/bin/time -v`.
- Timing is measured reading **local** files. Reading over the network
  moves ≈1× the object's bytes (the disk spill keeps re-reads local) but
  still adds fetch latency and a second-pass disk read — see
  [Remote selective read](#remote-selective-read) below.

## Convert throughput by geometry type

| input | geometry | features | wall | peak RSS | CPU |
|---|---|---:|---:|---:|---:|
| points-nyc-medium | points | 458 k | 1.7 s | 0.3 GiB | 131 % |
| germany-segments | lines | 19.2 M | 2:02 | 11.9 GiB | 138 % |
| germany-buildings | polygons | 59.0 M | 4:00 | 14.8 GiB | 138 % |
| fieldmaps-adm4 | admin polygons | 364 k | 1:54 | 4.7 GiB | 487 % |

Points are effectively free (no simplification, trivial clipping).
Lines and polygons scale with feature and vertex count. fieldmaps-adm4 is
the vertex-heavy stress case — only 364 k features but ~261 M vertices —
and it drives ~5 cores (487 % CPU) where the large-feature layers still
sit near ~1.4 cores, so there is feature-parallel headroom yet to claim.

## Materialisation mode

`--mode duplicating` (default) writes each level self-contained;
`--mode partitioning` writes each feature once. On vertex-heavy data the
difference is large:

| input | mode | wall | peak RSS |
|---|---|---:|---:|
| fieldmaps-adm4 | duplicating | 1:54 | 4.7 GiB |
| fieldmaps-adm4 | partitioning | **0:54** | 5.5 GiB |

Partitioning roughly halves convert time here because it materialises the
heavy geometry once instead of per level. Output is byte-identical in the
sense that matters for rendering; pick partitioning for large
vertex-dense polygon layers.

## Memory profile

`--profile bounded` caps in-flight memory for the streaming convert path.
It changes nothing about the output and, in practice, costs nothing in
wall time:

| input | profile | wall | peak RSS |
|---|---|---:|---:|
| germany-buildings | speed | 3:58 | 14.8 GiB |
| germany-buildings | bounded | 4:00 | **10.5 GiB** |

−29 % peak RSS for a 2-second wall difference. The default `--profile
auto` estimates the buffered output from the feature and level counts and
picks `bounded` automatically once it would exceed a fraction of available
RAM (set `TYLERTOO_AUTO_MEM_LIMIT_BYTES` to model a smaller box); pass
`--profile bounded` to force it, or `speed` to force RAM buffering.

## Remote selective read

`overview` reads remote GeoParquet (`s3://`, `https://`, `http://`,
`gs://`) with HTTP byte-range requests. With `--bbox`, only the row groups
overlapping the box are ever fetched — nothing else is downloaded. See
[Remote Reads](remote-reads.md) for the full walkthrough.

Extracting a Berlin slice from the 6.99 GB Germany buildings file:

```bash
tylertoo overview \
  s3://…/germany-buildings.parquet \
  berlin.parquet \
  --bbox 13.35,52.48,13.47,52.55 \
  --min-zoom 0 --max-zoom 14
```

fetches **52 MB — 0.74 % of the file — in 0.83 s**. No other tiler does
selective remote extraction like this.

A **full-file** remote convert re-reads the input several times (the
assign pass, the coarse-level write pass, and a finest-level canonical
re-read). Historically each pass re-fetched the input over the network,
so bytes moved ÷ file size climbed well above 1×. Two fixes closed that
gap:

| stage | fieldmaps-adm4 (2.90 GB, z0–14) | cause / fix |
|---|---:|---|
| before #261 | **96×** | oversized geometry chunk evicted-on-insert, re-fetched per page |
| after [#261](https://github.com/geoparquet-io/tylertoo/issues/261) | **3.0×** | chunk cache sized to the largest row group — no re-fetch *within* a pass, but each pass still re-fetched |
| after [#219](https://github.com/geoparquet-io/tylertoo/issues/219) | **≈1×** | fetched chunks spilled to local disk; later passes drain from disk, so each byte crosses the network once |

The ≈1× bound is verified deterministically by an input-level regression
test (`multi_pass_reads_move_object_once`); a full-corpus wall-clock
re-measurement (segments/buildings over `http://`) is the pending
follow-up. `--bbox` extraction remains the remote superpower — pruned row
groups are never fetched at all (germany-buildings `--bbox`: 52 MB,
**0.0074×** of the file), and the disk spill only ever holds the chunks
that would have been fetched once anyway.

The residual full-file cost is now local, not network: the passes still
re-*read* the spilled bytes from disk (seek latency) and re-*decode*
them. For latency-sensitive whole-file conversions, downloading first
(`aws s3 cp` + local convert) still avoids the second-pass disk read; the
disk spill lives under `TMPDIR` by default, and `--spill-dir <path>`
(Python: `spill_dir=`,
[#272](https://github.com/geoparquet-io/tylertoo/issues/272)) moves it
to a volume of your choosing — fast local disk, not a small tmpfs, when
converting large remote files.

The converter makes this actionable at runtime. A whole-file remote
convert of an object ≥ 1 GiB logs a one-line warning
([#267](https://github.com/geoparquet-io/tylertoo/issues/267)) steering
you to `--bbox` or a download-first workflow and reminding you to place
the spill on fast disk. And because the spill grows to ≈1× the *touched*
input bytes — a number known exactly from the parquet footer once the
`--bbox` row-group selection is made — the converter also preflights the
spill volume ([#272](https://github.com/geoparquet-io/tylertoo/issues/272)):
if the projected spill (plus a 5% margin) exceeds the free space where
the spill will live, it warns up front, naming the directory and the
shortfall, instead of letting the spill silently degrade to network
re-fetch when the volume fills mid-convert. `--bbox` extracts stay quiet
in both cases — they already fetch (and spill) only a fraction of the
object.
