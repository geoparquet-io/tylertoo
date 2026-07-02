# GeoParquet Overviews Specification

Version: `0.1.0`
Status: Draft — §11 design decisions APPROVED by maintainer
2026-07-02; implementation may proceed against this document.
License intent: CC BY 4.0
Standardization target: candidate official GeoParquet extension via
the opengeospatial/geoparquet process (the `covering` path into 1.1)

This document specifies **COG-style multi-resolution overviews** embedded
in a GeoParquet file. It defines a physical layout, a footer metadata
schema, and read/write protocols that let a client fetch only the
resolution band it needs over HTTP range requests, while the file remains
a fully valid GeoParquet 1.1 file readable by any existing tool.

It is designed for standardization through the **official GeoParquet
spec process** (opengeospatial/geoparquet): the metadata is structured
so it can be merged into the `geo` file metadata as a spec-level
extension (as `covering` was in 1.1), and it incubates under its own
footer key until then. The layout concepts of `partitioning` mode
(§2.3) are shared with the third-party COGP draft (Kanahiro); COGP
interop is an optional courtesy (§3.1), not a design constraint.

The key words MUST, MUST NOT, REQUIRED, SHOULD, SHOULD NOT, MAY, and
OPTIONAL are to be interpreted as in RFC 2119.

Open questions raised inline are collected in §11.

---

## 1. Scope and terminology

### 1.1 Purpose

Provide "COG for vector": a single self-describing GeoParquet file that
carries several precomputed generalizations of a dataset at increasing
levels of detail, so that a renderer can request a coarse overview cheaply
and progressively refine, and an analyst can recover the exact source data
with a single predicate.

### 1.2 Terms

- **Overview file**: a GeoParquet 1.1 file conforming to this spec.
- **Level**: a logical rendering-detail band, numbered `0` (coarsest) to
  `L-1` (finest). A level is materialized as a contiguous run of Parquet
  row groups and identified in metadata.
- **GSD (ground sample distance)**: the approximate smallest ground
  distance, **in meters**, that is independently meaningful at a level.
  GSD governs which features/vertices survive at a level; it is not an
  accuracy guarantee. Always meters, regardless of the file CRS (§7.1).
- **Canonical level**: the single level whose rows reproduce the source
  dataset value-for-value (§2.4). In `duplicating` mode this is the finest
  level; in `partitioning` mode the union of all levels is canonical
  (§2.3).
- **Generalization**: the process of producing a coarser rendering —
  feature thinning (dropping whole features), simplification (vertex
  reduction), and optional collapse (e.g. polygon → point).
- **Covering**: the GeoParquet 1.1 `bbox` covering struct column
  (`xmin, ymin, xmax, ymax`) with per-row-group statistics, used for
  spatial pruning.
- **Row group (RG)**: a Parquet row group. RG indices are zero-based.

### 1.3 Relationship to prior art

- **COGP** (third-party draft; feature-level, no simplification,
  prefix reads): this spec's `partitioning` mode shares its concepts;
  we deliberately keep its `gsd` and `row_group_end` field semantics
  so a compatibility key is cheap to emit (§3.1).
- **Cloud Optimized GeoTIFF / COPC**: the `duplicating` mode borrows the
  COG read model — one level band is a complete, self-contained rendering
  of the whole dataset; a reader fetches exactly one band.
- **ESRI Spatially Optimized Parquet**: rejected model (per-scale geometry
  *columns*). We use extra *rows*, not extra columns (§2.1).

---

## 2. Level model

### 2.1 Row model (normative)

Overview levels MUST be represented as **additional rows**, not additional
columns. Every row belongs to exactly one level. A file MUST NOT encode
levels as parallel geometry columns.

Rationale: extra rows keep the file a valid, flat GeoParquet table that any
SQL engine can filter with one predicate (`WHERE level = k`), and let
row-group pruning operate per level without schema games.

### 2.2 `duplicating` mode

Each level is a **self-contained generalized rendering of the entire
dataset**. A feature that is visible at level `k` typically also appears
(usually more generalized) at all coarser levels `0..k` and (usually less
generalized) at all finer levels `k..L-1`. A reader selects and fetches
**exactly one** level band for a given display scale.

Constraints:

- Levels `0..L-1` MUST be ordered coarse → fine.
- The **finest** level (`L-1`) is the canonical level (§2.4) and MUST
  reproduce the source data value-for-value.
- A coarser level MAY contain fewer features than a finer level (thinning)
  and MAY contain simplified or collapsed geometries.
- Feature identity across levels is NOT required to be stable, and readers
  MUST NOT assume a feature at level `k` corresponds 1:1 to a feature at
  level `k+1`. (Cross-level joins are a non-goal; see §10.)

Features are duplicated across levels in this mode; it has no
counterpart in the third-party COGP draft (which requires each feature
in exactly one row group).

### 2.3 `partitioning` mode

Each feature appears **exactly once**, placed at its **coarsest** level —
the coarsest level at which it is still meaningful under that level's GSD.
No geometry is simplified or duplicated; only the assignment of features to
levels (and the physical ordering) differs from an ordinary file. A reader
fetches the **prefix** of levels `0..z` for target level `z`.

Constraints:

- Every source feature MUST appear in exactly one row group, geometry
  preserved verbatim.
- Levels MUST be ordered coarse → fine and end on RG boundaries (§4).
- The **union of all levels** is the canonical dataset. There is no single
  canonical level row-band; the canonical level pointer (§3.4) MUST be
  `null` in this mode, signalling "all rows, no filter" (§6.2).

A `partitioning`-mode file's layout is concept-compatible with the
third-party COGP draft; a writer MAY additionally emit the `cogp`
compatibility key (§3.1) to make it consumable by COGP readers. COGP
validity is NOT a conformance requirement of this spec.

### 2.4 Canonical fidelity (normative)

In `duplicating` mode, the geometry **and** all attribute values of every
canonical-level row MUST be value-identical to the corresponding source
row. "Value-identical" means: identical attribute values, and geometry
whose coordinates are bit-for-bit equal after decoding (no simplification,
no reprojection, no coordinate rounding beyond what the source already
contained). Physical encoding (compression, dictionary, page layout) MAY
differ.

> OPEN QUESTION Q1: Do we require **row-order** preservation within the
> canonical level (i.e. canonical rows in source order), or only set
> equality? Hilbert sorting within the level (§4.3) reorders rows, so
> strict source-order preservation conflicts with the spatial-sort SHOULD.
> Recommendation: require **set/value** equality only, NOT row order, and
> state that explicitly. See §11.

---

## 3. Footer metadata

### 3.1 Key strategy (REVISED 2026-07-02)

**Decision: incubate under our own footer key, designed for merger
into the official `geo` metadata as a GeoParquet spec extension.**

- **Key name**: `geo:overviews` (UTF-8 string), value is a UTF-8 JSON
  object with the schema in §3.2. The `geo:`-prefixed sibling key
  signals spec-track intent while keeping the incubating structure
  out of the `geo` object itself (so no reader can mistake it for
  ratified 1.1/2.0 metadata). Upon standardization, the object moves
  verbatim to an `overviews` member of the `geo` metadata and the
  sibling key is dropped — the JSON schema is designed so this is a
  key rename only.

Rationale: this spec's standardization path runs through the official
opengeospatial/geoparquet process (the same path `covering` took into
1.1), not through third-party drafts. Structuring the metadata for
`geo`-merger from day one is what makes the eventual extension
proposal a rename rather than a redesign.

**COGP interop (optional, informative).** The `partitioning`-mode
layout is concept-compatible with the third-party COGP draft
(Kanahiro), and our `levels` schema deliberately keeps COGP's
`row_group_end`/`gsd` field semantics. A writer MAY additionally emit
a `cogp` footer key containing the COGP-subset fields
(`version`, `levels[].{row_group_end, gsd}`) for `partitioning`-mode
files so existing COGP readers can consume them. When both keys are
present the `geo:overviews` key is authoritative; a validator MUST
flag disagreement between them as an error. A COGP prefix-reader that
encounters a `duplicating`-mode file (via a MAY-emitted `cogp` key)
over-fetches — levels are self-contained, so a prefix is a superset —
but is never wrong; writers SHOULD nonetheless omit the `cogp` key in
`duplicating` mode to avoid advertising semantics COGP does not
define.

> OPEN QUESTION Q2 (revised): Is `geo:overviews` the right incubation
> key name, and should the MAY-emit `cogp` compatibility key exist at
> all? Recommendation: `geo:overviews`; emit `cogp` only behind an
> explicit writer flag, default off. To be settled with the GeoParquet
> spec maintainers directly. See §11.

### 3.2 JSON schema

```
{
  "version":         string,   // REQUIRED, semver MAJOR.MINOR.PATCH
  "levels":          [Level],  // REQUIRED, non-empty, coarse→fine
  "mode":            string,   // OPTIONAL: "duplicating" | "partitioning"
  "canonical_level": integer | null,  // OPTIONAL (see §3.4)
  "generalization":  Provenance        // OPTIONAL, informative (§3.5)
}

Level := {
  "row_group_end": integer,   // REQUIRED, inclusive, 0-based
  "gsd":           number,    // REQUIRED, meters, > 0
  "zoom":          integer     // OPTIONAL, Web Mercator z (§5.2)
}
```

All fields are defined by this spec. (`version`, `levels`,
`levels[].row_group_end`, and `levels[].gsd` also form the
COGP-compatible subset used by the optional `cogp` key, §3.1.)

### 3.3 `levels` constraints (normative)

- `levels` MUST be non-empty and ordered coarse → fine.
- `row_group_end` MUST be a JSON integer with
  `0 <= row_group_end < num_row_groups`, zero-based, and **strictly
  monotonically increasing** across the array. The final entry's
  `row_group_end` MUST equal `num_row_groups - 1` (levels cover all row
  groups, no gaps). Level `k` spans row groups
  `(levels[k-1].row_group_end + 1) .. levels[k].row_group_end` inclusive
  (level 0 starts at RG 0).
- `gsd` MUST be a positive JSON number and MUST be **strictly
  monotonically decreasing** across the array (coarse→fine ⇒ larger→smaller
  meters).
- If `zoom` is present on any level it SHOULD be present on all levels,
  MUST be a non-negative integer, and MUST be strictly monotonically
  increasing (consistent with decreasing `gsd`).

### 3.4 `mode` and `canonical_level`

- `mode` SHOULD be present. If absent, readers MUST assume
  `partitioning` (the non-duplicating default: safest assumption for a
  reader, since treating a partitioning file as duplicating would
  drop data).
- `canonical_level`:
  - In `duplicating` mode it MUST be present and MUST equal
    `len(levels) - 1` (the finest level). A reader/analyst uses it to
    build the canonical predicate (§5.2, §6.2).
  - In `partitioning` mode it MUST be `null` (or absent), meaning
    "the whole table is canonical; no level filter needed."

### 3.5 `generalization` provenance (informative)

An OPTIONAL object recording how each level was generalized, for
reproducibility and debugging. Informative only; readers MUST NOT rely on
it for correctness.

```
Provenance := {
  "engine":  string,           // e.g. "gpq-tiles 0.4.0"
  "levels":  [ {               // parallel to top-level "levels"
      "simplify_tolerance_m": number,   // world-space tolerance, meters
      "thinning_factor":      number,   // cell-winner factor
      "visibility_gate_m":    number,   // min bbox-diagonal kept, meters
      "geometry_types":       [string]  // union of kinds at this level
  } ]
}
```

### 3.6 Example — `duplicating` mode

```json
{
  "version": "0.1.0",
  "mode": "duplicating",
  "canonical_level": 2,
  "levels": [
    { "row_group_end": 1,  "gsd": 9783.94, "zoom": 2 },
    { "row_group_end": 5,  "gsd": 2445.98, "zoom": 4 },
    { "row_group_end": 14, "gsd": 611.50,  "zoom": 6 }
  ],
  "generalization": {
    "engine": "gpq-tiles 0.4.0",
    "levels": [
      { "simplify_tolerance_m": 4000, "thinning_factor": 4.0,
        "visibility_gate_m": 9784, "geometry_types": ["Point","Polygon"] },
      { "simplify_tolerance_m": 1000, "thinning_factor": 2.0,
        "visibility_gate_m": 2446, "geometry_types": ["Polygon"] },
      { "simplify_tolerance_m": 0,    "thinning_factor": 1.0,
        "visibility_gate_m": 0,    "geometry_types": ["Polygon"] }
    ]
  }
}
```

### 3.7 Example — `partitioning` mode

```json
{
  "version": "0.1.0",
  "mode": "partitioning",
  "canonical_level": null,
  "levels": [
    { "row_group_end": 0,  "gsd": 1000, "zoom": 6 },
    { "row_group_end": 3,  "gsd": 500,  "zoom": 7 },
    { "row_group_end": 12, "gsd": 100,  "zoom": 9 }
  ]
}
```

If the writer also emits the optional `cogp` compatibility key
(§3.1), it contains the `version` and `levels[].{row_group_end, gsd}`
subset of the above.

### 3.8 Versioning

`version` is semver. Minor bumps MAY add optional fields but MUST NOT alter
existing semantics; major bumps MAY break. Readers MUST ignore unrecognized
fields and MUST NOT treat an unsupported MAJOR version as conforming.

---

## 4. The `level` column and physical layout

### 4.1 `level` column (normative)

Every overview file MUST contain a physical column:

- **Name**: `level`
- **Type**: Parquet `INT32` (logical/annotated as a plain signed 32-bit
  integer).
- **Nullability**: NOT NULL. Every row MUST have a level value.
- **Domain**: `0 .. len(levels)-1`.

Conformance requires **column↔footer consistency**: for every row group
`r` and every row in `r`, the row's `level` value MUST equal the level
`k` whose RG span (§3.3) contains `r`. Equivalently, all rows of a given
row group MUST share one `level` value, and that value MUST match the
footer's level assignment for that RG index. A validator MUST check this
(§6, §8).

Rationale: the footer key serves footer-only readers (range-request
clients); the visible `level` column serves naive SQL readers (DuckDB,
pandas) that never parse the footer. Both MUST agree.

`level` SHOULD be dictionary/RLE-encoded (it is constant within each row
group, so this is nearly free).

### 4.2 Ordering (normative)

- Levels MUST be written coarse → fine (level 0's row groups first).
- Each level MUST end exactly on a row-group boundary; a row group MUST NOT
  contain rows from two levels.
- The mapping from level to its RG range MUST match the footer `levels`
  array (§3.3).

### 4.3 Intra-level spatial sort (SHOULD)

Within each level, features SHOULD be spatially clustered so that
row-group bbox statistics prune well — Hilbert-curve order is RECOMMENDED.

Input contract: the writer assumes gpio-optimized / Hilbert-sorted input
GeoParquet and preserves that order within each level. If the input is not
sorted, the writer MAY sort per level; if it does neither, the file is
still conformant but prunes poorly.

### 4.4 Covering column (normative for GeoParquet 1.1)

- The file MUST declare GeoParquet 1.1 `bbox` covering metadata at
  `geo.columns.<primary>.covering.bbox`.
- The covering MUST be a struct column with `xmin, ymin, xmax, ymax`
  child fields, each carrying per-row-group min/max statistics.
- Row-group statistics on the covering column are REQUIRED — they are the
  spatial-pruning index (§5.1). Producers MUST NOT strip them.

### 4.5 Encoding recommendations

- Compression: ZSTD is RECOMMENDED for all columns.
- Geometry (WKB) and the bbox covering child columns MUST NOT use
  dictionary encoding (high-cardinality; dictionary hurts and bloats).
- The `level` column SHOULD use RLE/dictionary.
- Row-group size SHOULD be tuned for bounded-latency range fetches;
  coarse levels SHOULD be kept small enough to serve as a quick overview
  (the first level SHOULD be a small number of row groups).

---

## 5. Read protocols

### 5.1 Rendering (progressive display)

Given a viewport at display scale, a renderer:

1. **Display scale → target GSD.** Compute the ground distance per screen
   pixel for the current viewport (meters/pixel), optionally biased by a
   quality factor. This is `target_gsd`.
2. **Level selection** (mode-dependent):
   - `duplicating`: select the **finest single level** whose
     `gsd >= target_gsd`; if none qualifies (zoomed in past the finest
     level), select the finest level `L-1`; if the target is coarser than
     level 0, select level 0. Fetch **only that one level band**
     (its RG range).
   - `partitioning`: select the target level `z` by the same rule, then
     fetch the **prefix** of row groups `0..levels[z].row_group_end`
     (levels accumulate).
3. **bbox pruning.** Intersect the viewport bbox with each candidate row
   group's covering min/max statistics; drop non-intersecting row groups.
4. **Byte ranges.** From the Parquet footer, map surviving row groups to
   file byte offsets and issue HTTP range requests (or object-store
   partial reads). Within fetched row groups, apply a per-feature bbox
   predicate to discard features outside the viewport.

On zoom-in in `partitioning` mode, only additional row groups are fetched;
prior reads remain valid (features are not duplicated). On zoom change in
`duplicating` mode, a different single band is fetched (prior band may be
discarded).

### 5.2 GSD / zoom mapping (Web Mercator)

For Web Mercator target zoom `z` (z0..z16), GSD in meters is:

```
gsd(z) = 40075016.69 / 1024 / 2^z
```

(40075016.69 m = Web Mercator equatorial circumference; 1024 = assumed
tile-band reference. This matches the cogp-rs convention.) Reference
values:

| z | gsd (m)   |   | z | gsd (m)  |
|---|-----------|---|---|----------|
| 0 | 39135.76  |   | 5 | 1223.0   |
| 1 | 19567.88  |   | 6 | 611.50   |
| 2 | 9783.94   |   | 7 | 305.75   |
| 3 | 4891.97   |   | 8 | 152.87   |
| 4 | 2445.98   |   | 9 | 76.44    |

For geographic (degree) CRS inputs, GSD is still expressed in meters using
`METERS_PER_DEGREE = 111320` to convert a degree-space target into meters
(§7.1). GSD is **always meters** in the footer regardless of file CRS.

### 5.3 Analysis (one-predicate rule)

The whole point of the visible `level` column is that analytical access is
a single predicate:

- `duplicating` mode: to recover the exact source dataset, filter to the
  canonical level: `WHERE level = <canonical_level>`. No other level's
  rows are canonical, so this both deduplicates and yields verbatim data.
- `partitioning` mode: **no filter needed.** The whole table (all levels)
  is the canonical dataset, each feature once. `SELECT * FROM file` returns
  the source set.

Example DuckDB SQL:

```sql
-- duplicating mode: exact source data (canonical_level = 2)
SELECT * FROM 'overview.parquet' WHERE level = 2;

-- duplicating mode: a coarse overview for a bbox at level 0
SELECT * FROM 'overview.parquet'
WHERE level = 0
  AND bbox.xmin < :xmax AND bbox.xmax > :xmin
  AND bbox.ymin < :ymax AND bbox.ymax > :ymin;

-- partitioning mode: exact source data (no level filter)
SELECT * FROM 'overview.parquet';

-- partitioning mode: coarse overview = prefix of levels
SELECT * FROM 'overview.parquet' WHERE level <= 0;
```

DuckDB with the parquet reader will use row-group `level` and `bbox`
statistics to skip row groups, so these predicates prune at the storage
layer.

---

## 6. Writer conformance and validation

### 6.1 Writer MUST

1. Produce a valid GeoParquet 1.1 file (valid `geo` metadata, primary
   geometry column, WKB or native encoding).
2. Declare the `bbox` covering column with per-RG min/max stats (§4.4).
3. Write levels coarse→fine, each ending on a row-group boundary (§4.2).
4. Write a NOT NULL `INT32` `level` column consistent with the footer
   (§4.1).
5. Write the `geo:overviews` footer key with valid JSON per §3,
   including strictly increasing `row_group_end` (final =
   `num_row_groups-1`) and strictly decreasing `gsd`.
6. In `duplicating` mode: set `mode`, set `canonical_level = L-1`, and
   guarantee canonical-level value-identity to source (§2.4).
7. In `partitioning` mode: place each feature exactly once with
   geometry verbatim, set `canonical_level = null`. (Optionally emit
   the `cogp` compatibility key per §3.1.)
8. Use ZSTD; no dictionary on geometry/bbox columns (§4.5).

### 6.2 Validation checklist (feeds `validate` subcommand, task P4)

A validator MUST check:

- [ ] File opens as valid GeoParquet 1.1 (geo metadata present, primary
      column declared).
- [ ] `geo:overviews` footer key present, parses as UTF-8 JSON,
      `version` is semver, MAJOR is supported.
- [ ] If a `cogp` compatibility key is also present, its
      `levels[].{row_group_end, gsd}` agree exactly with
      `geo:overviews` (disagreement is an error, §3.1).
- [ ] `levels` non-empty; `row_group_end` strictly increasing, 0-based,
      final == `num_row_groups - 1`.
- [ ] `gsd` strictly decreasing, all > 0.
- [ ] If `zoom` present, strictly increasing and consistent with `gsd`.
- [ ] `level` column exists, is INT32, NOT NULL, domain `0..L-1`.
- [ ] **Column↔footer consistency**: every row group's rows share one
      `level` value equal to the footer-derived level for that RG index
      (§4.1).
- [ ] Covering `bbox` struct column present with per-RG min/max stats.
- [ ] `mode` valid; if `duplicating`, `canonical_level == L-1`; if
      `partitioning`, `canonical_level` is null/absent.
- [ ] No dictionary encoding on geometry/bbox columns (SHOULD warn, not
      fail).
- [ ] Antimeridian: no geometry bbox spans the full lng range in a way
      that defeats pruning (§7.2) (SHOULD warn).

A validator MAY additionally compute quality metrics (row-group touch
counts per viewport, prefix latency, per-level feature/vertex counts,
clustering quality). These are informative.

Semantic correctness of the generalization (is level 0 actually a good
coarse rendering?) is a **producer responsibility**; the validator checks
structure, not cartographic quality.

---

## 7. Constraints and edge cases

### 7.1 CRS handling

- GSD is ALWAYS meters. For Web Mercator (EPSG:3857) data, use §5.2
  directly. For geographic (EPSG:4326) data, convert degree tolerances to
  meters with `METERS_PER_DEGREE = 111320` (equatorial degree length) when
  deriving/comparing GSD.
- The file's declared CRS (in `geo` metadata) is authoritative for the
  geometry coordinates; the meter-denominated GSD is an independent
  rendering hint and does NOT reproject geometry.
- Latitude distortion: `METERS_PER_DEGREE = 111320` is an equatorial
  approximation. This is acceptable for level selection (a coarse
  heuristic) but producers SHOULD document that high-latitude datasets see
  GSD/scale skew.

> OPEN QUESTION Q3: Should we mandate that overview files be in a single,
> known CRS (recommend EPSG:3857 or EPSG:4326) to make GSD unambiguous, or
> allow arbitrary projected CRS with meter units? Recommendation: allow
> EPSG:4326 and EPSG:3857 for v0.1 (the two COGP handles), defer arbitrary
> projected CRS. See §11.

### 7.2 Antimeridian

- Geometries MUST NOT cross the antimeridian in a way that breaks bbox
  pruning (a feature whose bbox spans ~360° of longitude defeats the
  covering index). Producers SHOULD split such geometries before writing.
- This mirrors COGP's prohibition and is REQUIRED for both modes.

### 7.3 Empty levels

- A level MUST contain at least one row group and at least one row. Empty
  levels are NOT allowed (they would violate the strictly-increasing
  `row_group_end` invariant and waste a band).
- If generalization would empty a level (e.g. every feature dropped below
  the visibility gate), the writer MUST either merge it into the adjacent
  coarser level or omit it entirely (renumbering levels).

### 7.4 Single-level degenerate files

- A file with `len(levels) == 1` is conformant. It is an ordinary
  GeoParquet file plus a `geo:overviews` key and a constant
  `level = 0` column. In
  `duplicating` mode `canonical_level = 0`. This is the degenerate "no
  overviews" case and is explicitly allowed.

### 7.5 Geometry types per level

- Generalization MAY change geometry type at coarser levels: a polygon MAY
  collapse to a point, a multipolygon MAY drop to a single polygon, etc.
  **Recommendation: allow it.** Coarse levels commonly render small
  polygons as points.
- If any level's geometry type differs from the primary declared type, the
  file MUST list the **union** of geometry types actually present. Per
  GeoParquet 1.1, `geo.columns.<primary>.geometry_types` MUST enumerate all
  types present anywhere in the column (e.g. `["Point","Polygon"]`).
- The optional `generalization.levels[].geometry_types` block (§3.5) SHOULD
  record the per-level union for tooling that wants to pick a renderer per
  band.

> OPEN QUESTION Q4: Should type collapse be opt-in (default: preserve type,
> drop-below-gate instead of collapse) to avoid surprising renderers that
> switch geometry type mid-zoom? Recommendation: opt-in collapse, default
> preserve-or-drop; document via `generalization`. See §11.

---

## 8. Non-goals (v0.1)

This spec explicitly does NOT address:

- **Topology preservation across features** (shared borders staying
  coincident after independent simplification). Each feature is
  generalized independently; slivers/gaps at coarse levels are expected.
- **Cross-level feature identity / joins.** A feature at level `k` is not
  guaranteed to map to a specific feature at level `k+1`.
- **In-place updates.** Overview files are treated as immutable build
  artifacts; editing means regenerating.
- **Multi-layer / multi-dataset files.** One dataset per file. Layering is
  an application concern.
- **New geometry encodings, CRS models, or tile-matrix-set semantics.**
- **Mandating a specific thinning/simplification algorithm.** The spec
  constrains layout and metadata; the generalization engine is
  implementation-defined (gpq-tiles supplies one).
- **Attribute aggregation** (summing/averaging attributes of dropped
  features). Deferred to a later `generalization` v0.2.

---

## 9. Worked example (3-level `duplicating` file)

A tiny buildings dataset, generalized into 3 levels at z2/z4/z6, written
`duplicating`. 15 row groups total.

### 9.1 Footer `geo:overviews` JSON

```json
{
  "version": "0.1.0",
  "mode": "duplicating",
  "canonical_level": 2,
  "levels": [
    { "row_group_end": 1,  "gsd": 9783.94, "zoom": 2 },
    { "row_group_end": 5,  "gsd": 2445.98, "zoom": 4 },
    { "row_group_end": 14, "gsd": 611.50,  "zoom": 6 }
  ]
}
```

### 9.2 Row-group table

| level | zoom | RG indices | rows/RG (approx) | bbox extent (lng/lat)      |
|-------|------|------------|------------------|----------------------------|
| 0     | 2    | 0–1        | ~200             | full dataset, heavily thinned + simplified |
| 1     | 4    | 2–5        | ~500             | full dataset, moderately generalized       |
| 2     | 6    | 6–14       | ~800             | full dataset, canonical (verbatim source)  |

Each level's row groups are Hilbert-sorted, so RG-level bbox extents form a
spatial tiling of the full dataset extent (e.g. RG 6 covers the
south-west quadrant, RG 14 the north-east, etc.). Level 0 (`row_group_end`
= 1) is the small quick-overview band; level 2 ends at RG 14 =
`num_row_groups - 1`.

### 9.3 Queries a z2 renderer issues

Target zoom 2 → `target_gsd ≈ 9783.94` → `duplicating` selects level 0
(finest level with `gsd >= target_gsd`). Fetch band = RG 0–1, pruned by
viewport bbox:

```sql
-- Renderer at z2, viewport bbox (minx,miny,maxx,maxy)
SELECT geometry, name
FROM 'buildings_overview.parquet'
WHERE level = 0
  AND bbox.xmin < :maxx AND bbox.xmax > :minx
  AND bbox.ymin < :maxy AND bbox.ymax > :miny;
```

Over HTTP the client would instead read the footer, map level 0 →
RG 0–1 byte offsets, prune RG 0/1 by covering stats, and range-request
only the surviving row groups.

### 9.4 Query an analyst issues

Recover the exact source dataset (canonical level = 2):

```sql
SELECT * FROM 'buildings_overview.parquet' WHERE level = 2;
-- row count == source row count; geometry + attributes verbatim
```

---

## 10. Forward compatibility: GeoParquet 2.0 / native GEOMETRY

GeoParquet 2.0 introduces a Parquet-native `GEOMETRY`/`GEOGRAPHY` logical
type with native geometry statistics (min/max bbox carried by Parquet
itself), removing the need for a separate covering column.

Migration plan:

1. **Covering → native stats.** In a 2.0 overview file, the `bbox` covering
   struct column (§4.4) is REPLACED by native geometry column statistics.
   All spatial-pruning language in §5.1 applies unchanged, sourcing min/max
   from the native geometry column's Parquet statistics instead of the
   covering column. Producers targeting 2.0 SHOULD omit the covering column.
2. **Footer key unchanged.** The `geo:overviews` key and its JSON
   schema (§3) are encoding-agnostic and carry over verbatim
   (`row_group_end`, `gsd`, `mode`, `canonical_level`, `zoom`) —
   until/unless the object is merged into the `geo` metadata by the
   official spec process, at which point it is a key rename only.
3. **`level` column unchanged.** Still an INT32 NOT NULL column.
4. **Version bump.** A 2.0 overview file uses `version` MINOR/MAJOR bump
   and declares GeoParquet 2.0 in its `geo` metadata; validators MUST
   accept native stats in place of the covering column when the file
   declares 2.0.
5. **Dual-target period.** During transition, a producer MAY emit BOTH the
   covering column AND native stats (redundant but maximally compatible).

> OPEN QUESTION Q5: For 2.0, do we keep emitting the covering column for a
> deprecation window, or hard-switch? Recommendation: emit both during
> transition (belt and suspenders), drop covering once 2.0 reader support
> is widespread. See §11.

---

## 11. Open questions — ALL RESOLVED (human review 2026-07-02)

- **Q1 (§2.4) APPROVED:** canonical fidelity is **set/value equality
  only**, not row order (Hilbert sort reorders rows within levels).
- **Q2 (§3.1) APPROVED:** incubation key is **`geo:overviews`**,
  designed for verbatim merger into the `geo` metadata via the
  official GeoParquet spec process; optional `cogp` compatibility key
  emitted only behind an explicit writer flag, **default off**. Key
  name may still be refined with the GeoParquet spec maintainers —
  implementations MUST keep it a single named constant.
- **Q3 (§7.1) APPROVED:** v0.1 restricts CRS to **EPSG:4326 and
  EPSG:3857**; arbitrary projected-meter CRS deferred.
- **Q4 (§7.5) APPROVED:** geometry-type collapse is **opt-in,
  default off** (default: preserve-type-or-drop).
- **Q5 (§10) APPROVED:** GeoParquet 2.0 files **dual-emit** the bbox
  covering column alongside native geometry stats during transition.
- **Q6 (new)** ~~`gsd(z)` uses the constant `1024`; confirm this matches the
  cogp-rs reference exactly.~~ **RESOLVED (verified against cogp-rs
  src/convert.rs, 2026-07-02):** cogp-rs auto-derives
  `gsd(i) = 40_075_016 / (base · 2^i)` with default `base = 1024`
  (chosen as ~4× a 256-px tile so sub-pixel features drop; the base is
  a CLI-configurable knob, and `--gsd` overrides derivation entirely).
  §5.2 table is correct as written.
- **Q7 APPROVED:** `mode` stays **SHOULD with documented
  `partitioning` default** (the safe reader assumption), not REQUIRED.
```
