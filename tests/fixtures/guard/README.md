# Guard fixtures

Inputs for the **golden tile guard** (`crates/core/tests/convert_guard_golden.rs`,
issue #558) — the check that fails when a change moves rendered tile geometry.

These are deliberately **not** in `tests/fixtures/realdata/`. That directory is
routed through git-lfs by `.gitattributes` and distributed as the `fixtures-v1`
release, so a working copy or CI job can legitimately hold a 130-byte pointer
instead of the fixture. A guard whose input can be absent is a guard that can
quietly not run — the #369 failure mode, and #558 is what it costs. So guard
fixtures are committed as **plain git blobs**, small enough to stay that way,
and `tests/support/fixture.rs::guard()` panics rather than skipping if one is
unusable.

Keep anything added here under ~2 MB. If a guard ever needs an input too big for
a plain blob, that is a signal the guard is testing too much at once, not a
reason to move it to lfs.

## `br-clip-divergence.parquet`

| | |
|---|---|
| Features | 3,443 real Brazil field-boundary polygons |
| Size | 908 KB (zstd) |
| Format | GeoParquet 1.0, WKB geometry, single row group, Hilbert-ordered |
| Bbox | -50.958, -14.662 → -43.177, -8.349 |
| Properties | `area` (float), `confidence` (float), `country`, `state` |

### Provenance

A minimized subset of an 82,714-feature Brazil 2025 field-boundary extract that
was **verified to produce different tile bytes under i_overlay 8 vs i_overlay 9**
on a full z0–z13 build — the divergence reported in #558. The subset keeps
**every** feature whose bbox intersects one of three 0.2°-square windows centred
on the three locations where that A/B actually diverged:

| window centre |
|---|
| -43.3004, -8.5125 |
| -44.2621, -14.2955 |
| -50.8011, -14.534 |

Whole windows, not a sample: thinning inside a window would change which
polygon wins a cell and could drop the very feature that moved.

### Discrimination (verified)

Re-verify by rolling our direct dependency back and running the guard:

```bash
# crates/core/Cargo.toml: i_overlay = "=8.1.2"
cargo update -p i_overlay@9.0.0 --precise 8.1.2   # also i_float 5 -> 4.1.0, i_shape 5 -> 4.0.0
cargo test --release -p tylertoo-core --test convert_guard_golden
```

It compiles unchanged and fails with **4 of 384 guarded tiles differing**:
`8/96/138` and `8/97/134`, in both the `defaults` and `ioverlay` cases.

The margin is thin, stated plainly: only two z8 tiles discriminate this bump
(the full Brazil A/B moved 18 tiles across z5–z8), and the `ioverlay` case
caught the same tiles as `defaults` here. Widening the fixture around more of
the A/B's divergent sites would harden it; the source extract is not in the
repo, so that is optional future work.

### Zoom coverage

The build requests z0–z13, but this fixture emits **no tiles at z0–z2** — the
golden starts at `3/3/4`. A change confined to z0–z2 is invisible to this guard.

### Extraction

```sql
INSTALL spatial; LOAD spatial;
COPY (
  WITH b AS (
    SELECT *, ST_XMin(geometry) xmin, ST_XMax(geometry) xmax,
              ST_YMin(geometry) ymin, ST_YMax(geometry) ymax
    FROM read_parquet('br-divergence-fixture.parquet')
  )
  SELECT area, confidence, country, state, geometry FROM b
  WHERE (xmin <= -43.2004 AND xmax >= -43.4004 AND ymin <=  -8.4125 AND ymax >=  -8.6125)
     OR (xmin <= -44.1621 AND xmax >= -44.3621 AND ymin <= -14.1955 AND ymax >= -14.3955)
     OR (xmin <= -50.7011 AND xmax >= -50.9011 AND ymin <= -14.4340 AND ymax >= -14.6340)
  ORDER BY ST_Hilbert(geometry, ST_Extent(ST_MakeEnvelope(-51.3, -15.0, -42.8, -8.0)))
) TO 'br-clip-divergence.parquet' (FORMAT parquet, COMPRESSION zstd);
```

The source extract is GeoParquet 2.0 (native Parquet `GEOMETRY` logical type);
DuckDB 1.4.1's writer emits GeoParquet 1.0 WKB, so the committed subset is 1.0.
That is immaterial to what this fixture guards: the encoding affects the read
path, and the i_overlay divergence is in clipping, downstream of decode. For
real-world preprocessing (reprojection, row-group sizing, Hilbert ordering)
prefer [`gpio`](https://github.com/cholmes/geoparquet-io) over hand-written SQL.

## `br-clip-divergence.golden.txt`

The committed golden: one line per guarded tile,
`<case> <z>/<x>/<y> <decompressed bytes> <xxh3-64 of the MVT body>`, sorted.
Digests rather than the archive bytes so a drift shows up as a readable diff
naming the tiles that moved, and of the *decompressed* body so a gzip encoder
change cannot masquerade as a geometry change.

Regenerate only when an output change is intended:

```bash
TYLERTOO_UPDATE_GOLDEN=1 cargo test -p tylertoo-core --test convert_guard_golden
```

Only the exact value `1` regenerates. It rewrites the file and then fails, so a regeneration can never be mistaken for
a pass. Commit the diff **with the change that caused it**.
