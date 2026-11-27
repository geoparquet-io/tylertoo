# Tuning knobs

Use this page to choose what to change. The [CLI reference](cli.md) and
[Python reference](python.md) own the accepted values, limits, and defaults.
Check the result in your map style as well as the build report.

## How a level is built

A ground sample distance (GSD) sets the scale for each level. Choose a zoom
range with [`--min-zoom`](cli.md#option-min-zoom) and [`--max-zoom`](cli.md#option-max-zoom), or supply distances with [`--gsd`](cli.md#option-gsd).
With zoom-based levels, increasing [`--gsd-base`](cli.md#option-gsd-base) keeps more detail.

The build selects input rows, chooses features for each level, simplifies
geometry, and exports tiles. Selection changes which features can appear.
Simplification changes their shapes. The export size valve can change both.

For reusable overview files, [`--mode`](cli.md#option-mode) selects complete, duplicating levels or
partitioning levels in which each feature appears once. Partitioning preserves
source geometry and does not support the same generalization choices.
See the [overview specification](../spec/overviews.md).

## Complete tiles or small tiles

Decide which information your map needs before adjusting individual knobs.

| If you need | Try | Trade-off |
| --- | --- | --- |
| A polygon coverage with connected borders | [`--no-simplification-of-shared-nodes`](cli.md#option-no-simplification-of-shared-nodes) | Extra work and memory to keep shared stretches aligned |
| Every selected feature and its source geometry | [`--verbatim`](cli.md#option-verbatim) | Dense tiles can become too large to serve |
| Fewer features at coarse scales | Thinning and the density budget | Some features disappear |
| More continuous coarse polygon coverage | [`--coverage-fill`](cli.md#option-coverage-fill) | Added area can overdraw scattered polygons and uses a representative feature's attributes |
| Smaller tiles within a delivery limit | Tile caps and the size valve | Geometry or attributes can change |

Compare the finest level as well as coarse levels. A style that draws polygon
fills cannot draw polygons represented as points without a circle layer.
For compatibility choices, see the [tippecanoe guide](../guides/tippecanoe.md).

## More or fewer features at coarse zooms

[`--point-thinning`](cli.md#option-point-thinning), [`--line-thinning`](cli.md#option-line-thinning), and [`--polygon-thinning`](cli.md#option-polygon-thinning) control the
selection grid for each geometry type. Larger cells keep fewer winners.
Polygon thinning can remove a nested polygon whose center shares another
polygon's cell. If every administrative area must remain, avoid thinning
polygons to reduce tile bytes.

[`--line-visibility`](cli.md#option-line-visibility) and [`--polygon-visibility`](cli.md#option-polygon-visibility) compare bounding-box diagonals
with the level's GSD. Lower a gate if small features disappear too early.
Polygons rejected by the gate can contribute collapse squares, so lowering
it does not necessarily add more visible area.

The density budget selects points after thinning. [`--drop-lines`](cli.md#option-drop-lines) and
[`--drop-polygons`](cli.md#option-drop-polygons) extend it to those geometry types. Dropping polygons can
open holes in a coverage. [`--no-density-drop`](cli.md#option-no-density-drop) disables the budget.
If points crowd the middle zooms, increase [`--drop-rate`](cli.md#option-drop-rate). If sparse areas
lose too many points, increase [`--drop-gamma`](cli.md#option-drop-gamma) to protect them.

### Ranking and entry zooms

Use [`--sort-key`](cli.md#option-sort-key) with [`--sort-direction`](cli.md#option-sort-direction) to choose the winner of a cell by a
numeric property. Use [`--class-rank`](cli.md#option-class-rank) for categorical priorities; in Python,
pass `class_rank_column=`, `class_ranks=`, and `class_rank_unknown=` instead.
[`--no-auto-rank`](cli.md#option-no-auto-rank) disables built-in ranking
for recognized schemas.

Use [`--magnitude-ladder`](cli.md#option-magnitude-ladder) with [`--ladder-step`](cli.md#option-ladder-step) when property values should
enter at successive zooms. Use [`--entry-zoom`](cli.md#option-entry-zoom) when you need explicit entry
zooms per value. Promoted features bypass ordinary thinning; check that the
result still fits your tile budget.

## Geometry detail

Lower [`--simplify-factor`](cli.md#option-simplify-factor) if coarse outlines lose detail. Higher values
remove more vertices. [`--no-cascade`](cli.md#option-no-cascade) simplifies each level from the source
rather than the next finer level, at the cost of more work.

For shared borders, use [`--no-simplification-of-shared-nodes`](cli.md#option-no-simplification-of-shared-nodes). Lowering the
tolerance alone does not make neighboring polygons simplify identically.
[`--max-zoom-simplify-factor`](cli.md#option-max-zoom-simplify-factor) controls a separate export step at the finest
level. It leaves the overview file unchanged; independent polygon
simplification can separate shared borders.

### Collapse and representation

Collapse squares preserve an approximation of the area of small polygons.
[`--collapse-square`](cli.md#option-collapse-square) requests squares explicitly. [`--collapse`](cli.md#option-collapse) requests
representative points, which need a point style. [`--no-collapse`](cli.md#option-no-collapse) drops the
small polygons instead. Under an entry-zoom ladder, automatic collapse uses
points; an explicit square request keeps squares.

Use [`--representation`](cli.md#option-representation) to choose geometry, points, or squares by zoom band.
For a build that combines feature and cell representations, see
[Several layers](#several-layers).

[`--verbatim`](cli.md#option-verbatim) turns off generalization. Explicit tuning options override it.
If you need source features at the finest scale but generalized coarse
levels, adjust the individual controls rather than making everything verbatim.

## Tile size

[`--max-compressed-tile-size`](cli.md#option-max-compressed-tile-size) caps delivery bytes. [`--max-tile-size`](cli.md#option-max-tile-size) is the
raw-size backstop. With several layers, both apply to the joined tile.
If a cap is exceeded, [`--size-valve`](cli.md#option-size-valve) chooses merging or feature dropping.
Merging keeps the target's attributes, so it cannot preserve every source
feature's identity. Dropping preserves each surviving feature's attributes.

[`--valve-scope`](cli.md#option-valve-scope) controls how far reductions spread beyond a dense tile.
With smooth scope, [`--valve-max-step`](cli.md#option-valve-max-step) controls the change between neighboring
tiles. If density changes look abrupt, reduce the step; inspect surrounding
tiles because the reduction can spread farther.

[`--tile-buffer`](cli.md#option-tile-buffer) extends geometry across seams. [`--extent`](cli.md#option-extent) controls coordinate
resolution inside a tile. Changing either can affect geometry and bytes.
[`--no-simple-clip-fastpath`](cli.md#option-no-simple-clip-fastpath) selects the full polygon overlay when you need
stable ring ordering across clipping paths.

Use [`--generate-variable-depth-tile-pyramid`](cli.md#option-generate-variable-depth-tile-pyramid) only if your server can find the
nearest ancestor when a tile is absent. Stop tiles carry finest-level features.

[`--tile-format`](cli.md#option-tile-format) chooses the encoding. For MLT, choose a compatible
[`--mlt-profile`](cli.md#option-mlt-profile) for your renderer. Check renderer support in the reference
before choosing a profile to reduce bytes.

## Points and lines

[`--cluster`](cli.md#option-cluster) combines points within a thinning cell and adds `point_count`.
Use [`--accumulate-attribute`](cli.md#option-accumulate-attribute) for numeric aggregates and [`--cluster-categories`](cli.md#option-cluster-categories)
for category counts. The representative keeps its location and other
attributes. Lines and polygons are not point clusters.

Line coalescing joins touching lines of the same class. [`--coalesce-snap`](cli.md#option-coalesce-snap)
controls endpoint snapping. [`--coalesce-junction-angle`](cli.md#option-coalesce-junction-angle) permits continuation
through junctions. If unrelated roads join, reduce snapping or junction
continuation, or use [`--no-coalesce-lines`](cli.md#option-no-coalesce-lines).

[`--coalesce-max-level-rows`](cli.md#option-coalesce-max-level-rows) bounds the candidates held for chaining. Long
lines can reach its geometry-size guard before its row-count guard.
Raising it trades memory for more coalescing.

Clustering and coalescing reserve output columns. Input columns with those
names are renamed; see the option help before relying on a property name.

## Several layers

Use [`--layer-name`](cli.md#option-layer-name) for a layer's name, or [`--layer-column`](cli.md#option-layer-column) to split one input
into layers by property value. [`--feature-order`](cli.md#option-feature-order) controls painting order
within each tile unless the style overrides it. [`--feature-id`](cli.md#option-feature-id) supplies an
integer identifier for feature state; merged features keep a representative id.

On [`tiles`](cli.md#tylertoo-tiles), [`--band`](cli.md#option-band) can mix geometry, points, squares, and aggregation cells
by zoom range. On [`pyramid`](cli.md#tylertoo-pyramid), the same flag assigns separate inputs to zoom
bands. See each command's help for the distinct syntax.

For `pyramid`, [`--generalize`](cli.md#option-generalize) applies generalization to GeoParquet bands.
[`--allow-missing-zooms`](cli.md#option-allow-missing-zooms) accepts sparse pre-tiled bands, whose missing zooms
render empty. Use [`--work-dir`](cli.md#option-work-dir) to place temporary band files on suitable disk.

## Memory and speed {#memory}

Start with [`--profile auto`](cli.md#option-profile), which sizes buffers to the machine and spills
when needed. Choose `bounded` when memory is tight and disk is available.
Choose `speed` when the working set fits in RAM. Profiles change memory
behavior, not the intended output.

[`--memory-limit`](cli.md#option-memory-limit) replaces the detected available memory; it is not an extra
limit layered over detection. Set it only to memory the run can use.
[`--workers`](cli.md#option-workers) also caps the core count used to size buffers. The thread pool
is process-wide, so the first Python call sets it.

Use [`--spill-dir`](cli.md#option-spill-dir) or `TMPDIR` on real local disk with enough free space.
A tmpfs `/tmp` counts against the memory limit. A slow network volume makes
staging, intermediate writes, and spill reads slow.

### When the memory check refuses a build

Read the estimate in the preflight message. Reduce the selected input with a
region or filter, use a larger machine, or reuse a compatible [`--plan`](cli.md#option-plan) to
skip the first scan. A saved plan avoids the feature table, not export memory.
Wide property schemas add to the working set; exclude properties you do not need.
If line chaining is the problem, lower [`--coalesce-max-level-rows`](cli.md#option-coalesce-max-level-rows) or use
[`--no-coalesce-lines`](cli.md#option-no-coalesce-lines). To size a run, use `true_peak_mib` from the
`TYLERTOO_PROFILE_JSON` record rather than the process high-water RSS.

If a single tile exceeds memory, changing [`--partition-wave`](cli.md#option-partition-wave) cannot split it.
Rebuild with fewer features at that zoom. With [`--verbatim`](cli.md#option-verbatim), turn thinning
back on for the dense geometry type, or drop verbatim mode. You can also raise
[`--min-zoom`](cli.md#option-min-zoom) to omit the dense coarse levels. Override a preflight refusal only when you
have checked that its estimate or detected limit is wrong; follow the error's
instructions.

### Read batches and reusable work

Leave [`--in-flight-batches`](cli.md#option-in-flight-batches), [`--read-workers`](cli.md#option-read-workers), and [`--partition-wave`](cli.md#option-partition-wave) on
`auto` until profiling identifies a bottleneck. More concurrency also holds
more work in memory. Lower [`--read-batch-size`](cli.md#option-read-batch-size) if individual decoded batches
are too large; it does not change feature selection.

[`--temp-compression`](cli.md#option-temp-compression) trades intermediate-file bytes against codec work.
[`--keep-overview`](cli.md#option-keep-overview) retains that intermediate for later exports.
[`--save-plan`](cli.md#option-save-plan) records selection work; [`--plan`](cli.md#option-plan) reuses it only when the
fingerprint matches the inputs and options.

### Paths and interrupted exports

An interrupted export can leave a checkpoint at `OUTPUT.partial`.
A later attempt preserves an existing partial as `OUTPUT.partial.prev`.
A completed export renames the new partial into place. Keep a recoverable
checkpoint before retrying if you need it.

## Filtering and selection

Use [`--bbox`](cli.md#option-bbox) for a longitude/latitude region, [`--mask`](cli.md#option-mask) for an intersecting
polygon region, and [`--filter`](cli.md#option-filter) for property predicates. They combine with
AND. Bounding-box and property statistics can skip row groups before reading
their data. Prepare spatially sorted input as in the
[Madagascar tutorial](../tutorials/madagascar.md#2-preview-one-region).

When the box starts with a negative longitude, write `--bbox=` with an
equals sign so the value is not read as a flag:

```bash
tylertoo tiles input.parquet output.pmtiles --bbox=-74,40,-73,41 --filter 'confidence > 0.8'
```

### Expression syntax

Put the column on the left of a comparison. The value can be a number,
a quoted string, or a Boolean. Combine predicates with logical operators
and parentheses.

- Use `AND`, `OR`, and `NOT` to combine or negate predicates.
- Use `IN` to test membership in a list.
- Use `IS NULL` or `IS NOT NULL` to test missing values.
- Use `LIKE` for string patterns, or `ILIKE` to ignore case.

Double an embedded quote in a string. Double-quote column names that contain
spaces.

Only true predicates keep rows. Nulls and NaNs produce unknown comparisons;
negating such a comparison does not keep the row. Test missing values
explicitly. These examples are checked by the filter parser:

<!-- check:filter -->
```text
confidence > 0.8
class IN ('primary', 'secondary') AND name IS NOT NULL
NOT (height < 10) OR height IS NULL
"road name" ILIKE 'são%'
name = 'it''s a road'
```

Use [`--include-property`](cli.md#option-include-property), [`--exclude-property`](cli.md#option-exclude-property), or
[`--exclude-all-properties`](cli.md#option-exclude-all-properties) to reduce attributes. Inclusion takes precedence.
Keep columns required by filtering, ranking, ordering, or identifiers.
At conversion time, excluded properties can avoid decoding work; at export
time, the overview still contains them.

## Output layout

For reusable overviews, [`--row-group-size`](cli.md#option-row-group-size) sets the row-group cap.
[`--row-group-size-policy`](cli.md#option-row-group-size-policy) can increase it toward coarse levels. Larger groups
can reduce requests for broad queries but read more data for small viewports.
[`--full-column-stats`](cli.md#option-full-column-stats) keeps statistics on large columns when clients need
property filtering. [`--cogp-compat`](cli.md#option-cogp-compat) adds a third-party footer key for
partitioning output.

The [sweep history](https://github.com/geoparquet-io/tylertoo/blob/main/corpus/SWEEPS.md)
records how maintainers chose the shipped settings.
