# Working with the overview file

The overview file is the artifact the two-step workflow keeps. Getting Started
built one, validated it, and exported it to PMTiles. This topic explains what
that file is: how one GeoParquet file carries every zoom level, why it stays a
file you can query with DuckDB instead of an opaque tile blob, and what that
buys you between building it and rendering it.

For readers coming to `geo:overviews` as a GeoParquet capability rather than as
a tiling step, this is the topic that describes the format itself.

## Design decisions

**The overview stays valid GeoParquet.** An overview is an ordinary GeoParquet
file with one added column and a block of metadata. Any Arrow, Parquet, or
DuckDB reader opens it and reads its features. Nothing about the multi-level
structure requires a special reader, so the file remains inspectable, queryable,
and portable long after it is built.

**One level column holds every zoom band.** Rather than emit a separate file per
zoom, the converter writes all levels into one file and tags each row with the
`level` it belongs to. A reader selects a resolution with `WHERE level = N`. The
whole pyramid travels as a single object, which matters when that object lives
in a bucket and gets copied, cached, or served by range request.

**The canonical finest level stays verbatim.** The maximum-zoom level holds your
features at full detail, unsimplified and unthinned. Every coarser level is a
derived generalization of it. Because the finest level loses nothing, the
overview is a superset of the input geometry, and full detail is always one
`WHERE level = max` query away.

**Duplicating mode repeats features across levels.** In the default mode each
level carries its own thinned and simplified copy of the features it shows, so a
level is self-contained and renders without joining back to another level. This
trades file size for read simplicity. Partitioning mode is the alternative that
places each feature once, for readers that reconstruct a level from its zoom
prefix.

**Metadata records the zoom-to-GSD ladder.** The footer's `geo:overviews`
metadata records each level's zoom and its ground sample distance. A reader
learns what resolution each band represents from the file itself, without
inferring it from the data, which is what lets a spec-aware client pick the
right level for a viewport.

## API walkthrough

### Building the overview

**`tylertoo overview <in> <out>`.** Reads prepared GeoParquet and writes the
multi-resolution pyramid into one file. The remaining flags shape the ladder it
builds.

**`--min-zoom` / `--max-zoom`.** The zoom range the ladder spans. `--max-zoom`
sets the canonical level and defaults to 6, coarse enough for a continental
view; a street-level map raises it, as the tutorial's 14 did.

**`--gsd <list>`.** An explicit, strictly decreasing ground-sample-distance
ladder in meters, for driving resolution directly instead of by zoom. It
overrides the zoom range when set.

**`--mode duplicating|partitioning`.** Chooses how levels materialize, per the
duplicating-versus-partitioning decision above.

### Reading the level structure

**The `level` column.** The band each row belongs to. Every query against an
overview narrows to a resolution through it, so `SELECT count(*) ... GROUP BY
level` is the fastest way to see the pyramid's shape.

**The `geo:overviews` footer metadata.** The per-level zoom-to-GSD record. Read
it to map a `level` value onto the resolution it represents.

**DuckDB or any Parquet reader.** Because the file is plain GeoParquet, the
spatial SQL you already run applies to it. You can count features per level,
extract one level to its own file, or inspect geometry before committing to an
export.

### Validating against the spec

**`tylertoo validate <file>`.** Checks the file against the `geo:overviews`
specification, section 6.2: the level metadata, the zoom-to-resolution mapping,
and the per-level structure. A file that passes agrees with what a spec-aware
reader expects, so it moves downstream without further inspection.

### Re-exporting without recomputing geometry

**`tylertoo export-pmtiles <ov> <out>`.** Reads the levels as they are and packs
them into tiles. No geometry is recomputed at export, so the levels you built
during convert become the tiles served to the map.

**Re-exporting with different options.** Because the overview is the durable
artifact, one build supports many exports. Change `--layer-name` to match a
different map style, or set a tile-size limit for a different renderer, without
rebuilding the pyramid. This is the payoff that justifies keeping the overview
as a first-class file rather than collapsing the workflow into one step.
