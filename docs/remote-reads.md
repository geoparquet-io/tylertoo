# Reading Overviews from Object Storage with DuckDB

Overview GeoParquet is designed to be queried in place over HTTP range
requests: a viewport query touches **0.14–6.5 % of the file**, whatever
the file size (measured against real S3 in
[`benchmarks/overview/RESULTS.md` §2b](https://github.com/geoparquet-io/gpq-tiles/blob/main/benchmarks/overview/RESULTS.md)).
This page is the evidence-based DuckDB client recipe for that read
path: the one-time secret setup, the settings that measurably help, the
ones that don't, and what a warm session actually behaves like.

All numbers below are from the issue
[#203](https://github.com/geoparquet-io/gpq-tiles/issues/203) knob
sweep (DuckDB v1.4.1, S3 `us-east-2`, 3-run medians on the same bucket,
viewports, and harness as the §2b baseline; raw data in
[`benchmarks/overview/duckdb_knobs_results.json`](https://github.com/geoparquet-io/gpq-tiles/blob/main/benchmarks/overview/duckdb_knobs_results.json),
harness `bench_duckdb_knobs.py`).

## One-time setup

```sql
INSTALL httpfs;
LOAD httpfs;

-- Private buckets: one secret, credentials from your AWS config
CREATE SECRET (
    TYPE s3,
    PROVIDER credential_chain,
    PROFILE 'my-profile',      -- omit to use the default chain
    REGION  'us-east-2'        -- set it: avoids a redirect round trip
);
```

Public buckets / plain HTTPS need no secret at all —
`read_parquet('https://…')` just works. Setting `REGION` explicitly
matters: with the wrong (or defaulted) region every request can pay an
extra 301-redirect round trip.

## Recommended session settings

```sql
SET enable_http_metadata_cache = true;  -- cache HEAD results across queries
SET parquet_metadata_cache = true;      -- cache parsed parquet footers
-- enable_external_file_cache defaults to true; leave it on.
-- http_keep_alive defaults to true; leave it on.
-- Leave parquet prefetching alone (defaults are right).
```

That is the whole recipe. The rest of this section is the evidence.

### What each knob measurably did

Sweep: two datasets (79 MB points, 343 MB polygons), three viewports
each (world/regional/street), cold = fresh process, 3-run medians.

| knob | cold effect | session effect | verdict |
|---|---|---|---|
| `enable_external_file_cache` (default ON) | none (nothing cached yet) | **repeat viewport: 0 requests, 10–84 ms** (was 1,860–3,080 ms with it off); adjacent pan fetches only uncached row groups | the knob that matters; leave ON |
| `enable_http_metadata_cache` (default OFF) | none (±3 %) | skips per-query HEADs in a session (−1 request/query); pairs with the file cache | turn ON |
| `parquet_metadata_cache` (default OFF) | none (±3 %) | skips footer re-fetch/re-parse on later queries against the same file | turn ON |
| `http_keep_alive` (default ON) | turning it OFF ~**doubled** every cold wall (1.9 s → 4.1 s NYC world; 3.2 s → 6.6 s Moldova world) | same | leave ON (default already right) |
| `disable_parquet_prefetching` | fewer, larger requests (39 → 19, NYC street) but **2.3–3.6× the bytes** (4.9 → 15.3 MB); wall within noise | — | leave OFF |
| `prefetch_all_parquet_files` | no measurable change on these single-file viewport queries | — | leave OFF |
| `threads` (16 → 64) | no measurable change (7–47 requests/viewport don't saturate 16 threads) | — | leave default |

Honest summary: **no client knob makes the cold query materially
faster** — cold wall is TLS + footer + data round trips to the bucket
(~2–3 s from a residential link to `us-east-2`), and DuckDB's defaults
(keep-alive, prefetch coalescing) are already right. What the knobs buy
you is the *session*: metadata caches plus the (default-on) external
file cache turn repeat and overlapping viewports from seconds into
milliseconds.

### Cold vs warm-session behavior

What to expect, using the 343 MB polygons file as the example
(medians; the small file behaves the same, scaled down):

| query | requests | bytes | wall |
|---|---|---|---|
| cold viewport (fresh process) | 11–47 | 1.9–20.7 MB | 2.7–3.2 s |
| exact repeat, recommended settings | **0** | **0** | **16–84 ms** |
| adjacent pan (bbox shifted one width) | **0** (served from cache) | 0 | **9–46 ms** |

The Moldova pans landing at 0 requests is not a fluke: the row groups
fetched for the first viewport spatially overlap the shifted one
(Hilbert-clustered data), so the cache absorbs the pan. Where a pan
does need new data (the NYC dataset), the recommended settings still
fetch only the uncached remainder — street pan: 11 requests / 1.5 MB /
1.36 s, vs 28 requests / 3.8 MB / 1.97 s with the data cache off.

A note on the §2b baseline tables: that benchmark deliberately ran
DuckDB with the external file (data) cache **off**, so its "warm"
numbers stay symmetric with a cacheless PMTiles reader — a fairness
device, not a recommendation. A real user should run with the cache on
(the default) and gets the session behavior above, not §2b's
warm-equals-cold-minus-metadata numbers.

If you need lower cold latency than a generic SQL client can give, the
format supports it: a purpose-built reader gets footer-cached pans of
130–300 ms (see the latency-floor section of `RESULTS.md` §2b and
`benchmarks/overview/parallel_reader.py`).

## The viewport query (read protocol)

An overview file is plain GeoParquet plus a `level` column and a
`geo:overviews` footer key. The whole read protocol is: pick a level,
then bbox-filter it (spec §5.1 in `context/OVERVIEWS_SPEC.md`).

**1. Inspect the levels** (footer-only — a few KB even on a
multi-hundred-MB object):

```sql
WITH meta AS (
    SELECT decode(value)::JSON AS ov
    FROM parquet_kv_metadata('s3://bucket/overviews.parquet')
    WHERE decode(key) = 'geo:overviews'
), lv AS (
    SELECT i - 1                                   AS level,
           (ov->'levels'->(i - 1)->>'gsd')::DOUBLE AS gsd_m,
           (ov->'levels'->(i - 1)->>'zoom')::INT   AS zoom
    FROM meta,
         generate_series(1, json_array_length(ov->'levels')::BIGINT) t(i)
)
SELECT * FROM lv;
```

**2. Pick the level for a target Web Mercator zoom `z`** — the finest
level whose GSD is at least the display GSD (spec §5.2). Replace the
final `SELECT` above with:

```sql
SELECT max(level) AS level_for_z
FROM lv
WHERE gsd_m >= 40075016.69 / 1024 / pow(2, {z});
```

(If no level qualifies you are zoomed past the finest level: use
`json_array_length(ov->'levels') - 1`.)

**3. The viewport query** — one level band + bbox overlap; DuckDB
prunes row groups on the `level` and `bbox` column statistics and
range-reads only the survivors:

```sql
SELECT *
FROM read_parquet('s3://bucket/overviews.parquet')
WHERE level = {k}
  AND bbox.xmin <= {xmax} AND bbox.xmax >= {xmin}
  AND bbox.ymin <= {ymax} AND bbox.ymax >= {ymin};
```

For exact (non-generalized) data instead of a rendering level, filter
to the canonical level (`ov->>'canonical_level'`) in `duplicating`
mode, or read the whole table in `partitioning` mode — see spec §5.3.

Producer-side layout knobs that affect this read path
(`--row-group-size`, `--full-column-stats`) are covered in
[Advanced Usage](advanced-usage.md#output-layout-for-remote-reads).
