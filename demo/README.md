# tylertoo demo — Brazil 2025 field predictions

**GeoParquet → PMTiles from a 630 GiB global collection**: the
[Fields of The World](https://fieldsofthe.world/) predictions for Brazil's
2025 growing season, carved out of the global `results/` collection on Source
Cooperative *during the tiling read* — `--filter "label = 'field' AND
time >= '2025-01-01'"` + `--bbox` — and tiled to a complete z0–14 pyramid
with **dots at z0–5 and polygons progressively taking over from z6** in a
single archive — tuned (`--representation "0-5:point,6-14:geom" --collapse
--polygon-visibility 0 --simplify-factor 4`) so each field stays a visible dot
until it is a ≥1 px polygon and **nothing disappears mid-zoom**. No established
tool takes this path:
tippecanoe does not read GeoParquet, let alone filter a remote collection
while tiling it.

- **Live demo:** [docs/demo.md](../docs/demo.md) → renders on the docs site at
  `/demo/`, with the interactive map at [docs/demo/viewer.html](../docs/demo/viewer.html).
- **Numbers + methodology + findings:** [RESULTS.md](./RESULTS.md).

## Hosting

The PMTiles is published to **Source Cooperative** and rendered live:

```
s3://us-west-2.opendata.source.coop/nlebovits/gpq-tiles-demo/
  brazil-2025-fields.pmtiles          # rendered in the viewer
  brazil-field-boundaries.pmtiles     # previous demo (kept)
```

Public URL (serves HTTP **Range** + **CORS**, which PMTiles requires):

```
https://s3.us-west-2.amazonaws.com/us-west-2.opendata.source.coop/nlebovits/gpq-tiles-demo/brazil-2025-fields-v2.pmtiles
```

The viewer (`docs/demo/viewer.html`) renders over
[CARTO Dark Matter](https://carto.com/basemaps/) and defaults to that URL;
override with `?pmtiles=<url>`. The MVT source-layer is `overview` (tylertoo's
default). z0–5 tiles carry **points**, and z6–13 carry a mix of points (fields
still too small to draw) and polygons, so the style needs a `circle` layer next
to the polygon `fill`/`line` layers — fill-only styles render the low zooms
blank.

### Re-uploading

```bash
export AWS_PROFILE=source-coop
aws s3 cp brazil-2025-fields.pmtiles \
  s3://us-west-2.opendata.source.coop/nlebovits/gpq-tiles-demo/ \
  --content-type application/octet-stream
```

Any bucket works as long as it serves **Range + CORS** — without both, PMTiles
fails silently (blank map, no error). To preview against local files, Python's
`http.server` (3.7+) honors Range:

```bash
cd docs/demo
python3 -m http.server 8080
# open http://localhost:8080/viewer.html?pmtiles=http://localhost:8080/local.pmtiles
```

## Source data

The input is the FTW predictions `results/` collection: 1,000 Spark part
files, 629.6 GiB, 8.2 billion rows, mixing three prediction classes and two
vintages (2024/2025) with global coverage. `brazil-2025-manifest.txt` lists
the 52 part files whose footers intersect the Brazil bbox (one URL per line,
read over HTTPS by `tylertoo overview --files-from`); the run fetched those
once (~1×, spilled locally) and never touched the other 948.

## Notes

The z0–5 point band bypasses the polygon visibility gate (a dot is always
visible), so even z0 renders — a polygon-only pyramid's z0 was empty. From z6
`--collapse --polygon-visibility 0` keeps every field represented (as a dot
until it is a ≥1 px polygon), so the visible feature count rises at every zoom
instead of dropping at the point→polygon boundary. Clipping and simplification
differ from tippecanoe by design — see
[`context/ARCHITECTURE.md`](../context/ARCHITECTURE.md). This demonstrates a
*pipeline and its output*, not byte-identical tiling.
