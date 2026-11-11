// The Brazil tutorial's map: step 1's brazil.pmtiles (all of Brazil, 5.9 GB)
// on Source Cooperative, read by HTTP range requests, over an OpenFreeMap
// basemap. One layer, "fields": A5 cells (with point_count) at z0-7, fields
// from z8.
(function () {
  const el = document.getElementById("brazil-map");
  if (!el || !window.maplibregl || !window.pmtiles) return;
  const protocol = new pmtiles.Protocol();
  maplibregl.addProtocol("pmtiles", protocol.tile);
  const archive = "https://data.source.coop/nlebovits/tylertoo-brazil-demo/brazil.pmtiles";
  const map = new maplibregl.Map({
    container: el,
    style: "https://tiles.openfreemap.org/styles/positron",
    center: [-53, -14],
    zoom: 3.5,
    cooperativeGestures: true,
  });
  map.addControl(new maplibregl.NavigationControl({ showCompass: false }));
  map.on("load", () => {
    map.addSource("brazil", { type: "vector", url: "pmtiles://" + archive });
    // Cell counts span 1 to over a million across z0-7: color by their log.
    map.addLayer({
      id: "density", type: "fill", source: "brazil", "source-layer": "fields", maxzoom: 8,
      filter: ["has", "point_count"],
      paint: {
        "fill-color": ["interpolate", ["linear"], ["ln", ["get", "point_count"]],
          0, "#fde0c5", 4, "#f59e5b", 8, "#e85017", 12, "#7a1f05"],
        "fill-opacity": 0.7,
      },
    });
    map.addLayer({
      id: "density-outline", type: "line", source: "brazil", "source-layer": "fields", maxzoom: 8,
      filter: ["has", "point_count"],
      paint: { "line-color": "#7a1f05", "line-width": 0.5, "line-opacity": 0.5 },
    });
    map.addLayer({
      id: "fields", type: "fill", source: "brazil", "source-layer": "fields", minzoom: 8,
      paint: { "fill-color": "#05221d", "fill-opacity": 0.15 },
    });
    map.addLayer({
      id: "fields-outline", type: "line", source: "brazil", "source-layer": "fields", minzoom: 8,
      paint: { "line-color": "#05221d", "line-width": 1 },
    });
  });
  map.on("click", (e) => {
    const f = map.queryRenderedFeatures(e.point, { layers: ["density", "fields"] })[0];
    if (!f) return;
    const rows = Object.entries(f.properties)
      .map(([k, v]) => `<tr><th>${k}</th><td>${typeof v === "number" ? v.toLocaleString() : v}</td></tr>`)
      .join("");
    new maplibregl.Popup().setLngLat(e.lngLat)
      .setHTML(`<strong>${"point_count" in f.properties ? "A5 cell" : "field"}</strong><table>${rows}</table>`).addTo(map);
  });
})();
