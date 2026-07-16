# Known Limitations

Things gpq-tiles does not do yet. Each is tracked in an open issue;
none has a workaround that gpq-tiles silently applies for you.

## Antimeridian / ±180° not handled

Geometries that cross the antimeridian (or that are stored with a bbox
spanning nearly the whole world, as unsplit crossing features are) are
not split at ±180°. At coarse zooms such a feature can be assigned to
tiles across the entire longitude range and render as a horizontal
smear; datasets touching Fiji, Chukotka, the Aleutians, or global
extents are the ones that hit this. The planned fix is an in-engine
split-first pass with a span-budget backstop, since GeoParquet — unlike
GeoJSON's RFC 7946 — does not require producers to split these
geometries themselves. Until it lands, split crossing features upstream.
Tracked in
[#240](https://github.com/geoparquet-io/gpq-tiles/issues/240).

## One layer per output

A PMTiles archive produced by gpq-tiles contains exactly one MVT layer
(`--layer-name`, default derived from the input name). There is no way
to feed several GeoParquet inputs into one archive as separate layers,
the way tippecanoe's `-L` does — today you would produce one archive
per layer and combine them client-side in the map style. Multi-layer
support (multiple inputs, one archive) is tracked in
[#16](https://github.com/geoparquet-io/gpq-tiles/issues/16).

## No polygon coalescing at coarse zooms

When polygons get too small to see at a given zoom, gpq-tiles drops
them (or, with `--collapse`, replaces them with representative points)
— it does not merge neighbors into larger polygons the way
tippecanoe's `--coalesce-smallest-as-needed` can. For full-coverage
layers (land cover, parcels, dense building fabric) this means coarse
zooms show a thinned sample or dot fill rather than a merged surface;
the [tuning guide](OVERVIEW_TUNING.md#country-scale-dot-fill-for-dense-polygon-layers)
documents the dot-fill recipe that works today. Coverage-preserving
polygon coalescing is tracked in
[#246](https://github.com/geoparquet-io/gpq-tiles/issues/246).
