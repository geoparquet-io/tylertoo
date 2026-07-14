# gpq-tiles demo — Germany buildings

End-to-end **GeoParquet → PMTiles** on real Overture data (Germany buildings,
59,032,924 features), head-to-head against the geoparquet-io recommended pipeline
(GeoJSON → tippecanoe).

- **Live demo:** [docs/demo.md](../docs/demo.md) → renders on the docs site at
  `/demo/`, with the interactive map at [docs/demo/viewer.html](../docs/demo/viewer.html).
- **Numbers + methodology:** [RESULTS.md](./RESULTS.md).

## Hosting

The tuned Germany-buildings PMTiles is published to **Source Cooperative** and
rendered live (tuned only — the default archive differs only marginally):

```
s3://us-west-2.opendata.source.coop/nlebovits/gpq-tiles-demo/
  germany-buildings-gpq-tuned.pmtiles     # rendered in the viewer
  germany-buildings-gpq-default.pmtiles   # benchmark artifact
```

Public URL (serves HTTP **Range** + **CORS**, which PMTiles requires):

```
https://s3.us-west-2.amazonaws.com/us-west-2.opendata.source.coop/nlebovits/gpq-tiles-demo/germany-buildings-gpq-tuned.pmtiles
```

The viewer (`docs/demo/viewer.html`) renders the buildings over
[CARTO Dark Matter](https://carto.com/basemaps/) and defaults to that URL; override
with `?pmtiles=<url>`.

### Re-uploading

```bash
export AWS_PROFILE=source-coop
aws s3 cp germany-buildings-gpq-tuned.pmtiles \
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

## Fairness notes

Zoom range (`z0–14`), layer name (`buildings`), and the 500K per-tile cap are
matched across pipelines. Clipping and simplification differ from tippecanoe by
design — see [`context/ARCHITECTURE.md`](../context/ARCHITECTURE.md). This
compares *pipelines and their output*, not byte-identical tiling.
