# Diving Deeper

Once the [Getting Started](../getting-started.md) tutorial has given you a
working mental model, these topics go one level down — each is self-contained,
so read the ones that match what you are doing.

- [Preparing input for tiling](preparing-input.md) — the gpio-optimized
  GeoParquet contract (WGS84, Hilbert sorting, row-group sizing) and why each
  part matters for speed and memory.
- [Working with the overview file](overview-file.md) — the `geo:overviews`
  format: level bands, metadata, and how the file stays valid, SQL-queryable
  GeoParquet you can inspect with DuckDB.
- [Tuning what appears at each zoom](tuning-zoom.md) — the quality ladder as one
  mental model: class ranking, visibility gates, the density budget, clustering,
  line coalescing, and simplification.
- [Tiling remote and multi-file inputs](remote-and-multi-file.md) — `s3://` /
  `https://` / `gs://` byte-range reads, `--bbox` row-group pushdown,
  `--files-from` multi-partition input, and `--filter` attribute pushdown.
- [Keeping memory bounded](bounded-memory.md) — the streaming model
  (memory ≈ O(row group)), the two-pass structure, and what to do when a file is
  too big for RAM.
- [How tylertoo relates to tippecanoe](tippecanoe.md) — a factual capability
  comparison: what each tool does, what only tylertoo does, and which
  quality-ladder concepts are shared.
- [Decoding PMTiles back to GeoParquet](decoding.md) — `tylertoo decode`, its
  zoom/layer selectors, and why a tiles-to-GeoParquet round trip is lossy.

For the exhaustive list of every flag and option, see the
[Reference](../reference/cli.md).
