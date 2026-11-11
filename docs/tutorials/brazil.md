--8<-- "examples/brazil-fields/README.md:build"

### The result

The archive from step 4, served from this site. A5 density cells, colored by
`point_count`, cover z0–z7; zoom past z8 to see the field polygons. Click a
feature to see its properties.

<div id="brazil-map" class="tutorial-map" role="region" aria-label="Map of brazil.pmtiles"></div>
<p class="tutorial-map-caption">Basemap © <a href="https://openfreemap.org">OpenFreeMap</a>, © <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors. Fields © <a href="https://fieldsofthe.world/">Fields of the World</a>.</p>

<link rel="stylesheet" href="https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.css">
<script src="https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.js"></script>
<script src="https://unpkg.com/pmtiles@3.2.1/dist/pmtiles.js"></script>
<script src="../../assets/tutorials/brazil-map.js"></script>

--8<-- "examples/brazil-fields/README.md:run"
