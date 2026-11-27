# Coming from tippecanoe

tylertoo reads GeoParquet. For other formats, use `gpio` first; see [preparing input](../tutorials/madagascar.md).

## Flag mapping

Use these tylertoo options for familiar tippecanoe flags:

--8<-- "crates/cli/docs/tippecanoe-tables.md:flags"

## The tippecanoe preset

`--preset tippecanoe` (Python `preset="tippecanoe"`) replaces defaults where tylertoo has a matching option. Options you pass still win.
`tiles` and `convert()` take the whole preset; `overview` takes the convert rows, and `export-pmtiles` takes the export rows. `pyramid` takes no preset.
You cannot combine the preset with `--verbatim`. The algorithms still differ, so matching settings do not guarantee matching tiles.

--8<-- "crates/cli/docs/tippecanoe-tables.md:preset"

--8<-- "examples/tippecanoe/README.md"

<!--
To enable: after the comparison manifest and renderer land, remove this outer comment, change bench-pending to bench, and restore the region comment delimiters.
## Output side by side

Compare each tool's benchmark settings on the corpus datasets.
The archives come from the release run.

<!- bench-pending:comparison-pmtiles/compare-maps ->
<!- /bench ->

<div class="compare-maps">
  <div id="compare-tylertoo" class="tutorial-map" role="region" aria-label="tylertoo output"></div>
  <div id="compare-tippecanoe" class="tutorial-map" role="region" aria-label="tippecanoe output"></div>
</div>
<p class="tutorial-map-caption">Basemap: <a href="https://openfreemap.org/">OpenFreeMap</a>, © <a href="https://www.openmaptiles.org/">OpenMapTiles</a>, © <a href="https://www.openstreetmap.org/copyright">OpenStreetMap contributors</a>.</p>
<link href="https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.css" rel="stylesheet">
<script src="https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.js"></script>
<script src="https://unpkg.com/pmtiles@3.2.1/dist/pmtiles.js"></script>
<script src="../assets/guides/compare-map.js"></script>
-->
