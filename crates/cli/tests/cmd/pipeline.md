Every subcommand a reader chains together, run in one sandbox over
`tests/fixtures/properties/overture-names.parquet` (20 points; its `raw`
column has no MVT encoding, so the "not exported" note shows too).
`pipeline.in/` holds a copy of it. Durations print as `[..]`.

The bare form is `tiles`. `--keep-overview` names the intermediate file,
which otherwise gets a random temp path.

```console
$ tylertoo overture-names.parquet names.pmtiles --max-zoom 6 --keep-overview names-ov.parquet
  intermediate overview: names-ov.parquet ([..], retained)
✓ Converted overture-names.parquet → names.pmtiles
  8 tiles across z0..z6 in [..]s
  1 property column not exported (no MVT encoding for the type): "raw" (Binary)

```

The same conversion in two steps: build the overview file, check it, export it.

```console
$ tylertoo overview overture-names.parquet ov.parquet --max-zoom 6

✓ Overview overture-names.parquet → ov.parquet  (Duplicating mode)
  20 input features → 41 rows across 7 levels in [..]s
  lvl        gsd(m)    features    vertices         bytes
    0      39135.76           1           1  [..]
    1      19567.88           2           2  [..]
    2       9783.94           3           3  [..]
    3       4891.97           3           3  [..]
    4       2445.98           5           5  [..]
    5       1222.99           7           7  [..]
    6        611.50          20          20  [..]

```

```console
$ tylertoo validate ov.parquet
Validating ov.parquet
  [PASS] geoparquet_geo_metadata: 'geo' metadata present with geometry column(s)
  [PASS] geoparquet_covering_declared: bbox covering declared
  [PASS] overviews_key_present: 'geo:overviews' present and parses
  [PASS] overviews_version: version 0.2.0 (MAJOR 0 supported)
  [PASS] overviews_structure: levels/gsd/zoom/canonical invariants satisfied
  [PASS] mode_canonical: duplicating: canonical_level == L-1
  [PASS] level_column: 'level' is INT32 NOT NULL
  [PASS] level_footer_consistency: every row group's level stats match the footer
  [PASS] coalesce_mode: coalescing metadata on a duplicating-mode file
  [PASS] coalesce_count_column: "coalesced_count" is INT32 NOT NULL
  [PASS] coalesce_count_values: coalesced_count >= 1 everywhere; canonical level all 1
  [PASS] covering_stats: all row groups carry covering min/max statistics

✓ valid overview file (12 checks passed)

```

```console
$ tylertoo export-pmtiles ov.parquet out.pmtiles
Exporting ov.parquet → out.pmtiles
  mode=duplicating zooms z0..z6
  z0  (level 0):       1 tiles,         1 features
  z1  (level 1):       1 tiles,         2 features
  z2  (level 2):       1 tiles,         3 features
  z3  (level 3):       2 tiles,         6 features
  z4  (level 4):       1 tiles,         5 features
  z5  (level 5):       1 tiles,         7 features
  z6  (level 6):       1 tiles,        20 features

✓ 8 tiles, 44 features, 0 oversized tiles in [..]s
  1 property column not exported (no MVT encoding for the type): "raw" (Binary)

```

```console
$ tylertoo stats out.pmtiles
z  tiles  total  mean  p50  p99  max
0      1    161   161  161  161  161
1      1    200   200  200  200  200
2      1    227   227  227  227  227
3      2    457   228  228  229  229
4      1    282   282  282  282  282
5      1    333   333  333  333  333
6      1    621   621  621  621  621

Largest 8 tile(s):
z   x   y  bytes
6  10  24    621
5   5  12    333
4   2   6    282
3   1   2    229
3   1   3    228
2   0   1    227
1   0   0    200
0   0   0    161

```

```console
$ tylertoo decode out.pmtiles decoded.parquet --zoom 6
Decoding out.pmtiles → decoded.parquet
  zooms z6..z6, layers: overview

✓ 20 features from 1 tiles (0 skipped as degenerate) in [..]s
  note: output is the tiled representation (simplified, clipped, duplicated across zooms); see `tylertoo decode --help`

```

Merging a single archive is allowed, with a note on stderr.

```console
$ tylertoo merge merged.pmtiles out.pmtiles
note: merging a single archive just rewrites it
Merging 1 archive(s) → merged.pmtiles
  z0           1 tiles
  z1           1 tiles
  z2           1 tiles
  z3           2 tiles
  z4           1 tiles
  z5           1 tiles
  z6           1 tiles

✓ 8 tiles (8 unique) from 1 archive(s), z0..z6 in [..]s

```
