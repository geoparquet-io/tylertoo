# Coming from tippecanoe

## Compare the maps

Pick a dataset. The top map shows the archive tylertoo built, and the bottom map shows the one tippecanoe built from the same input. The two maps pan and zoom together.

<!-- bench:comparison-pmtiles/compare-maps -->
<label for="compare-dataset">Dataset</label>
<select id="compare-dataset">
<option value="points-nyc-medium" data-tylertoo="https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/points-nyc-medium/tylertoo.pmtiles" data-tippecanoe="https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/points-nyc-medium/tippecanoe.pmtiles" data-layer="points-nyc-medium" data-geometry="point">New York City places (point)</option>
<option value="polygons-portland-medium" data-tylertoo="https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/polygons-portland-medium/tylertoo.pmtiles" data-tippecanoe="https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/polygons-portland-medium/tippecanoe.pmtiles" data-layer="polygons-portland-medium" data-geometry="polygon">Portland buildings (polygon)</option>
<option value="lines-oregon-large" data-tylertoo="https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/lines-oregon-large/tylertoo.pmtiles" data-tippecanoe="https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/lines-oregon-large/tippecanoe.pmtiles" data-layer="lines-oregon-large" data-geometry="line">Oregon roads (line)</option>
<option value="polygons-ftw-moldova-large" data-tylertoo="https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/polygons-ftw-moldova-large/tylertoo.pmtiles" data-tippecanoe="https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/polygons-ftw-moldova-large/tippecanoe.pmtiles" data-layer="polygons-ftw-moldova-large" data-geometry="polygon">Moldova parcels (polygon, dense)</option>
</select>

<div class="compare-maps">
<div id="compare-tylertoo" class="tutorial-map" role="region" aria-label="tylertoo output"><span class="compare-badge">tylertoo</span></div>
<div id="compare-tippecanoe" class="tutorial-map" role="region" aria-label="tippecanoe output"><span class="compare-badge">tippecanoe</span></div>
</div>
<p class="tutorial-map-caption">Base map: <a href="https://openfreemap.org/">OpenFreeMap</a>, © <a href="https://www.openmaptiles.org/">OpenMapTiles</a>, © <a href="https://www.openstreetmap.org/copyright">OpenStreetMap contributors</a>.</p>

<details markdown><summary>Download the archives</summary>

| Dataset | tylertoo archive | tippecanoe archive |
|---|---|---|
| New York City places (point) | [tylertoo](https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/points-nyc-medium/tylertoo.pmtiles) | [tippecanoe](https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/points-nyc-medium/tippecanoe.pmtiles) |
| Portland buildings (polygon) | [tylertoo](https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/polygons-portland-medium/tylertoo.pmtiles) | [tippecanoe](https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/polygons-portland-medium/tippecanoe.pmtiles) |
| Oregon roads (line) | [tylertoo](https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/lines-oregon-large/tylertoo.pmtiles) | [tippecanoe](https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/lines-oregon-large/tippecanoe.pmtiles) |
| Moldova parcels (polygon, dense) | [tylertoo](https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/polygons-ftw-moldova-large/tylertoo.pmtiles) | [tippecanoe](https://data.source.coop/nlebovits/tylertoo-benchmarks/v0.8.0-1639-gf0e7d29e/polygons-ftw-moldova-large/tippecanoe.pmtiles) |

_Run: v0.9.0, commit f0e7d29e, tippecanoe v2.82.0, gpio 1.7.0, allocator: mimalloc, aarch64, CPU 0xd40, 64 CPUs, 247 GiB RAM, 2026-11-27._

</details>
<!-- /bench -->

<link href="https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.css" rel="stylesheet">
<script src="https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.js"></script>
<script src="https://unpkg.com/pmtiles@3.2.1/dist/pmtiles.js"></script>
<script src="../assets/guides/compare-map.js"></script>

## Flag mapping

tylertoo reads GeoParquet. For other formats, use `gpio` first; see [preparing input](../tutorials/madagascar.md).

Use these tylertoo options for familiar tippecanoe flags:

--8<-- "crates/cli/docs/tippecanoe-tables.md:flags"

## The tippecanoe preset

`--preset tippecanoe` (Python `preset="tippecanoe"`) replaces defaults where tylertoo has a matching option. Options you pass still win.
`tiles` and `convert()` take the whole preset; `overview` takes the convert rows, and `export-pmtiles` takes the export rows. `pyramid` takes no preset.
You cannot combine the preset with `--verbatim`. The algorithms still differ, so matching settings do not guarantee matching tiles.

--8<-- "crates/cli/docs/tippecanoe-tables.md:preset"

--8<-- "examples/tippecanoe/README.md"
