# gpq-tiles demo — Germany buildings, head-to-head

End-to-end **GeoParquet → PMTiles** on real Overture data (Germany buildings,
59,032,924 features), comparing the native gpq-tiles path against the current
geoparquet-io recommended pipeline (GeoJSON → tippecanoe).

- `compare.html` — side-by-side swipe viewer (MapLibre + `pmtiles://`).
- `RESULTS.md` — the head-to-head metrics table (measured).

## The comparison

| pipeline | command |
|---|---|
| incumbent | `gpio convert geojson data.parquet \| tippecanoe -P -Z0 -z14 -l buildings -o out.pmtiles` |
| gpq-tiles (default) | `gpq-tiles tiles data.parquet out.pmtiles --min-zoom 0 --max-zoom 14 --layer-name buildings --max-tile-size 500K` |
| gpq-tiles (tuned) | above `+ --polygon-visibility 2.0 --collapse --drop-rate 1.3 --profile bounded` |

Both start from the **same** gpio-optimized GeoParquet; the incumbent round-trips
through a GeoJSON stream, gpq-tiles reads the Parquet natively with no
intermediate.

## Hosting the viewer (required for it to work)

`compare.html` reads tiles with HTTP **Range** requests straight from a bucket.
The bucket **must** serve:

1. **Range requests** (`Accept-Ranges: bytes`, `206 Partial Content`), and
2. **CORS** (`Access-Control-Allow-Origin`, and `Access-Control-Expose-Headers:
   Content-Range` so the client can read the ranged response).

Without **both**, PMTiles fails silently — a blank map, no error. This is the
single most common demo-hosting mistake.

Upload the three `.pmtiles` files, then point the viewer at them one of two ways:

- **Edit the file:** set the URLs in the `DEFAULT_SOURCES` block near the top of
  `compare.html`.
- **Pass at runtime** (no edit):
  ```
  compare.html?gpq_default=https://bucket/germany-buildings-gpq-default.pmtiles
              &gpq_tuned=https://bucket/germany-buildings-gpq-tuned.pmtiles
              &tippecanoe=https://bucket/germany-buildings-tippecanoe.pmtiles
  ```
  (URLs may omit the `pmtiles://` prefix — it's added automatically.)

The two dropdowns choose which outputs sit on the left/right of the swipe;
default-vs-tuned and gpq-vs-tippecanoe are both one selection away. View-preset
buttons jump to Germany / Berlin / Hamburg.

### Preview locally before hosting

Python's `http.server` (3.7+) honors Range, which is enough to smoke-test the
viewer against local files:

```bash
cd demo
cp /path/to/*.pmtiles .
python3 -m http.server 8080
# open http://localhost:8080/compare.html?gpq_default=http://localhost:8080/germany-buildings-gpq-default.pmtiles&...
```

(Local `http.server` does **not** send CORS headers, but same-origin local files
don't need them — this only validates rendering, not the CORS side of hosting.)

### pmtiles.io fallback

For a zero-setup share, drop a hosted `.pmtiles` URL into
<https://pmtiles.io/> — it renders a single archive with a layer inspector. The
swipe comparison, though, needs `compare.html`.

## Fallback / fairness notes

- Zoom range, layer name (`buildings`), and the 500K per-tile size cap are matched
  across all three pipelines. tippecanoe's default max-tile is 500K; gpq-tiles is
  pinned to match via `--max-tile-size 500K`.
- Clipping and simplification differ from tippecanoe by design — see
  `context/ARCHITECTURE.md` for the documented divergences. The comparison is of
  *pipelines and their output*, not a claim of byte-identical tiling.
