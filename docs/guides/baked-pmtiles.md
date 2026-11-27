# One file for analysis and the map

A dataset often ships as two files: the GeoParquet for DuckDB, GDAL, or
pandas, and a PMTiles archive for the web map. Two files drift apart and need
two URLs, two cache entries and two uploads. `tylertoo` can bake the archive
into the GeoParquet file instead, so one file serves both.

```bash
tylertoo tiles parcels.parquet parcels.pmtiles --bake parcels.baked.parquet
```

`tiles` writes `parcels.pmtiles` as usual. `parcels.baked.parquet` is a copy of
the input with the archive inside it. To bake an archive you already have, or
one built by `pyramid` or a sharded build:

```bash
tylertoo bake parcels.parquet parcels.pmtiles -o parcels.baked.parquet
```

## What readers see

The archive goes between the last row group and the footer, and the footer
gains one entry, `geo:pmtiles`, that points at it. No offset in the file
moves, so:

- DuckDB, GDAL, `pyarrow`, and every other Parquet reader see the original
  GeoParquet: same rows, same `geo` metadata, same row-group pruning.
- `tylertoo stats`, `decode`, `merge` and `serve` read the baked file like the
  archive it carries. `serve` serves the embedded archive as
  `/{name}.pmtiles`, so its viewer and any PMTiles client can read it.
- A browser finds the archive in one range request for the file's last 64 KB,
  then reads tiles at the archive's offset. A PMTiles source that adds that
  offset to each request is all a MapLibre page needs.

`bake --strip` removes the archive and gives back the original file, byte for
byte:

```bash
tylertoo bake --strip parcels.baked.parquet -o parcels.parquet
```

The layout and the rules a reader applies are in the
[overviews specification](../spec/overviews.md), section 14.

## Costs and limits

- The baked file is the GeoParquet plus the archive. On MassGIS's 2.15 million
  parcels, a 617 MB GeoParquet and a 217 MB archive make an 834 MB file.
- `--bake` copies the input, so it needs one local GeoParquet file. For a
  directory, a URL or a sharded build, tile first, then run `tylertoo bake` on
  a local copy of the file.
- A tool that rewrites the data can keep the footer metadata and carry the
  `geo:pmtiles` entry into a file where it no longer points at the archive.
  Readers check the entry against the file and ignore one that does not point
  at a PMTiles archive, and `tylertoo` reports it. Bake again after a rewrite.

## Python

```python
import tylertoo  # notest: needs your own parcels.parquet

tylertoo.overview("parcels.parquet", "overview.parquet")
tylertoo.export_pmtiles("overview.parquet", "parcels.pmtiles")
tylertoo.bake("parcels.parquet", "parcels.pmtiles", "parcels.baked.parquet")
tylertoo.strip("parcels.baked.parquet", "parcels.parquet")
```
