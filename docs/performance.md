# Performance

gpq-tiles converts GeoParquet into multi-resolution overviews and
PMTiles. This page characterises how that conversion scales on real
multi-GB inputs, measured on current `main`.

- Raw numbers and methodology history:
  [`benchmarks/overview/PROFILE.md`](https://github.com/geoparquet-io/gpq-tiles/blob/main/benchmarks/overview/PROFILE.md).
- Head-to-head against the GeoJSON → tippecanoe pipeline: see the
  [Demo](demo.md) (Germany buildings, 59 M features: **13 m 11 s** vs the
  incumbent pipeline failing at 57 m 30 s). We don't chase feature-level
  tippecanoe parity — the demo settles the comparison; this page is about
  how gpq-tiles scales on its own terms.

## How these were measured

- **Machine:** 16-core AMD Ryzen 7040 laptop, 54 GiB RAM, release build.
- **Inputs:** gpio-optimised (`gpio sort hilbert --add-bbox`) Overture and
  fieldmaps extracts. Optimising the input is not optional — Hilbert
  ordering and right-sized row groups are what make the numbers below (and
  the remote pruning further down) possible.
- **Convert:** `gpq-tiles overview <in> <out> --min-zoom 0 --max-zoom 14`,
  default knobs, wall/RSS/CPU from `/usr/bin/time -v`.
- Timing is measured reading **local** files. Reading over the network
  adds re-fetch cost that inflates wall time — see
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

−29 % peak RSS for a 2-second wall difference. Use `bounded` when RSS
headroom matters more than the last few percent of throughput.

## Remote selective read

`overview` reads remote GeoParquet (`s3://`, `https://`, `http://`,
`gs://`) with HTTP byte-range requests. With `--bbox`, only the row groups
overlapping the box are ever fetched — nothing else is downloaded. See
[Remote Reads](remote-reads.md) for the full walkthrough.

Extracting a Berlin slice from the 6.99 GB Germany buildings file:

```bash
gpq-tiles overview \
  s3://…/germany-buildings.parquet \
  berlin.parquet \
  --bbox 13.35,52.48,13.47,52.55 \
  --min-zoom 0 --max-zoom 14
```

fetches **52 MB — 0.74 % of the file — in 0.83 s**. No other tiler does
selective remote extraction like this.

The caveat is the mirror image: a **full-file** remote convert re-fetches
the input, because the streaming engine re-reads it per level and large
selections overflow the 256 MiB chunk cache. Measured bytes moved ÷ file
size, full file vs bbox:

| call | bytes moved | ÷ file |
|---|---:|---:|
| points-nyc (full) | 30.8 MB | 1.00× |
| germany-segments (full) | 6.82 GB | 2.67× |
| germany-buildings (full) | 18.1 GB | 2.59× |
| fieldmaps-adm4 (full) | 278 GB | 96× |
| germany-buildings `--bbox` | 52 MB | **0.0074×** |

So `--bbox` extraction is the remote superpower; for a *whole-file*
remote conversion, download first (`aws s3 cp` + local convert) or expect
the re-fetch. Reducing that full-file cost is tracked in
[#261](https://github.com/geoparquet-io/gpq-tiles/issues/261).
