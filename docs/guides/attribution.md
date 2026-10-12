# Name, description, and attribution

A PMTiles archive can carry a human-readable name, a description, and an
attribution string. The archive carries the credit; a viewer has to be set up
to read and display it (see [MapLibre](#showing-the-attribution-in-maplibre)).
A map that doesn't load the metadata shows no credit, so check what your data
license requires of the page, not only of the file.

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
which names the tileset after the output file when `-n` is unset and copies the
name into `description` when `-N` is unset. A tippecanoe command with `-n` but
no `-N` therefore writes the name twice; pass `--description` with the same
value to keep that.
An empty value (`--attribution ''`) is a real value: tylertoo writes an empty
string.

tylertoo doesn't infer a credit from the input file, its properties, or its
GeoParquet metadata. State it yourself.

An archive's metadata is limited to 16 MiB, before and after compression.
tylertoo checks early that your three values fit on their own, before any
conversion work. That is only a lower bound: the structural metadata
(`vector_layers` and the rest) adds bytes, and the compressed section is
checked again when the archive is written, so a value near the limit can still
be refused during writing.

`tiles --plan-only` writes no archive, so it ignores these flags and reports
them as ignored. It still checks their size, so a fleet's preflight catches a
value no job could write.

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

An input's empty attribution (`""`) counts only when no input carries a
credit: with any non-empty credit present, the empty ones are discarded. If
every input's attribution is empty, the result is an empty string. An explicit
`--attribution ''` is different: it replaces the whole inherited result.

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

The PMTiles protocol for MapLibre GL JS reads an archive's metadata only when
you ask it to. Create the protocol with the `metadata: true` option. Without
it, the map never sees the attribution, and its attribution control shows
only MapLibre's own credit. Leave the vector source's own `attribution`
unset and MapLibre shows the archive's in that control, with links intact.

The official
[PMTiles for MapLibre GL JS](https://docs.protomaps.com/pmtiles/maplibre)
page has a complete example, including the protocol setup. tylertoo's docs
tests don't run JavaScript, so this guide ships no script of its own.

The archive's metadata is also what `pmtiles show` prints and what
`PMTiles.getMetadata()` and `getTileJson()` return.
