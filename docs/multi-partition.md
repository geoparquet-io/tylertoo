# Multi-Partition Input

Real-world GeoParquet datasets frequently arrive as a *set* of parquet
files — Hive/Spark `part-*.parquet` directories, Overture-style
partitions — rather than one file. The `overview` and `tiles`
subcommands (and the Python `overview()` function) accept such sets
directly and read them as one logical dataset:

```bash
# A directory of partitions (recursive)
tylertoo overview parts/ overviews.parquet

# A glob pattern (quote it so the shell doesn't expand it)
tylertoo tiles "parts/**/*.parquet" tiles.pmtiles

# An s3:// or gs:// prefix (note the trailing slash)
tylertoo overview s3://bucket/dataset/ overviews.parquet

# An explicit ordered manifest
tylertoo overview --files-from manifest.txt overviews.parquet
```

`validate`, `decode`, and `export-pmtiles` are single-file
subcommands: given a directory, glob, remote prefix, or
`--files-from`, they fail with a one-line error pointing back at
`overview`/`tiles`.

## How an input string is resolved

| input shape | resolution |
|---|---|
| existing local file | single file (the historical behavior) |
| existing local directory | recursive collection of `.parquet` files |
| string containing `*`, `?`, or `[` | glob expansion, filtered to `.parquet` files |
| `s3://`/`gs://` URL ending in `/` | native object listing under the prefix |
| `s3://`/`gs://` URL not ending in `/` | one remote object (extension-less presigned/API URLs included) |
| `https://` URL ending in `/` | error — generic HTTP has no listing API; use `--files-from` |
| `--files-from <PATH>` | explicit ordered manifest (see below) |

The trailing slash is what makes a remote URL a prefix — query string
and fragment are stripped before the check, so
`s3://bucket/dataset/?list-type=2` is still a prefix. A resolution
that finds exactly one file collapses to the single-file path; one
that finds none fails with `no .parquet files found in input ...`.

## Sidecar filtering

Partitioned datasets carry non-data files; the resolvers skip them:

- **Directories** (local): only `*.parquet` files are collected, and
  any file or directory whose basename starts with `.` or `_` is
  skipped at every level — `_SUCCESS`, `.crc` files, `_temporary/`
  and `_delta_log/` trees never participate.
- **Remote prefixes**: only keys ending `.parquet` are listed;
  zero-byte objects are skipped; so is any key with a path component
  below the prefix starting with `.` or `_`.
- **Globs**: only files with a `.parquet` extension survive, but no
  hidden-name filtering is applied — a glob names its matches
  explicitly.

## Ordering guarantee

The converter reads its input several times (assignment scan, coarse
levels, canonical finest level) and keys its winner tables by global
row offset, so every pass must see the same rows in the same order.
`ConvertSource` guarantees this at resolve time:

- directory, glob, and prefix results are **sorted
  lexicographically** (by path / object key) and never reordered;
- `--files-from` manifests and Python `list[str]` inputs are
  preserved **verbatim, in entry order** — reordering the manifest
  reorders the dataset;
- rows stream in part order: part *i + 1* opens only after part *i*
  is exhausted.

## Schema and CRS validation

All partitions are validated against the first one before any data is
read (footers only, loaded with bounded concurrency):

- identical column names, types, and order;
- identical field (extension) metadata — the geometry encoding must
  match, and the error shows the first differing metadata key when it
  doesn't;
- identical detected CRS (the error names both raw CRS declarations);
- **nullability is the one permitted difference**: the exposed schema
  is the union (any part nullable ⇒ nullable).

A failure names the offending partition, e.g.
`incompatible input partition "parts/b.parquet": column "id" has type
Utf8 but the first partition has Int64 (first partition:
"parts/a.parquet")`.

## `--files-from` manifests

`tiles` and `overview` accept `--files-from <PATH> OUTPUT` in place of
the positional INPUT. The manifest format:

- one local path or remote URL per line (entries are trimmed);
- blank lines are skipped, as are comment lines whose first non-space
  character is `#`;
- line order is preserved verbatim — it defines the dataset row order;
- each line must name a single `.parquet` file or object; directories,
  globs, and prefixes are not expanded;
- local and remote entries may be mixed;
- a local entry that is not an existing file fails up front, naming
  the manifest line and path.

```text
# ordered parts (this order defines the row order)
/data/local-part-0.parquet
s3://bucket/dataset/part-1.parquet
https://example.com/download/part-2.parquet
```

Remote manifest entries connect in parallel (bounded at 8 concurrent
connects), with the manifest order preserved in the result.

This is also the workaround for the `https://` limitation: generic
HTTP servers expose no listing API, so an `https://.../` prefix is
rejected — list the object URLs explicitly in a manifest instead.

In Python, pass a `list[str]` to `overview()` for the same explicit
ordered-parts shape:

```python
from tylertoo import overview

overview(["part-0.parquet", "s3://bucket/part-1.parquet"], "out.parquet")
```

## Default layer names

When `tiles` (or Python `convert()`) derives the PMTiles layer name
from the input, multi-partition shapes generalize the single-file
"file stem" rule:

| input | default layer name |
|---|---|
| `data/buildings.parquet` | `buildings` |
| `data/nyc_buildings/` (directory) | `nyc_buildings` |
| `data/parts/*.parquet` (glob) | `parts` (last wildcard-free segment) |
| `s3://bucket/datasets/roads/` (prefix) | `roads` |
| `--files-from portland-roads.txt` | `portland-roads` (manifest stem) |

Pass `--layer-name` (Python: `layer_name=`) to override.

## Remote behavior notes

- All parts of an `s3://`/`gs://` prefix share one object-store
  instance, so credentials are resolved once per bucket; object sizes
  come from the listing (no per-part HEAD requests).
- `--bbox` row-group pruning applies per part; a part with no
  intersecting row groups is never opened at all.
- The standard `AWS_*` environment variables are honored for
  S3-compatible endpoints (e.g. `AWS_ENDPOINT_URL`, `AWS_REGION`,
  and `AWS_SKIP_SIGNATURE=true` for endpoints that serve anonymous,
  unsigned requests); a signature/authorization failure against a
  custom endpoint says so in the error text.
- Fetch behavior, caching, and the disk spill are described in
  [Remote Reads](remote-reads.md).
