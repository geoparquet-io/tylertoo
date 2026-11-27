--8<-- "examples/brazil-fields/README.md:build"

## Inspect the result

The map uses `brazil.pmtiles` from step 1. The cell colors show how many
fields each cell contains. Zoom to level 8 to see individual field
boundaries, then click a field to view its properties.

<div id="brazil-map" class="tutorial-map" role="region" aria-label="Map of brazil.pmtiles"></div>
<p class="tutorial-map-caption">Base map © <a href="https://openfreemap.org">OpenFreeMap</a>, © <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors. Fields © <a href="https://fieldsofthe.world/">Fields of the World</a>.</p>

<link rel="stylesheet" href="https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.css">
<script src="https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.js"></script>
<script src="https://unpkg.com/pmtiles@3.2.1/dist/pmtiles.js"></script>
<script src="../assets/tutorials/brazil-map.js"></script>

To inspect either archive locally, open it in the [PMTiles viewer](https://pmtiles.io/).

--8<-- "examples/brazil-fields/README.md:run"
