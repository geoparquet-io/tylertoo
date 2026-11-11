// The Brazil tutorial's map: brazil.pmtiles (step 1) over an OpenFreeMap basemap.
// One layer, "fields": A5 cells (with point_count) at z0-7, fields from z8.
(function () {
  const el = document.getElementById("brazil-map");
  if (!el || !window.maplibregl || !window.pmtiles) return;
  const protocol = new pmtiles.Protocol();
  maplibregl.addProtocol("pmtiles", protocol.tile);
  const archive = new URL("../../assets/tutorials/brazil.pmtiles", document.baseURI).href;
  const map = new maplibregl.Map({
    container: el,
    style: "https://tiles.openfreemap.org/styles/positron",
    center: [-55.55, -12.55],
    zoom: 7.5,
    cooperativeGestures: true,
  });
  map.addControl(new maplibregl.NavigationControl({ showCompass: false }));
  map.on("load", () => {
    map.addSource("brazil", { type: "vector", url: "pmtiles://" + archive });
    map.addLayer({
      id: "density", type: "fill", source: "brazil", "source-layer": "fields", maxzoom: 8,
      filter: ["has", "point_count"],
      paint: {
        "fill-color": ["interpolate", ["linear"], ["get", "point_count"],
          1, "#fde0c5", 25, "#f59e5b", 100, "#e85017", 534, "#7a1f05"],
        "fill-opacity": 0.65,
      },
    });
    map.addLayer({
      id: "density-outline", type: "line", source: "brazil", "source-layer": "fields", maxzoom: 8,
      filter: ["has", "point_count"],
      paint: { "line-color": "#7a1f05", "line-width": 1 },
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
