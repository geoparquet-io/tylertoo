# Name, description, and attribution

A PMTiles archive can carry a human-readable name, a description, and an
attribution string. Viewers read the attribution and show it on the map, so
data licenses that require a credit (OpenStreetMap, national agencies, open
data portals) are met by the archive itself rather than by every page that
embeds it.

Three flags set them. Each takes one string and is optional:

| Flag | Stored as | tippecanoe |
|---|---|---|
| `--name` | `name` | `-n` |
| `--description` | `description` | `-N` |
| `--attribution` | `attribution` | `-A` |

```bash
tylertoo tiles parks.parquet parks.pmtiles \
  --name "Park boundaries" \
  --description "National park boundaries, 2025 release" \
  --attribution '<a href="https://www.nps.gov/">National Park Service</a>'
```

The flags work on every command that writes an archive:

- `tiles`
- `export-pmtiles`
- `pyramid`
- `merge`

In Python, pass the same three names as keywords to
[`export_pmtiles()`](../reference/python.md). `overview()` and the deprecated
`convert()` take none; run `overview()` and then `export_pmtiles()`.

`--layer-name` is a different setting: it names the MVT layer that styles
refer to. The three flags describe the tileset as a whole.

## What gets stored

tylertoo stores each string exactly as given. It doesn't trim it, escape it, or
check the HTML. Quotes, newlines, and Unicode survive, and a consuming
application decides how to render the markup, so write only attribution HTML
you trust.

An unset flag leaves its key out of the archive. This differs from tippecanoe,
which fills `name` and `description` with the output filename when you give none.
An empty value (`--attribution ''`) is a real value: tylertoo writes an empty
string.

tylertoo doesn't infer a credit from the input file, its properties, or its
GeoParquet metadata. State it yourself.

An archive's metadata is limited to 16 MiB, before and after compression.
A value that cannot fit is refused before any conversion starts.

## Merged and pyramid archives

`merge` and `pyramid` build one archive from several. Each of the three fields
resolves on its own, from this order:

1. An explicit flag on the command wins, even an empty one. It overrides only
   its own field.
2. Otherwise, the inputs' values are combined:

| Inputs carry | Result |
|---|---|
| Nothing | The key is left out |
| One value, or the same value repeated | That value |
| Different names, or different descriptions | An error that asks for `--name` or `--description` |
| Different attributions | Every distinct credit, sorted, joined with `" \| "` |

tylertoo refuses to choose between two names because the answer would depend
on the order of the inputs. Credits are different: dropping one would stop crediting a source, so
all are kept. An already-joined credit stays one string and isn't split, so
merging a merged archive can repeat a source's name. Pass an explicit
`--attribution` for the final archive if that matters.

Sorting uses plain byte order, so the result doesn't depend on the order you
list the inputs.

A sharded build needs no extra step. Every shard job takes the same flags (an
empty shard still writes them), and `merge` combines them:

```bash
tylertoo merge fields.pmtiles coarse.pmtiles shard-*.pmtiles
```

GeoParquet bands in a `pyramid` contribute nothing. Only PMTiles bands carry
values to inherit, and the flags on `pyramid` set the final archive's own
values. A band taken from a PMTiles archive keeps that archive's full
attribution, because tylertoo cannot tell which credit belongs to which
feature.

## Showing the attribution in MapLibre

The PMTiles protocol for MapLibre GL JS reads the archive's metadata only when
you ask it to. Without `metadata: true`, the map never sees the attribution:

```javascript
const protocol = new pmtiles.Protocol({ metadata: true });
maplibregl.addProtocol("pmtiles", protocol.tile);
```

Leave the vector source's own `attribution` unset and MapLibre shows the
archive's, in its attribution control, with links intact. See the official
[PMTiles for MapLibre GL JS](https://docs.protomaps.com/pmtiles/maplibre)
example for a complete page.

The archive's metadata is also what `pmtiles show` prints and what
`PMTiles.getMetadata()` and `getTileJson()` return.
