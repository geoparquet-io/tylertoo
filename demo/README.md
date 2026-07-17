# tylertoo demo — Brazil field boundaries

Full-country **GeoParquet → PMTiles** on real cloud-native data
([Fields of The World](https://fieldsofthe.world/) field boundaries for Brazil,
55,499,514 features), read directly from GeoParquet on Source Cooperative and
tiled to a complete z1–14 pyramid — a path no established tool offers, because
tippecanoe does not read GeoParquet.

- **Live demo:** [docs/demo.md](../docs/demo.md) → renders on the docs site at
  `/demo/`, with the interactive map at [docs/demo/viewer.html](../docs/demo/viewer.html).
- **Numbers + methodology:** [RESULTS.md](./RESULTS.md).

## Hosting

The Brazil field-boundaries PMTiles is published to **Source Cooperative** and
rendered live:

```
s3://us-west-2.opendata.source.coop/nlebovits/gpq-tiles-demo/
  brazil-field-boundaries.pmtiles     # rendered in the viewer (8.4 GiB)
```

Public URL (serves HTTP **Range** + **CORS**, which PMTiles requires):

```
https://s3.us-west-2.amazonaws.com/us-west-2.opendata.source.coop/nlebovits/gpq-tiles-demo/brazil-field-boundaries.pmtiles
```

The viewer (`docs/demo/viewer.html`) renders the boundaries over
[CARTO Dark Matter](https://carto.com/basemaps/) and defaults to that URL;
override with `?pmtiles=<url>`. The MVT source-layer is `overview` (tylertoo's
default layer name).

### Re-uploading

```bash
export AWS_PROFILE=source-coop
aws s3 cp brazil-field-boundaries.pmtiles \
  s3://us-west-2.opendata.source.coop/nlebovits/gpq-tiles-demo/ \
  --content-type application/octet-stream
```

Any bucket works as long as it serves **Range + CORS** — without both, PMTiles
fails silently (blank map, no error). Source Cooperative's `opendata` bucket does
by default. To preview against local files, Python's `http.server` (3.7+) honors
Range:

```bash
cd docs/demo
python3 -m http.server 8080
# open http://localhost:8080/viewer.html?pmtiles=http://localhost:8080/local.pmtiles
```

## Source data

The 27 per-state input files are listed in `brazil-manifest.txt` (one Source
Cooperative URL per line) and read over HTTPS by `tylertoo overview --files-from`.
A full-country run fetches each object once (~1×, spilled locally so later passes
never re-hit the network); a regional `--bbox` fetches only the covering row
groups.

## Notes

Zoom range is `z1–14`; z0 is empty (no features are visible at that scale) and
is omitted from the pyramid. Clipping and simplification differ from tippecanoe
by design — see [`context/ARCHITECTURE.md`](../context/ARCHITECTURE.md). This
demonstrates a *pipeline and its output*, not byte-identical tiling.
