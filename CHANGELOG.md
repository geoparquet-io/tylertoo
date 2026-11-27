# Changelog

Notable changes to this project are recorded here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and releases follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## v0.9.1 (2026-11-27)

The v0.9.0 Linux wheels failed to build, so v0.9.0 is on crates.io but not
on PyPI. v0.9.1 is the first release of the 0.9 line on PyPI. The changes
since v0.8.0 are under v0.9.0.

### Fixed

- The manylinux wheels build again. The per-tile trace called
  `libc::gettid()`, a wrapper that glibc added in 2.30, and the manylinux2014
  wheels link against glibc 2.17. It now uses the raw `gettid` syscall, and
  clippy refuses `libc::gettid` (#936).

## v0.9.0 (2026-11-27)

MapLibre Tile output, cell bands, a local viewer, reprojection of projected
input, and defaults that follow tippecanoe's. Default tile output changes, the
Rust API is frozen at the crate root, and Python 3.11 is the minimum. Read
**Breaking** before upgrading.

### Breaking

- **Tile output: no default MVT feature id (#443).** Without `--feature-id`
  (Python `feature_id=`), tiles carry no MVT id; they used to carry a
  tile-local one. Archives are 1–9% smaller on the test fixtures, so the
  tile-size valve drops fewer features: the Madagascar admin fixture keeps
  155 and 149 more features at z4 and z5 (+0.05% overall), with tile counts
  unchanged. Pass `--feature-id <column>` for stable ids.
- **Default tile output follows tippecanoe.** Default archives differ from
  v0.8.0 in five ways:
  - Polygon coverages are complete by default (#751). The density budget
    thins points only, like tippecanoe's `-r`, and `--drop-lines` and
    `--drop-polygons` opt back in. `--polygon-thinning` defaults to 0.
    Polygons too small for a level are drawn as placeholder squares, and
    `--no-collapse` drops them. A tile over the size cap merges its densest
    lines and polygons into the feature kept before them (`--size-valve
    merge`). A merged feature keeps the attributes of the feature it merged
    into; `--size-valve drop` keeps every attribute exact. On Madagascar
    admin-4 at z8 the missing area falls from 48.6% to 0.23%.
  - The default size caps are 500K compressed and 2M raw (#770). The old
    default capped the raw tile at 500K, about twice as strict as
    tippecanoe's `-M`.
  - `--drop-rate` defaults to 2.5, tippecanoe's `-r` (was 1.65). Point
    tilesets come out about a quarter smaller. `--drop-rate 1.65` restores
    the old output (#882).
  - `--tile-buffer` defaults to 5 px (was 8), and lines are simplified at the
    max zoom at 0.25 × GSD. `--tile-buffer 8 --max-zoom-simplify-factor 0`
    restores the old output byte for byte (#883).
  - Line features at the max zoom no longer carry `coalesced_count: 1`.
    Styles must read a missing key as 1, for example
    `["coalesce", ["get", "coalesced_count"], 1]`.
    `--include-property coalesced_count` keeps it at every level (#884).
- **Rust API freeze (#437, #436).** The stable API is the crate root: the
  convert, export, check, tiles, pyramid and decode entry points, their
  options, reports and errors, `ErrorKind`/`Classify` and `Compression`.
  Every module is private; the CLI and Python bindings reach internals through
  `tylertoo_core::internal`, which is `#[doc(hidden)]` and has no stability
  guarantee.
  - Options, reports, errors, kinds and their struct variants are
    `#[non_exhaustive]`. Build options from `Default` with the `with_*`
    setters, and build value records (`Band`, `PropertySelection`,
    `AccumulateSpec`, `ClassRanking`, `TilesShard`, …) with their `new`.
  - No arrow, parquet, geo-types, geoarrow, object_store, prost or
    tylertoo-io type appears in a stable signature. A CI gate allows only
    `tylertoo_core`, `core`, `alloc`, `std`, `serde_core` and `serde_json`
    paths in the public-API baseline.
  - `tylertoo-io` is published to crates.io as an internal crate of
    tylertoo-core, with no stability guarantee.
- **Typed errors (#438).** `Error::PMTilesWrite` and `Error::PMTilesRead`
  are gone; archive, write and pyramid failures are `ArchiveError`,
  `WriteError` and `PyramidError`. `GeoParquetRead` and `InvalidConfig`
  carry typed reasons instead of strings, and a hostile page header has its
  own variant. `ClassRanking::new` returns `ClassRankingError` and
  `Band::parse` returns `BandSpecError` instead of `String`, with the same
  messages and error kinds. The hostile page header's CLI message now reads
  "arrow error: External error: unexpected end of file" (was "Parquet
  argument error: External: …"). One classifier, `ErrorKind`, maps every
  error to a CLI exit code and a Python exception.
  - Pyramid errors take the class of their cause: a band archive mismatch
    raises `ValueError` (was `RuntimeError`); a band whose input or memory
    budget fails raises `ValueError` or `MemoryError`; a refused band option
    exits 2 / raises `ConfigError`.
  - Archive read errors say "Failed to read PMTiles" (was "Failed to write
    PMTiles"), and the doubled prefixes on merge, pyramid-finalize and
    archive-read errors are gone.
- **Streaming option removed (#440).** `--no-streaming`, Python
  `streaming=` and `ConvertOptions::streaming` no longer exist; streaming is
  the only mode.
- **Input rejected that was accepted before:**
  - A multi-file input that mixes parts with a declared CRS and parts with
    an assumed WGS84 (#708).
  - A `geo` metadata value that is not a JSON object; the error names the
    file and suggests `gpio convert` (#708).
  - `--drop-gamma` below 1 (it was silently treated as 1) and a negative
    `--simplify-factor` (it behaved like 0). `--drop-gamma` is not checked
    under `--no-density-drop`.
  - `--include-property bbox`, which names the bbox covering and not a
    property (#663). `--exclude-property bbox` warns. A `--filter` that reads
    an excluded property fails before the run.
  - A `--filter` column named `like` or `ilike` without double quotes. Both
    are now keywords, as `in` and `is` are (#836).
- **Python 3.11 or later.** Wheels are `cp311-abi3`; Python 3.9 and 3.10
  are no longer supported.

### Added

- **MapLibre Tile output.** `--tile-format mlt` on `tiles`, `export-pmtiles`
  and `pyramid` (Python `tile_format=`) writes MLT for MapLibre GL JS 5.12+
  and MapLibre Native. Archives are 10–28% smaller than MVT on the benchmark
  data (#860). `--mlt-profile` picks the encoder, and the size caps measure
  MLT bytes. `merge` refuses to mix MVT and MLT shards. `decode` and
  `stats --layers` read MLT archives. `pyramid` also takes `--profile`.
- **Cell bands.** `tiles --band LO-HI:KIND` (Python `convert(bands=[...])`)
  sets what each zoom range shows (#859). `KIND` is `geom`, `point` or
  `square` for features, or `grid`, `h3`, `s2` or `a5` for aggregated cells:
  `--band 0-7:a5 --band 8-14:geom`. Sharded builds take cell bands, and the
  merged fleet matches an unsharded run byte for byte.
  `pyramid --band LO-HI:SYSTEM:INPUT[:LAYER]` draws cells from another input.
- **`tylertoo serve ARCHIVE...`** (Python `tylertoo.serve()`) serves
  archives on a local HTTP server with a MapLibre viewer, TileJSON, metadata,
  stats and Range reads (#865).
- **Reprojection on read.** GeoParquet input in a projected CRS is
  reprojected to EPSG:4326 as it is read, from the EPSG code its CRS declares
  (#868). Parts in different CRSs convert as one dataset. Input in EPSG:4326
  or EPSG:3857 gives byte-identical output.
- `tylertoo stats --layers` reports feature counts, bytes and geometry types
  per zoom and layer. Python `tylertoo.stats()` returns the same structure as
  `stats --json` (#837).
- `--mask PATH` on `overview` and `tiles` (Python `mask=`) keeps the features
  that intersect a polygon from a GeoJSON or GeoParquet file, or from any
  object with a `.wkb` attribute (#866). The mask's bbox prunes row groups
  before any data is read.
- `--filter` takes `LIKE`, `ILIKE`, `NOT LIKE` and `NOT ILIKE` with `%` and
  `_` wildcards (#836). A `LIKE` with a literal prefix prunes row groups.
- `--cluster-categories COL` (Python `cluster_categories=`) gives each
  clustered point a count column per category of COL, for pie or
  dominant-category styles (#846).
- `--generate-variable-depth-tile-pyramid`, as in tippecanoe (#809). Export
  stops a subtree at a tile that holds the finest rows within the caps. On
  1.36M buildings the archive falls from 156 MB to 88.5 MB.
- `--coverage-fill` closes coarse-zoom holes in polygon coverages (#763). It
  is opt-in. On Madagascar admin-4 at z4 the missing area falls from 12.8% to
  0.01%.
- `--no-simplification-of-shared-nodes` (Python
  `no_simplification_of_shared_nodes=True`), tippecanoe's `-pn`, simplifies
  a border two polygons share the same way in both, so slivers close (#905).
  Auckland parcels at z12 go from 97.6% to 99.97% of the source area drawn.
  It is off by default.
- `--max-zoom-simplify-factor` sets simplification at the max zoom (#847).
  `--max-compressed-tile-size` is tippecanoe's `-M` (#751).
- `--name`, `--description` and `--attribution` on `tiles`, `export-pmtiles`,
  `pyramid` and `merge`, and on the Python functions, write PMTiles metadata
  (#746, from jimmyrocks's #669).
- `--workers N` (Python `workers=`) and `--memory-limit SIZE` (Python
  `memory_limit=`) (#921).
- `--temp-compression lz4|zstd` (Python `temp_compression=`) picks the codec
  of the temporary overview that `tiles` writes (#878).
- A global `-q/--quiet` flag. `tylertoo --help` and the CLI reference list
  the exit codes (#726).
- Python: property selection (`include_properties`, `exclude_properties`,
  `exclude_all_properties`), `verbatim=` and `entry_zoom=` on `overview()`,
  and `pyramid()` and `decode()` (#728).
- The convert report gains `crs_assumed_wgs84`, set when an input declares no
  CRS. An unknown `geo.version` major warns once (#704).
- The `geo:overviews` footer records the entry-zoom ladder in
  `generalization.entry_zoom` (#662).
- The `tiles_over_limit` diagnostic counts the tiles left over the size cap,
  lists the ten largest, counts the ones that hold merged features, and names
  `--size-valve drop` (#847, #886).
- Environment overrides for remote staging and HTTP clients:
  `TYLERTOO_STAGING_CONCURRENCY`, `TYLERTOO_STAGING_MEM_BUDGET_BYTES`,
  `TYLERTOO_STAGING_COALESCE_BYTES` and `TYLERTOO_HTTP_<KEY>` (#700, #729).
- Pass 1 warns when the input rows are not sorted by location (#768) and
  names the `gpio sort hilbert` command for the input: on unsorted input,
  merged features join unrelated geometry and tile edges show more seams.
- `--preset tippecanoe` (Python `preset="tippecanoe"`) on `tiles`, `overview`
  and `export-pmtiles` starts from tippecanoe's drop rate, gamma,
  simplification, tile buffer, size caps, size valve and one valve threshold
  per zoom (`--valve-scope zoom`, #768) (#822). Options you pass still win.
  A data shard refuses `zoom` before it reads anything; pass
  `--valve-scope smooth` there. `docs/guides/tippecanoe.md` lists every value and what still
  differs.
- `tiles --layer-column COLUMN` and Python `convert(layer_column=...)` put
  each feature in the layer named by its value in COLUMN, and null or blank
  values in `_unknown` (#821). This is tippecanoe's `tippecanoe.layer`. The
  column is not written as a property. Each layer is tiled on its own ladder
  and the layers are merged. The size caps apply to the merged tile: an
  over-cap join is refitted across layers. A join too big to refit in
  memory stays over the cap, with a warning (#838).
  `--shard` and `--keep-overview` are refused with it (#839). Rust callers
  that match `TilesOutcome` or `TilesError` get new `Layered`,
  `LayerColumn`, `Layer` and `Merge` arms.
- `pyramid` archives whose bands share a zoom now carry a `tilestats` entry
  per layer. The size caps apply to a shared zoom's merged tile: a join of
  GeoParquet bands is refitted across layers, and a join with a pre-tiled
  archive band loses features. A join that cannot shrink, or is too big to
  refit in memory, stays over the cap with a warning (#838).
- `--max-zoom auto:N` and Python `max_zoom="auto:N"` cap the automatic zoom
  pick at N (#793). Plain `auto` keeps its z16 ceiling. Rust callers that
  match `MaxZoom` exhaustively need the new `AutoCapped` arm.
- The convert report gains `geo_version`:
  `{"status": "known" | "unknown" | "missing" | "mixed", "version": ...}`,
  covering every input part (#708).
- The Python wheel runs the full CLI as `python -m tylertoo` and as a
  `tylertoo` script (#819): every subcommand, `shard-plan`, `merge`,
  `stats` and `--shard` included, with the binary's output, stderr logging
  and exit codes. The Python API now installs its `logging` bridge on its
  first call instead of at import.
- Sharded builds work with line coalescing, which is on by default (#546).
  The convert plan carries the coalesce chains the coarse job built, so a
  data shard runs no chain stage: it reads the row groups of the chains that
  reach its tiles. Its memory follows its own share of the data (on 2.8M
  Overture segments at 64 shards, about 1 GB per shard against 4 GB when
  each shard rebuilt the chains). Plans saved by earlier builds must be
  re-created with `--save-plan`. Tile bodies match a single-machine
  build byte for byte, except for over-cap tiles under the default
  `--size-valve merge` when a shard prunes row groups (#854; pass
  `--size-valve drop` for an exact match). `--no-coalesce-lines` is no longer
  needed, and `InvalidOption::ShardWithCoalescedLines` is gone.

### Changed

- **Tile output: the size valve's threshold reaches past the tile it fits
  (#768).** `--valve-scope` (Python `valve_scope=`) defaults to `smooth`.
  The export scan predicts each dense tile's threshold, and every tile
  around it, one that fits too, starts from that threshold faded by
  `--valve-max-step` (Python `valve_max_step=`, default 2) a tile, out to
  where it no longer thins anything (at most 8 tiles). Neighbouring tiles
  then keep at most twice each other's share of points, or merge at most
  twice less hard, so density steps down over several tiles instead of
  jumping at a tile edge. Point tiles fade under either valve. On 1.8
  million French points the 95th-percentile jump between neighbouring z3–z6
  tiles falls from 3.5–39× to 2× under `--size-valve drop` (those zooms keep
  44–81% of the points `tile` keeps), and from 4.3–41× to 2× under the
  default `merge` (40–80%). The prediction and the
  floors stay within the export's memory budget; if they cannot, the
  export warns and runs as `--valve-scope tile`. `--valve-scope tile` thins
  each tile on its own as before; `zoom` starts every tile from its zoom's
  largest threshold (not with `--tile-range`). A `tiles` shard whose export
  fades the valve now reads input up to nine pivot tiles past its range,
  not two, so it predicts the same floors: on world-wide points cut into 8
  shards at z6 the fleet reads 27% more rows; on a regional input nothing
  changes. Under `--valve-scope tile`, or with no size cap, it reads two. The default costs at most
  1.12× the wall time of `tile` on the benchmark inputs and is 2.8× faster
  on dense points.
- **Tile output: point tiles over a cap keep points by a fixed rank (#768).**
  Under every `--valve-scope`, including `tile`, a point tile thinned by
  the size valve keeps the points whose input row ranks best (the row
  number bit-reversed), not an even stride from the tile's first point.
  The points kept at one zoom are kept at the next wherever it keeps a
  larger share: on French points 93–96% of the z3 points stay at z4,
  against 5% before. Overviews record each row's input position in a new
  `input_row` column; older overviews rank by overview row, and a column of
  theirs named `input_row` stays a property.
- The Python wheel uses mimalloc as its global allocator on every platform,
  as the CLI does (#739, #792). On the 792 buildings input (median of 3),
  export takes 10.0 s against 10.9 s and peak RSS is 3.36 GB against 2.84 GB
  with the system allocator: about 8% faster for 18% more memory. Archives
  are byte-identical. A non-musl source build opts out
  with `maturin build --no-default-features`; musl always uses mimalloc.
- Rust callers pass a `TileCoord` to
  `StreamingPmtilesWriter::add_tile_precompressed` instead of separate
  zoom, x and y arguments. `Compression::from_str` is now
  `Compression::parse`; `str::parse::<Compression>()` is also available.
- Remote staging reports each operation's live concurrency, including
  later operations in the same process (#779).
- `tiles` writes its temporary overview with LZ4: the file is 1.4–1.6×
  larger and faster to write. `--temp-compression zstd` restores v0.8.0's
  codec (#878).
- `--profile auto` and `speed` use every core the process may use. `bounded`
  keeps its cap of 40 workers (#921).
- Tiles are 0.5–3% smaller (#889). A Float64 value that a float holds
  exactly is written as an MVT float, a negative integer as `sint`, and gzip
  runs on libdeflate at level 6. Decoded values do not change.
- Errors name the file they concern: a missing input, a corrupt footer, or a
  file with no geometry column (#726). A missing output directory fails
  before any input is read.
- Per-tile size-valve messages log at DEBUG. One warning counts the tiles
  still over the limit (#847).

### Fixed

- Export finds a geometry column with any name, such as Esri's `SHAPE`, from
  the `geo` metadata. An input with a second geometry column tiles the
  primary one (#676).
- The merge valve meets the size cap on dense polygon tiles (#901). Before it
  drops whole features, it re-encodes the tile at extent 2048, then 1024,
  then 512. `--size-valve drop` keeps tippecanoe's 0.8 margin after its
  first cut.
- `--drop-polygons` no longer adds the polygons it drops back as placeholder
  squares (#903).
- Placeholder squares dither on the source area at every level, so a square
  no longer survives to z0 by chance (#888). Default output changes a little.
- `--bbox` matches features across the antimeridian, and the report counts
  wrapped features (#721).
- Export stops with an error after its scan when one tile cannot fit in
  memory. It used to be OOM-killed minutes later (#713).
- The PMTiles writers compare bytes on a dedup hash hit, so a hash collision
  no longer stores the wrong tile body (#691).
- Layer and field names with `"` or `\` write valid PMTiles metadata.
  Integer covering columns no longer prune row groups the read needs (#757).
- A signed input's query string no longer appears in the Referer header of a
  redirect (#890).
- A change to `TYLERTOO_STAGING_*`, `TYLERTOO_HTTP_*` or a cloud credential
  variable takes effect on the next conversion in the same process (#864).
- `validate` reads clustered overviews whose geometries pass about 2 MB a row
  (#657).
- Hostile input gives typed errors: polygon clipping refuses non-finite or
  huge coordinates (#698), and a `--filter` statistic of `i64::MIN` or a
  dictionary page that claims billions of values no longer crashes (#899).
- `decode` prints each error cause once (#660).
- The sdist carries `NOTICE` at its root, so PyPI accepts it (#646).
- Dependency updates clear RustSec advisories in rustls, rustls-webpki,
  webpki-root-certs, zstd-sys, url and parquet (#674, #753, #786, #914).
- Pre-read disk warnings cover only staged input and staged layers; projected
  spill and intermediate sizes are info logs. Conversion rechecks space before
  pass 2 using kept per-level rows, including attribute filtering, the input's
  compression ratio with a 2x margin for the intermediate, and the profile's
  spill codec price. Framing stays raw. Pass 2 reprices each level once from
  the bytes it actually buffers; the need is the largest on one volume at any
  phase (each level's spill file is deleted after it drains), plus an output
  archive allowance of half the intermediate. Python's up-front `UserWarning`
  covers staged input and staged layers; intermediate and spill shortages are
  logged after pass 1 and when pass 2 measures its buffered levels, appear
  as `disk_shortage` report diagnostics, and `convert()` raises each as a
  `UserWarning` once the run succeeds. Recheck remedies name both CLI flags and
  Python keywords (#872).
- The CLI and Python extension immediately purge freed mimalloc pages under
  `profile=bounded`, lowering peak RSS at a throughput cost (#851). Auto and
  speed leave the allocator setting untouched.
- An `https://` input keeps its query string, so presigned S3 and Azure SAS
  URLs work; the query is shown as `?<redacted>` in logs, errors, reports
  and saved plans (#820).
- The density budget no longer empties sparse tiles (#850). Three causes,
  all fixed: the rounding leftover went to the first neighborhoods in scan
  order, so equal small shares emptied every later one; each level shared
  out only its newcomers, so rounding errors compounded level by level; and
  nothing matched tippecanoe's rule that a tile keeps its first feature
  whatever the drop rate. Now the shares are rounded as a running total in Z-order over
  a Web Mercator grid, each counts what coarser levels kept, and a tile the
  budget would empty keeps one feature. On 1.05M Overture places,
  `--preset tippecanoe` missed 298 of tippecanoe's 1,645 z12 tiles and 1,022
  of its 5,094 z13 tiles; it now misses 12 and 28, with the same per-zoom
  feature counts. Any budgeted output changes, the default included.
- `--generate-variable-depth-tile-pyramid` takes `--max-tile-size 0`, and so
  `--preset tippecanoe`, which sets it (#849). It used to refuse them, naming
  a flag the preset user never passed. With no raw cap the probe is bounded
  by the default 2M; a tile over it is drawn at the usual depth.
- A data shard at the antimeridian reads the features from across it that
  export draws into its edge tiles. Its read margin used to stop at ±180°
  (#546 review).
- `shard-plan` cuts an EPSG:3857 input by where its rows are. It used to
  read the row-group statistics, which are metres, as degrees, and put every
  row in the last shard (#546 review).
- A data shard no longer warns that a level below its pivot "became empty
  after simplification". It exports no such level, and its share of one is
  often empty (#546 review).
- Inputs with many list columns (Overture segments: 86 list leaves) are no
  longer handed out one row per batch. A decode no longer crosses a row
  group, so it is charged with one group's list values rather than two:
  Overture's 22k-row groups decode whole instead of a row at a time. A group
  whose list values alone pass the bound still decodes a row at a time, and
  the reader joins those rows back into batches within the byte bound. Each
  one-row batch kept about 260 KB of Arrow overhead per buffered output row,
  so `tiles` on a 38 MB extract passed 118 GB and was OOM-killed. It now
  peaks at 1.5 GB in 23 s (#848).
- A read whose batches hit the decode byte bound is cut into the same
  batches by one reader or by several (`--read-workers`): one row-by-row
  byte rule cuts both, so the intermediate overview file no longer depends
  on the worker count there (#858).
- A column with 8- or 16-bit dictionary keys is read in batches of no more
  rows than its key indexes, and one nested in a list in batches of no more
  list values, so a batch no longer fails with `DictionaryKeyOverflowError`
  (#857). A parallel read (`--read-workers`) joins such a column at a worker
  seam as one reader does, so one under a fixed-size list no longer panics
  there. The key type is unchanged.
- A crafted Parquet footer with an overlong varint, or a page header that
  skips past the end of the file, is refused instead of allocating gigabytes
  or spinning for minutes (#789). So is a footer whose child or row-group
  count the pre-check read differently from parquet: through a field
  written as another wire type than parquet reads it as, or a schema child
  count that parquet's i32 read makes large, or a schema nested more than
  64 levels deep, which overflowed the stack.
- A WKT projected CRS (`PROJCRS`, `PROJCS`, …) that embeds EPSG:4326 as its
  base is no longer read as WGS84 (#708).
- `TYLERTOO_AUTO_MEM_LIMIT_BYTES` set or changed after the first conversion
  in a process is honoured; only the machine probe is cached.
- No "geo metadata has no version" warning when a native GEOMETRY column
  implies GeoParquet 2.0.
- The overviews format version is independent of the tool version; the spec
  stays 0.x and readers refuse a foreign MAJOR (#441).

### Performance

Output is byte-identical unless the entry says otherwise.

- On 64 cores, default `tiles` and `export-pmtiles` runs are 29% faster
  (geometric mean), `decode` is 6× faster, and `--profile bounded` is 18%
  faster with 20% less RSS (#818). Export and pass 2 use idle cores for
  7–11% more on three datasets (#881). Nested Rayon waits no longer leave
  the pool idle (#921).
- Peak RSS falls about 25% under `auto` and 29% under `bounded`. Full
  Overture segments under `auto` go from 10.9 GB to 5.1 GB (#871). Export
  packs each feature's properties into one buffer (#897).
- List-heavy row groups decode by their excess list values. Full Overture
  segments go from 843 s to 24.6 s (#902).
- The merge valve quantizes each member once per tile. FTW Moldova export at
  `--simplify-factor 0` goes from 131 s to 16 s (#770). A dense merged group
  is swept once, so the 48M-polygon UNOSAT input finishes instead of stalling
  (#911). Export waves are sized by their own members, which is 24% faster
  with half the peak RAM on that input (#917). The valve predictor clips a
  sampled row once for all its tiles (#920).
- The pre-export scan reads the bbox covering and leaves the geometry
  unread: 8.98 s to 0.77 s on FTW Brazil MT (#737). `--max-zoom auto` reads
  the covering too (#703).
- Pass 1 reads row groups in parallel (#722) and scans geometry where it
  lies (#712). Pass 2 processes several batches at once and runs 1.46×
  faster on FTW Ukraine (#714).
- Export runs small zoom levels concurrently (#652) and interns property
  keys (#667).
- `decode` reads tiles by offset, so an archive larger than memory decodes
  (#736). The pyramid merge holds one tile per band, not an index of every
  tile (#725).
- Polygon validation pruning, a background archive writeback on Linux, and a
  parallel pass-2 drain (#878). Profile-driven fixes for the #788
  bottlenecks (#790) and a cheaper default size valve (#799).

### Internal

- One owner per shared concept, checked by clippy, cargo-deny, jscpd and
  similarity-rs (#787, #805). Export split into stage submodules (#918) and
  reads the overview file through one engine (#898). Hand-rolled code
  replaced by existing crates (#757). Fuzz targets for every outside input
  (#689). One benchmark harness with generated docs numbers (#694, #760,
  #812). A docs overhaul (#933), CI fixes, and dependency bumps.
- Mutation testing (cargo-mutants) is replaced by targeted proptest and
  Hypothesis property suites (#677).

## v0.8.0 (2026-10-06)

Sharded builds, much faster conversion and export, and a long list of
robustness fixes. Some commands now refuse to overwrite existing files; see
**Changed** before upgrading.

During development, `main` carried versions 0.8.0 to 0.11.0. None of those
were released. This is the first release after v0.7.1.

### Added

- **Sharded builds:** `tiles --shard I/N` builds one disjoint slice of a
  large job, `--tile-range` limits a build to part of the tile space, and
  `tylertoo merge` joins the shards into one archive. The merged result
  matches a single-machine build (#498, #510, #537, #541, #555).
- **Saved convert plans:** `--save-plan` writes the expensive first pass to a
  file, `--plan` reuses it, and `tiles --plan-only` writes the plan without
  exporting tiles (#505, #521, #560, #574, #600).
- **`--max-zoom auto`** picks the finest zoom from the data, similar to
  tippecanoe's `-zg`. In Python, pass `max_zoom="auto"` (#444, #573).
- **`--feature-id <COLUMN>`** sets stable MVT feature ids from a column, so
  MapLibre `setFeatureState` works across tiles and zooms (#443, #568).
- **`tylertoo stats`** reports tile sizes per zoom (count, total, mean, p50,
  p99, max), with `--largest N` and `--json` (#551, #557).
- **`pyramid --feature-order`**, and `--band` now accepts remote inputs and
  paths that contain colons (#374, #491, #598).
- **Nested properties:** struct, list and map columns reach the tiles as JSON
  strings instead of being dropped. Columns that are still skipped produce a
  warning (#434, #599).
- **`--bbox` on GeoParquet 2.0 files** skips row groups using the file's
  native geometry statistics (#497, #500).
- **Profiling:** `TYLERTOO_PROFILE_JSON` writes per-phase timers and memory
  use, including peak memory within each phase (#502, #550, #571, #627).

### Changed

- **Existing outputs are no longer overwritten by default.** `tiles`, the
  bare `tylertoo IN OUT` form, `overview`, `export-pmtiles` and `decode`
  refuse to write over an existing file unless you pass `-f`/`--force`. A
  directory at the output path is always refused (#427, #552, #595).
- **Zoom limit:** zooms above 30 are rejected up front. Before, they were
  accepted and the run hung (#371, #488).
- **Out-of-range input fails loudly:** when every feature falls outside the
  valid coordinate range (for example, meters labelled as degrees), the
  conversion fails instead of writing an empty archive. Partial drops are
  counted and reported, and warnings from the core library now reach the
  CLI (#429, #493, #553, #563).
- **Clustered PMTiles:** tiles are written in tile-id order, and the header
  min and max zoom match the tiles actually stored, so `go-pmtiles verify`
  accepts the archives (#501, #506, #516, #529, #554).
- **Pyramid bands** are checked against the tiles they really contain, not
  their declared zoom range (#495, #503, #514, #527).
- **Rust API:** `tylertoo-core` has breaking changes. The level-assignment
  and cluster functions take `&FeatureTable` instead of `&[AssignFeature]`;
  see `crates/core/api/tylertoo-core.txt` for the full surface (#543, #630).

### Fixed

- Features that cross the antimeridian keep the part past ±180°, as in
  tippecanoe (#342, #642).
- At zoom 15 and above, the top rows of tiles were lost because latitude was
  clamped at ±85.05 instead of the exact Web Mercator limit (#416, #484).
- On the musl Linux binary, large runs no longer abort from memory
  fragmentation (#480, #483).
- `--profile auto` and `--partition-wave auto` respect container memory
  limits (cgroup v1 and v2, as used by Docker, Kubernetes and Slurm) (#481,
  #485).
- Killed runs no longer leave a truncated `overview` or `decode` output in
  place of the previous file (#427, #595).
- GeometryCollections are encoded as one feature per geometry type instead of
  being dropped (#431, #592).
- Hostile or corrupt input is rejected with an error instead of a crash or
  wrong output: malformed WKB, truncated or hostile remote Parquet footers,
  hostile PMTiles archives, out-of-tile coordinates, NaN values, and
  invalid `--extent` or `--tile-buffer` values (#399, #406, #417, #428, #430,
  #432, #433, #623, #632).
- Simplifying very long lines no longer overflows the stack (#575, #578).
- Output is byte-identical across thread counts (#423, #487, #609, #610,
  #611).
- The first pass fails early with a clear message when its feature table
  cannot fit in memory, and pass-1 line memory stays within its limit (#449,
  #543, #549, #569).
- Writing more than 32,767 row groups now works (#507, #508, #509).

### Performance

- Export is about 1.75× faster (#535, #559), and checkpoints cost the same
  no matter how large the archive is (#459, #528).
- The first pass and the level assignment run in parallel (#460, #504, #534,
  #564).
- Pass-2 reads run in parallel, and spilling to disk no longer blocks (#494,
  #533, #536).
- Memory use is lower: the pass-1 feature table shrank to 42 bytes per row,
  and the density table and line-joining step use less memory (#543, #565,
  #570, #579, #581, #626).
- Zoom levels that share a simplification step compute it once (#499, #525).

### Internal

- Docs rebuilt around two tested tutorials (#639). Release builds check for
  integer overflow (#432, #589). Line clipping now uses i_overlay 9 only
  (#205, #435, #597). New regression guards, benchmarks against tippecanoe,
  sharded slow tests, dependency updates and CI work (#447, #448, #524, #558,
  #562, #566, #567, #612, #619).

## v0.7.1 (2026-09-23)

Packaging and documentation updates for v0.7.0, with easier installation and
a new quickstart. The tiling engine is unchanged.

### Added

- **Python wheels:** `pip install tylertoo` uses prebuilt wheels on Linux
  x86_64 and aarch64 (manylinux and musllinux, including Alpine), macOS Intel
  and Apple Silicon, and Windows x86_64. Each stable-ABI wheel (`abi3-py39`)
  supports CPython 3.9 and newer without source builds (#410, #473).
- **`cargo binstall tylertoo`** downloads a prebuilt release binary without
  requiring Rust or `protoc` (#411, #469).
- **60-second quickstart** in the README and at the start of the
  getting-started guide, using a downloadable 28 MB sample file. Every
  command was tested (#413, #472).
- **Community files:** `SECURITY.md` with private vulnerability reporting, a
  code of conduct, issue forms, a PR template, and issues labelled
  good-first-issue (#415, #467).

### Changed

- **Reproducible builds:** `Cargo.lock` is committed, and release binaries,
  wheels and crates.io publishes use `--locked` to build from reviewed
  dependencies. v0.7.0 was built without a lockfile (#412, #468).
- **Clearer README intro and a curated v0.7.0 changelog.** The Brazil tutorial
  now links to the 52-file remote manifest; the previously named single file
  never existed. The gpio commands no longer use a flag absent from gpio
  1.5.0 (#414, #472).

### Fixed

- **Release pipeline:** natively built wheels are smoke-tested (installed and
  imported) before publication. Manual workflow re-runs cannot upload wheels
  under an already released version (#473).

### Internal

- uv.lock version-sync hook and CI check (#466); geometry-test-data submodule
  setup documented (#464, #474); gpq-tiles tombstone runbook (#470);
  rust-toolchain action re-pin (#475); dependency and action bumps (#477,
  #478).

## v0.7.0 (2026-09-19)

Replaces the original per-tile pipeline with `geo:overviews`: build a
multi-resolution overview GeoParquet, then export tiles from it. The overview
file can be validated, queried with SQL, and re-exported.

### Added

- **`geo:overviews` GeoParquet overviews** — `tylertoo overview` embeds
  COG-style multi-resolution levels in a single valid GeoParquet file;
  `tylertoo validate` checks one against the draft spec (#168, #184, #190).
- **`tylertoo export-pmtiles`** — PMTiles v3 export from an overview file,
  with per-level progress logging and incremental checkpoints (#169, #229).
- **`tylertoo tiles`, and the bare form** — one-shot GeoParquet → PMTiles via
  overview → export, with `--keep-overview`, a configurable spill directory,
  a free-space preflight, and the same tuning options as the two-step commands
  (#251, #276, #318, #319).
- **`tylertoo decode`** — PMTiles v3 → GeoParquet with tippecanoe-decode
  semantics and `zoom` / `layer` / `mvt_id` provenance columns (#112, #206).
- **`tylertoo pyramid`** — merge several inputs with disjoint zoom ranges into
  one PMTiles archive. Bands may be GeoParquet (tiled during conversion) or
  existing PMTiles. Bands in different layers may share a zoom range (#348,
  #385, #392).
- **Remote input** — read `s3://`, `https://` and `gs://` GeoParquet over
  byte-range requests, including remote prefix listing (#210, #216, #281).
- **Multi-file input** — a local directory, a glob, an `s3://` / `gs://`
  prefix, a `--files-from` manifest (ordered, mixed local and remote), or a
  Python `list[str]` is read as one dataset, with cross-partition
  schema and CRS validation and a deterministic row order (#277, #281, #282).
- **`--spill-dir` and a free-space guard** — choose where to store the
  remote-input spill file. Before pass 1, the converter estimates its size
  and warns about potentially insufficient space or tmpfs storage (#272,
  #273).
  Conversions that would stage roughly the whole remote object also show
  equivalent download-then-convert commands up front (#267).
- **`--bbox` regional extracts** — spatial pushdown skips row groups outside
  the requested region, avoiding unnecessary reads and remote fetches
  (#102, #207).
- **`--filter` / `--where`** — SQL-WHERE-style attribute predicates with
  parquet row-group statistics pushdown, timestamp columns included
  (#315, #321).
- **Property selection** — `--include-property`, `--exclude-property` and
  `--exclude-all-properties` (tippecanoe `-y` / `-x` / `-X`), applied at scan
  time on `overview` and at encode time on `export-pmtiles` (#386, #391).
- **`--feature-order`** — set within-tile draw order to input order or a
  property, ascending or descending (#361, #366).
- **Magnitude ladder** — `--magnitude-ladder` and `--entry-zoom` set each
  feature's first zoom from an attribute (#364, #375).
- **`--verbatim`** — tile the input as given, disabling generalization at
  every level (#345, #360, #367).
- **Zoom-band representation** — `--representation "0-7:point"` draws a band as
  representative points and the rest as polygons in one archive, and
  `--collapse-square` replaces dropped tiny polygons with area-dithered
  placeholder squares (#279, #317, #322).
- **Tiny-polygon accumulator** — preserve the area of dropped polygons at
  coarse levels (#384, #394).
- **Per-level point clustering** — `--cluster` and `--accumulate-attribute`
  carry cluster winners with a `point_count` column and numeric attribute
  aggregation through the overview pipeline (#170).
- **Line coalescing** — on by default, with `--coalesce-snap`,
  `--coalesce-max-level-rows` and `--no-coalesce-lines` (#183).
- **Cascading simplification** — each level simplifies the one above it rather
  than the source; `--no-cascade` opts out (#218).
- **`--report`** — machine-readable JSON run reports from `overview`,
  `export-pmtiles` and `tiles` (#316, #318).
- **Python overview API** — `overview()`, `validate()` and `export_pmtiles()`
  bindings, with `convert()` kept as a facade over them (#185).
- **Prebuilt CLI binaries** attached to every GitHub Release — Linux x86_64
  (gnu and musl), macOS Intel and Apple Silicon, Windows x86_64 — plus
  generated release notes (#275).
- Warn during conversion about features suspected of crossing the antimeridian
  (#188, #199).

### Changed

- **Breaking: the legacy per-tile pipeline is gone.** Removed `pipeline.rs`,
  `Converter`, `TilerConfig` and their library re-exports. All conversion now
  runs overview → export (#177, #189). Removed the old pipeline flags:
  `--streaming-mode`, `--include` / `--exclude` / `--exclude-all` property
  filtering, output-compression selection, and `--deterministic`. Use the
  `overview` / `export-pmtiles` tuning flags, also accepted by `tiles` (#249).
- **Breaking: Python `convert()` is deprecated.** It no longer runs the removed
  legacy pipeline; it chains `overview()` + `export_pmtiles()`.
  Use the two-step API for all options. Its legacy keyword
  arguments `drop_density`, `compression`, `include`, `exclude`,
  `exclude_all`, `deterministic`, `drop_smallest_as_needed`,
  `drop_smallest_threshold` and `progress_callback` were removed; passing one
  raises `TypeError`.
- `--polygon-visibility` retuned **4.0 → 2.0** (#259): the rendered sweep
  showed gates above 2.0 remove too many features at coarse zooms without
  reducing file size. Gates below ~2.0 mostly admit candidates dropped by
  write-time collapse (`corpus/SWEEPS.md`, Decision 6).
- Per-tile size caps at 500K by default, matching tippecanoe (#280).
- The simple-clip fast path is on by default; `--no-simple-clip-fastpath` opts
  out (#239, #256).
- Line thinning retuned 2.0 → 1.0, point thinning defaults to 16.0 under
  clustering, and junction continuation now defaults off, based on the same
  sweep methodology.
- Overview footers declare spec version 0.2.0 (#184, #190).
- The declared MSRV is now 1.95, corrected from 1.75 and verified in CI (#358).

### Fixed

- **PMTiles directories**: the contiguous-offset rule now applies to leaf
  entries, oversized directories split into leaves, and `leaf_dirs_offset` is
  never written as 0 (#356, #377, #378).
- **MVT polygons are cleaned in tile space after quantization**, preventing
  self-intersecting or degenerate rings (#383, #393). Ring winding follows
  the spec.
- `export-pmtiles` declares the requested minimum zoom even when the coarse
  levels generalized to nothing (#380, #390).
- `coalesced_count` is withheld from tiles when it is 1 everywhere (#379,
  #389).
- Renamed source columns are published under their source name (#359, #365).
- `DATE`, `TIMESTAMP` and `DECIMAL` properties are encoded instead of dropped,
  with timestamps UTC-stamped.
- `--tile-buffer` is read as tile pixels, not MVT extent units, and tile
  membership derives from the buffer-expanded bbox.
- MultiPolygon parts that split into pieces at a tile boundary are kept (#244).
- Empty overview levels auto-clamp instead of failing the run (#211).
- Reserved-column collisions are auto-renamed rather than rejected (#288).
- An explicit null GeoParquet `crs` is assumed to be OGC:CRS84.
- Coarse-level export used geographic latitude instead of Web Mercator Y in the
  tile-local transform, distorting high-latitude tiles.
- An input with a case-insensitive `level` column collision is rejected up
  front instead of producing an ambiguous overview file.
- `decode` names the section or tile that failed to decompress, with its byte
  offset (#381, #388).
- Oversized remote column chunks are no longer re-fetched per page (#261), and
  `http://` inputs work (#262).

### Performance

- Remote-input network traffic is bounded to about 1× the object's bytes,
  down from ~3×, regardless of zoom range or pass count. Selected row groups
  are staged up front for coalesced, parallel fetches; column chunks are
  cached to the largest row group's working set (#261); and an on-disk spill
  file serves passes 2 and later (#219, #286, #287). If the spill volume fills,
  conversion continues by re-fetching from the network.
- Export uses a single-read fan-out pass to avoid cross-level prefix and
  per-partition re-reads, with parallel clipping, simplification, MVT encoding
  and gzip (#227, #228, #233, #235).
- Large polygons split top-down recursively instead of being clipped whole
  (#226), and three O(V²) hot spots in clipping and validity checking were
  replaced or capped (#237, #241, #242).
- Memory budgeting uses winner-grid level waves, partition waves sized from
  the densest partition and available cores, and an auto profile that selects
  RAM or spill storage from measured geometry (#293, #294, #303,
  #305, #306, #311).
- Parquet row-group encoding and GeoParquet WKB encoding run in parallel
  (#296, #304).
- `--profile` presets over a single-read pipelined pass-2 engine (#212, #213).

### Internal

For Clippy, rustfmt, dependency, CI and benchmark-harness changes, see the full
465-commit record: `git log v0.6.0..v0.7.0`.

## v0.6.0 (2026-03-11)

### Feat

- implement point clustering with position averaging (#25)
- implement accumulator system for attribute aggregation (#23)
- implement gap-based density detection (#24)
- support WKT geometry encoding (#35)
- implement tiny polygon accumulation (#85)
- **profiling**: add fine-grained spans to read_parquet phase
- add time profiling with tracing
- add memory profiling with dhat

### Fix

- remove needless borrow in benchmark
- resolve clippy warnings and unused imports
- remove WKT fixture from repo, tests skip when missing
- gracefully skip WKT tests when fixture is missing
- skip profiling integration tests when dhat-heap feature enabled
- use tempfile crate for proper temp directory isolation in tests
- use cross-platform temp directories in integration tests

### Refactor

- replace wagyu-rs with i_overlay for polygon clipping

### Perf

- parallel row group I/O for ~24% speedup
- reuse file handle across row groups (#41)
- parallelize tile encoding in Phase 3 (#90)

## v0.5.0 (2026-03-10)

### Feat

- **core**: wire up pipeline to use WorldCoord throughout (Phase 2)
- **core**: add WorldCoord-based hierarchical clipping (Phase 2)
- **core**: add WorldCoord-based feature drop functions (Phase 2)
- **core**: add WorldCoord support to MVT encoding and validation (Phase 1)
- **core**: add WorldCoord support to clipping modules (Phase 1)
- **core**: add WorldCoord-based simplification functions (Phase 1)
- **core**: add WorldCoord type for 32-bit integer coordinates (Phase 0)
- **clip**: integrate wagyu-rs for robust polygon clipping
- add --deterministic flag and fix PR #63 review feedback

### Fix

- **clip**: enable U-shape split test with wagyu-rs v0.2.1
- **clip**: add wagyu fallback for edge case geometry handling (#94)
- change default compression to gzip and add CRS validation
- change default compression to gzip for compatibility
- implement leaf directory support for large PMTiles archives
- **core**: clamp tile coordinates and bounds to valid ranges
- **core**: fix issue #83 - geometry coordinates collapsing to zeros
- resolve clippy warnings in tests
- **core**: align feature_drop coordinate precision with MVT encoding
- **ci**: download fixtures from release instead of LFS
- **ci**: remove 1.8GB fixture from LFS to fix bandwidth quota
- Remove unused MIN_EXPECTED_TILES constant
- Remove #[ignore] from regression tests - fixture is in LFS
- use is_empty() instead of len() > 0 for Clippy
- Fix clippy warning and add clipping benchmarks
- **tile**: clamp latitude to Web Mercator bounds
- use wagyu-rs from crates.io instead of path dependency
- **ci**: update benchmark group names after consolidation

### Perf

- Add pre-clip bounding box filter for large geometries
- Implement hierarchical clipping across zoom levels
- Replace Wagyu with Sutherland-Hodgman for tile clipping

## v0.4.0 (2026-02-25)

### Feat

- **python**: add progress callback support
- **python**: add streaming mode and parallel control parameters
- **python**: add property filtering and layer name parameters

### Fix

- add version sync safeguards and fix pyproject.toml version mismatch
- add clippy allow for too_many_arguments and add clippy to pre-commit
- use workspace dependency for gpq-tiles-core version

## v0.2.0 (2026-02-25)

### Fix

- **release**: complete v0.2.0 release setup

## v0.1.0 (2026-02-24)

### Feat

- set up commitizen for automated versioning and releases
- **quality**: warn about pathologically small row groups
- default to zstd compression, expose parallel options in CLI
- add progress bars for cleaner output
- parallelize geometry processing within row groups (#37)
- parallelize tile processing for large geometries (#33)
- **cli**: add --streaming-mode flag with progress reporting
- **pipeline**: implement ExternalSort streaming mode
- **pipeline**: add StreamingMode::ExternalSort variant
- **core**: add external sort and WKB serialization modules
- **streaming**: add StreamingPmtilesWriter with LowMemory mode
- **streaming**: add memory budget configuration and tracking
- **streaming**: add row-group-based streaming tile generation
- **quality**: add GeoParquet file quality detection for streaming
- add tile deduplication with XXH3 hashing and run_length encoding
- add compression options (gzip, brotli, zstd, none) for PMTiles output
- add property filtering with --include/-y, --exclude/-x, --exclude-all/-X flags
- add 17K feature fixture for parallelization benchmarks
- add tilestats metadata to PMTiles output
- auto-extract field metadata from GeoParquet schema
- add field metadata support to PMTiles writer
- derive layer name from input filename, add --layer-name CLI flag
- complete Phase 5 Python bindings with uv/ruff tooling
- add benchmark suite with generate_tiles_from_geometries API
- **pipeline**: add Rayon parallel tile generation
- **pipeline**: wire spatial indexing into tile generation
- **spatial-index**: add space-filling curve sorting for efficient tile generation
- complete Phase 3 with density-based dropping
- integrate feature dropping into pipeline (Phase 3)
- add point thinning (1/2.5 drop rate per zoom)
- add line dropping (coordinate quantization algorithm)
- implement tiny polygon dropping with diffuse probability
- implement PMTiles v3 writer (Tasks 7-9)
- implement tiler pipeline wiring clip → simplify → MVT
- implement MVT encoding for vector tiles
- add golden comparison tests against tippecanoe output
- implement geometry clipping with correct BooleanOps
- implement zoom-based simplification
- implement Arrow-native geometry batch processing (TDD green)

### Fix

- **release**: add version to core dep, add READMEs, fix benchmark filter
- **release**: install protoc inside manylinux container
- **python**: copy README into crate for sdist builds
- **ci**: use tag triggers for release, skip slow benchmarks
- consolidate release workflows and fix version bump detection
- guard against degenerate linestrings in simplify and fix flaky test
- PMTiles now compatible with pmtiles.io and standard viewers
- **golden**: update stale Z8 test to use full pipeline
- resolve three medium/low priority issues
- simplify geometry in tile-local pixel coordinates
- handle antimeridian crossing in tiles_for_bbox
- **clip**: preserve all polygon parts when clipping produces MultiPolygon
- use real bbox calculation instead of world bounds
- resolve CI timeouts and coverage linker errors
- upgrade pyo3 0.24 → 0.28 for Python 3.14 support
- resolve CI failures for benchmark, check, test, and security audit

### Perf

- **streaming**: add memory benchmarks for streaming pipeline
