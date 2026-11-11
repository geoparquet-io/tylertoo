// The Brazil tutorial's map: step 1's brazil.pmtiles (all of Brazil, 2025)
// on Source Cooperative, read by HTTP range requests, over an OpenFreeMap
// basemap. One layer, "fields": A5 cells (with point_count) at z0-7, fields
// from z8.
(function () {
  const el = document.getElementById("brazil-map");
  if (!el || !window.maplibregl || !window.pmtiles) return;
  const protocol = new pmtiles.Protocol();
  maplibregl.addProtocol("pmtiles", protocol.tile);
  const archive = "https://data.source.coop/nlebovits/tylertoo-brazil-demo/brazil.pmtiles";

  // A cell's count grows about fourfold per zoom out, so one scale would put
  // every cell of a zoom in one class. Each zoom gets its own breaks: the
  // sextiles of point_count over that zoom's cells in this archive.
  const BREAKS = {
    0: [6800, 23000, 57000, 230000, 300000], 1: [780, 2400, 15000, 44000, 110000],
    2: [220, 1100, 6200, 16000, 30000], 3: [58, 610, 2100, 4500, 8800],
    4: [46, 270, 680, 1200, 2400], 5: [25, 95, 200, 360, 650],
    6: [10, 30, 57, 99, 180], 7: [4, 10, 17, 30, 56],
  };
  // A single-hue sequential ramp, light (few fields) to dark (many).
  const RAMP = ["#cde2fb", "#86b6ef", "#3987e5", "#256abf", "#184f95", "#0d366b"];
  const classes = (z) => {
    const b = BREAKS[z];
    const expr = ["step", ["get", "point_count"], RAMP[0]];
    b.forEach((v, i) => expr.push(v, RAMP[Math.min(i + 1, RAMP.length - 1)]));
    return expr;
  };
  const fill = ["step", ["zoom"], classes(0)];
  for (let z = 1; z <= 7; z++) fill.push(z, classes(z));

  const map = new maplibregl.Map({
    container: el,
    style: "https://tiles.openfreemap.org/styles/positron",
    center: [-53, -14],
    zoom: 3.5,
    cooperativeGestures: true,
  });
  map.addControl(new maplibregl.NavigationControl({ showCompass: false }));
  map.addControl(new maplibregl.FullscreenControl());

  const legend = document.createElement("div");
  legend.className = "tutorial-map-legend";
  el.appendChild(legend);
  const fmt = (v) => v.toLocaleString();
  const drawLegend = () => {
    const z = Math.floor(map.getZoom());
    if (z > 7) {
      legend.innerHTML = "<strong>Fields</strong> (zoom out for density)";
      return;
    }
    const b = BREAKS[z];
    const rows = RAMP.slice(0, b.length + 1).map((c, i) => {
      const lo = i === 0 ? 1 : b[i - 1];
      const label = i === b.length ? `${fmt(lo)}+` : `${fmt(lo)}–${fmt(b[i] - 1)}`;
      return `<div><span style="background:${c}"></span>${label}</div>`;
    });
    legend.innerHTML = `<strong>Fields per A5 cell</strong>${rows.join("")}`;
  };

  map.on("load", () => {
    map.addSource("brazil", { type: "vector", url: "pmtiles://" + archive });
    map.addLayer({
      id: "density", type: "fill", source: "brazil", "source-layer": "fields", maxzoom: 8,
      filter: ["has", "point_count"],
      paint: { "fill-color": fill, "fill-opacity": 0.8 },
    });
    map.addLayer({
      id: "density-outline", type: "line", source: "brazil", "source-layer": "fields", maxzoom: 8,
      filter: ["has", "point_count"],
      paint: { "line-color": "#ffffff", "line-width": 0.5, "line-opacity": 0.6 },
    });
    map.addLayer({
      id: "fields", type: "fill", source: "brazil", "source-layer": "fields", minzoom: 8,
      paint: { "fill-color": "#05221d", "fill-opacity": 0.15 },
    });
    map.addLayer({
      id: "fields-outline", type: "line", source: "brazil", "source-layer": "fields", minzoom: 8,
      paint: { "line-color": "#05221d", "line-width": 1 },
    });
    drawLegend();
  });
  map.on("zoomend", drawLegend);
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
