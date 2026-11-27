# Name, description, and attribution

Set `--name`, `--description`, and `--attribution` on `tiles`, `export-pmtiles`, `pyramid`, or `merge`.

In Python, `convert()`, `export_pmtiles()`, and `pyramid()` accept `name`, `description`, and `attribution`.
These describe the archive; `--layer-name` names the vector layer inside it.

--8<-- "examples/attribution/README.md"

## What gets stored

Values are stored byte for byte, without HTML escaping, so use HTML you trust.
An unset flag omits its metadata key; an empty string (`''`) stays empty.
Nothing is inferred from your input. tippecanoe fills in a name and description from the output filename.

## Merged and pyramid archives

An explicit flag wins for its field, even when empty. Otherwise, input metadata determines each field:

| Inputs carry | Result |
|---|---|
| Nothing | The key is left out |
| One value, or the same value repeated | That value |
| Different names, or different descriptions | The key is left out, with a warning |
| Different attributions | Every distinct credit, sorted, joined with `" \| "` |

Credits are never dropped. GeoParquet bands carry no metadata to inherit; PMTiles bands carry their full credit.
Give every shard the same `--name` and `--attribution`.

## Showing the attribution in MapLibre

Use `new pmtiles.Protocol({ metadata: true })` so MapLibre reads the archive's attribution.
Leave the vector source's `attribution` unset to show the archive's credit, including links.
See the [PMTiles example for MapLibre GL JS](https://docs.protomaps.com/pmtiles/maplibre).
