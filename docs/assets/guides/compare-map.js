(function () {
  const select = document.getElementById("compare-dataset");
  const tools = ["tylertoo", "tippecanoe"];
  const elements = tools.map(tool => document.getElementById("compare-" + tool));
  if (!select || elements.some(el => !el) || !window.maplibregl || !window.pmtiles) return;
  const protocol = new pmtiles.Protocol();
  maplibregl.addProtocol("pmtiles", protocol.tile);
  const maps = elements.map(container => new maplibregl.Map({
    container, style: "https://tiles.openfreemap.org/styles/positron",
    center: [0, 0], zoom: 1, cooperativeGestures: true,
  }));
  let syncing = false, loaded = 0, revision = 0;
  maps.forEach((map, index) => {
    map.addControl(new maplibregl.NavigationControl({ showCompass: false }));
    map.on("move", () => {
      if (syncing) return;
      syncing = true;
      try {
        maps[1 - index].jumpTo({ center: map.getCenter(), zoom: map.getZoom(),
          bearing: map.getBearing(), pitch: map.getPitch() });
      } finally { syncing = false; }
    });
    map.on("load", () => { if (++loaded === maps.length) update(); });
    map.on("click", event => {
      if (!map.getLayer("output")) return;
      const feature = map.queryRenderedFeatures(event.point, { layers: ["output"] })[0];
      if (!feature) return;
      const box = document.createElement("div");
      for (const [key, value] of Object.entries(feature.properties)) {
        const row = document.createElement("div");
        row.textContent = key + ": " + String(value);
        box.appendChild(row);
      }
      new maplibregl.Popup().setLngLat(event.lngLat).setDOMContent(box).addTo(map);
    });
  });
  function update() {
    if (loaded !== maps.length) return;
    const option = select.selectedOptions[0];
    if (!option) return;
    const data = option.dataset, current = ++revision;
    if (!data.tylertoo || !data.tippecanoe || !data.layer) return;
    const styles = {
      point: ["circle", { "circle-color": "#256abf", "circle-radius": 3 }],
      line: ["line", { "line-color": "#256abf", "line-width": 1.5 }],
      polygon: ["fill", { "fill-color": "#256abf", "fill-opacity": 0.65 }],
    };
    const style = styles[data.geometry];
    if (!style) return;
    maps.forEach((map, index) => {
      elements[index].setAttribute("aria-label", tools[index] + " output: " + option.textContent);
      if (map.getLayer("output")) map.removeLayer("output");
      if (map.getSource("output")) map.removeSource("output");
      map.addSource("output", { type: "vector", url: "pmtiles://" + data[tools[index]] });
      map.addLayer({ id: "output", type: style[0], source: "output",
        "source-layer": data.layer, paint: style[1] });
    });
    new pmtiles.PMTiles(data.tylertoo).getHeader().then(header => {
      if (current === revision) maps[0].jumpTo({
        center: [header.centerLon, header.centerLat], zoom: header.centerZoom,
      });
    }).catch(() => {});
  }
  select.addEventListener("change", update);
})();
