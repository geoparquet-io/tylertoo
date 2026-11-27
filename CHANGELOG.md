# Changelog

Notable changes to this project are recorded here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and releases follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## v0.9.0 (2026-11-27)

### BREAKING CHANGE

- ClassRanking::new and Band::parse return typed errors
instead of String.
- Error::GeoParquetRead, Error::InvalidConfig,
PyramidError::InvalidConfig and ConvertError::InvalidConfig carry typed
reasons instead of String.
- these items are unstable; call the facade functions,
which validate their options.
- build these records with T::new(...) or
T::default().with_*(...); read ClassRanking through its getters.
- match struct variants of the error enums and Diagnostic
with `..`; they can no longer be built outside tylertoo-core.
- InputError::Decode and DecodeError::Mvt::source are
unstable; match the Footer variants with `..`.
- the options structs, reports, errors and kind enums can
no longer be built with struct literals or matched exhaustively outside
tylertoo-core. Build options with `Default::default()` and the `with_*`
setters; add a wildcard arm to matches.
- module paths such as `tylertoo_core::overview::convert::
ConvertOptions` are gone; import product items from the crate root
(`tylertoo_core::ConvertOptions`). Everything else is reachable only
through the unstable `tylertoo_core::internal`.
- `Error::PMTilesWrite`, `PMTilesRead`, `MvtEncoding`,
`InvalidGeometry` and `Config` are removed; pyramid functions return
`PyramidError`; `validate_bands` returns `PyramidError`; `is_config()` is
replaced by `Classify::kind()`.
- `ConvertOptions::streaming`, `--no-streaming` and the
Python `streaming=` keyword are gone; passing them is an error.
- tiles written without --feature-id no longer carry MVT ids.
- `MaxZoom` gains the `AutoCapped(u8)` variant; exhaustive
matches must handle it.
- add_tile_precompressed takes TileCoord; the optional
compression parser is Compression::parse, with FromStr also available.
- besides the new default caps (9e71075), the merge valve
now merges everything at once in a tile that stalls twice, so exports
whose tiles stall write different tiles even with the old caps.
- exports with default options write different tiles: a
500K compressed cap and a 2M raw cap instead of a 500K raw cap. Pass
`--max-tile-size 500K --max-compressed-tile-size 0` for the old behaviour.
- `--include-property` naming the geometry's bbox covering
(`bbox`, `geometry_bbox`, or a declared covering) is now an error on
`overview` and `tiles`. It was previously accepted and had no effect.
Drop the name from include lists.

### Feat

- a memory-limit option sets the memory a run sizes to (#876)
- a workers option sets the worker pool's size (#876)
- **core**: a traced wait span carries its call's body time and last body end (#876)
- **core**: tile trace splits quantize and layer build into sub-stages, with spans (#876)
- **bench**: tile_trace.py summarizes a tile trace (#876)
- **core**: per-tile export trace behind TYLERTOO_TILE_TRACE (#876)
- **core**: --preset tippecanoe sets --valve-scope zoom (#768)
- **core**: --valve-scope smooth|tile|zoom; drop floors thin over-cap point tiles (#768)
- **core**: --merge-scope smooth|tile, shard-invariant merge floors (#768)
- **core**: shared-border pins behind --no-simplification-of-shared-nodes, off by default (#802)
- **core**: max-zoom simplification keeps the shared-border pins too (#802)
- **core**: pin pass-1 junctions so shared borders simplify identically (#802)
- **core**: drop features at merge from an over-cap join with a pre-tiled band (#838)
- **core**: cap the joined multi-layer tile with one valve pass across layers (#838)
- **core**: default tile buffer 5 and line-only max-zoom simplification
- **export**: list the tiles left over the size cap and name --size-valve drop (#873)
- **core**: default the density-budget drop rate to tippecanoe's 2.5
- **bench**: what each tool keeps, every sampled tile, and --cap 0 (#869)
- **bench**: compare tylertoo's tiles with tippecanoe's directly (#869)
- **cli**: --temp-compression lz4|zstd for the temporary intermediate overview
- **input**: reproject EPSG:3857 parts in a set that mixes CRSs
- **input**: reproject projected-CRS input to EPSG:4326 as it is read
- **tiles**: --band draws cells at some zooms and features at others (#332)
- **filter**: --mask keeps only features that intersect a polygon
- **cli**: serve archives over HTTP with a MapLibre viewer (#825)
- **tiles**: route features to layers by a column (--layer-column)
- **pyramid**: cell bands aggregate points into grid, h3, s2 or a5 cells (#332)
- **export**: MapLibre Tile output and pyramid --profile
- **shard**: build line-coalesced datasets with --shard (#546)
- **python**: run the full CLI from the wheel (python -m tylertoo)
- **cluster**: per-category point counts for clustered points (--cluster-categories)
- **export**: count tiles left over the size limit; opt-in max-zoom simplification
- **python**: tylertoo.stats() with largest and layers
- **stats**: per-zoom, per-layer report with --layers (#825)
- **config**: add --preset tippecanoe for tippecanoe-equivalent defaults
- **filter**: LIKE, ILIKE, NOT LIKE and NOT ILIKE in --filter
- **bench**: Brazil head-to-head and Africa streaming records, memory profiles
- **bench**: Brazil and Africa showcase regions, a --profile bounded cell, and streaming-only records
- **export**: variable-depth metadata note, report fields, diagnostics staging (#803)
- **export**: coarse-to-fine probe and frontier pruning (#803)
- **export**: StopPolicy with rule A′ and the RowRelease hook (#803)
- **export**: variable-depth option and combination rejections (#803)
- **core**: per-phase saturation telemetry in TYLERTOO_PROFILE_JSON (#810)
- **bench**: one-machine Overture buildings showcase, tylertoo vs tippecanoe (#810, track D)
- **bench**: read per-zoom wave width and spill bytes; keep_output cells; repoint stale result links (#810)
- **bench**: results-table cells; valve_guard verifies Moldova and records the #792 input sha256 (#810)
- **bench**: run_manifest records CPU, effective cores and provenance; valve_guard baseline keyed by host class (#810)
- **core**: typed errors for ClassRanking::new and Band::parse
- **core**: typed reasons for GeoParquetRead and InvalidConfig; type the hostile page header
- **core**: hide the non-facade helpers of stable types
- **core**: non_exhaustive value records with constructors
- **core**: mark every struct variant of the error enums and Diagnostic #[non_exhaustive]
- **core**: non_exhaustive stable options, reports, errors and kinds; with_* setters
- **core**: gather internals under tylertoo_core::internal; stable API is the root facade
- typed archive and pyramid errors, one error classifier (#438)
- remove the public streaming option (#440)
- **core**: cap the automatic max zoom with auto:N (#793)
- **core**: one typed GeoVersion on the report, covering every part; carry root in CrsAssumed; name the file for non-object geo (#708)
- **core**: report geo_version and geo_version_known; reject non-object geo JSON; quote the gpio inspect hint (#708)
- **bench**: final-window cells for the held questions (#788)
- **bench**: a multi-wave Moldova export cell and $TMPDIR watches (#740)
- **bench**: record the memory scope's events and peak (#715)
- **bench**: per-binary env and a disk-write gate for the spill-codec A/B (#715)
- **bench**: report each binary against the one before it (#734, #739)
- **bench**: warm-up runs and the spill-codec cell (#734, #715)
- **bench**: export-nz cell and a pinned-CPU-set load gate (#734, #739)
- **bench**: workload manifest and interleaved runner (#734)
- **export**: --profile on export-pmtiles; tiles passes its profile through (#788)
- **export**: log a multi-wave level store's grants (#740)
- **export**: grant the tile-gzip cache its cap from the run's budget (#733)
- **cli**: mimalloc is the default global allocator (#739)
- **python**: opt-in mimalloc feature, always on for musllinux (#739)
- **core**: reservation API on the run's MemoryBudget (#716, #788 C2)
- **cli**: opt-in mimalloc global allocator for non-musl targets (#739)
- **python**: defaults and validation from core, CLI knobs on every function (#779)
- **ci**: verify_archive.py --strict checks every tile against whole-archive invariants (#779)
- **ci**: nightly gate canaries and staleness check (#779)
- **ci**: mutate the CLI, block fix PRs on missed mutants (#779)
- **ci**: baseline and band ratchet (#779)
- **export**: cap the compressed tile at 500K by default, raw at 2M (#761)
- **export**: opt-in coverage fill for coarse-zoom holes (#750)
- **bench**: end-to-end pipeline bench for CodSpeed and the gate (#759)
- **bench**: parallel scorers, pinned JS served locally, README and attribution (#755)
- **bench**: one-command coalesce benchmark from the #745 harness (#755)
- **convert**: polygon thinning off by default (#745)
- **convert**: warn when the input is not spatially sorted (#745)
- **core**: merge oversized tiles and keep polygons by default (#745)
- **export**: hybrid estimate counts attribute bytes; add #745 harness
- **convert**: density budget per geometry kind (#745)
- **export**: hybrid valve predicts level thresholds from the scan (#745)
- **export**: merging size valves behind a switch (#745)
- PMTiles name, description and attribution (supersedes #669) (#746)
- **python**: 1.0 parity — pyo3-log, property selection, verbatim=, entry_zoom=, pyramid(), decode() (#445) (#728)
- **core**: validate GeoParquet geo.version and surface the assume-WGS84 fallback (#451) (#704)

### Fix

- **ci**: point the two #768 monotone exemptions at #925
- **core**: the valve predictor logs each sample phase with its chunks (#919)
- **core**: the valve sampler clips a shared row into its sampling tiles alone (#919)
- **core**: the valve predictor clips a sampled row once for all its tiles (#919)
- **ci**: gate the RssAnon test helper to linux-gnu and wrap a long test line (#876)
- **core**: run resources are applied once, after the whole configuration passes (#876)
- **core**: a chosen worker count lowers every core-derived width (#876)
- **core**: the export's memory probe returns freed allocator pages first (#876)
- **core**: a fill group closes before a batch would pass its row cap (#876)
- **core**: a tile trace's start and end lines bracket its export (#876)
- **core**: the predictor's sampler tests one tile key per row in O(1) (#768)
- **core**: import the flush's atomics inside it, under its own expectation (#768)
- **core**: a predictor flush stops adding dense state at the scan's room (#768)
- **core**: bound the valve predictor's fan-out; CI fixes after the #918 merge (#768)
- **python**: print the CLI's buffered warnings unconditionally; state the buffer's limits (#768)
- **python**: empty the user-warning buffer only when a run panics (#768)
- **python**: start each run with an empty user-warning buffer; the CLI prints its own (#768)
- **python**: buffer user warnings in the log bridge and raise them as the call returns (#768)
- **python**: raise a user warning when the run finds it, so a failed run still warns (#768)
- **export**: keep the stage modules' log target at overview::export (#916)
- **core**: constant-time wave lookup, honest density log, one wave owner (#876)
- **core**: price the #908 valve workspace, 384 -> 400 B/member
- **deps**: bump url to 2.5.9 and parquet to 59.3.3; waive unmaintained chrono and humantime
- **python**: export_pmtiles and pyramid raise user warnings too (#768)
- **python**: raise a user warning once, not also as a log record (#768)
- **core**: only the convert's marked input_row column ranks points (#768)
- **core**: valve floors keep points by count, wrap the antimeridian, and price per tile kind (#768)
- **core**: end the fade without a cliff and refuse zoom on a shard early (#768)
- **core**: one fade rule for both valves, set by --valve-max-step (#768)
- **core**: thin point tiles by a fixed rank keyed on the input row (#768)
- **core**: a drop floor keeps a share of a point tile by stride, not the points past a gap (#768)
- **core**: check only the predictor's dense state against the scan budget, not the tile counts tile holds too (#768)
- **core**: under smooth a drop floor thins only a point tile over its cap without it (#768)
- **core**: a price sample one empty clip short still prices its tile (#768)
- **core**: break a price sample's --feature-order ties by the tile's own file order (#768)
- **core**: price the valve samples a budgeted chunk of tiles at a time (#768)
- **core**: the valve predictor holds a sparse tile's count alone, checks its state against the scan's budget and releases it after the floors (#768)
- **core**: a date-line pair is one latitude and one world apart, not any same-latitude pair (#768)
- **core**: a tile whose floored first pass merged counts as oversized (#768)
- **core**: the predictor's scan pipe holds one flush of scan batches, not eight (#768)
- **core**: run the merge predictor in the scan consumer, not on its own thread (#768)
- **core**: thin a z0 point tile holding date-line pairs by the point stride (#768)
- **core**: drop the doubled WARNING prefix from the unsorted-input diagnostic (#768)
- **core**: warn when pass 1 finds the input rows not sorted by location (#768)
- **core**: layered refits keep the finest level's shared-border pins (#802)
- **core**: without pins, max-zoom holes keep main's diagonal gate; no pin compaction when off (#802)
- **core**: public API baseline for the pins option; pyramid takes no pin keyword (#802)
- **core**: a vertex of many stacked records finds a shared neighbour by sorting (#802)
- **bench**: score a few tiles of a large source without holding it all (#802)
- **core**: compact a chunk's occurrences in place (#802)
- **core**: max-zoom thinning keeps holes inside shells under pins, one ring gate (#802)
- **core**: export keeps no pin set for a level without polygons (#802)
- **core**: price the junction buffer before it grows, warn through a typed diagnostic (#802)
- **bench**: --score-only never cuts the NZ subset either (#802)
- **bench**: count stacked overlap once, rescale each layer's extent, build nothing on --score-only (#802)
- **core**: shrink the junction pins to the bytes the budget reserves (#802)
- **core**: reserve the resident junction pins in the pass-2 budget (#802)
- **core**: drop the junction occurrences past the scan budget (#802)
- **core**: cap archive-join drops at MAX_DROP_PASSES; nest point probes (#838)
- **core**: bound the archive-join drop to a logarithmic search (#838)
- **core**: filter unaffordable joins once per zoom and report them as diagnostics (#838)
- **core**: joint refits spill in the spill dir and skip joins too big for memory (#838)
- **core**: screen joins by a deflate bound, not the parts' gzip sizes (#838)
- **core**: --drop-polygons no longer turns rate-dropped polygons into placeholder squares (#875)
- **core**: merge one tile group at a time under the bounded profile
- **core**: leave the combine pool uncapped under the speed profile
- **core**: omit a joined tile the joint valve empties; warn that source losses miss joint refits (#838)
- **core**: price the joined-tile fit wave from the members it reads (#838)
- **core**: bound the joined-tile retry by the memory budget and report its losses (#838)
- **core**: blame a merged feature only when the lone over-cap survivor is one (#838)
- **core**: keep each layer of a joined tile in the merge valve's extent and drop fallback (#838)
- **core**: singles left after the eight cuts go before merged coverage (#873)
- **core**: the merge fallback drops every unmerged feature first, and never coarsens a point tile (#873)
- **core**: count a dropped merge's sources once, and expose the #873 zoom counters (#873)
- **core**: the over-cap warning does not name 512 as the floor (#873)
- **core**: warn, naming the tile, when one merged feature stays over the cap at the extent floor (#873)
- **core**: book a source merged into a kept feature as merged, not dropped (#873)
- **core**: halve an over-cap merged tile's extent before dropping merged features (#873)
- **core**: the drop valve keeps a 0.8 margin after its first cut (#873)
- **test**: fuzz_seeds builds again after #898 and #899 merged together
- **io**: count a dictionary page's stored bytes when a V2 header skips decompression (#894)
- **io**: stop the header walk at its chunk's end before skipping (#894)
- **io**: walk a page header on the stream, on its first read (#894)
- **io**: refuse any page header past the 1 MiB read-ahead (#894)
- **io**: read a page header ahead only while it could be a dictionary (#894)
- **io**: refuse a dictionary page that claims more values than its bytes (#894)
- **filter**: widen an i64::MIN statistic without overflowing abs() (#894)
- **fuzz**: the overview fuzz hook reads each band once (#833)
- **core**: count each over-cap joined tile once and keep overviews only while needed (#838)
- **core**: resolve a dictionary's struct/list value per slice, not per whole array (#874)
- **core**: log the pass-2 in-flight depth after MemoryBudget caps it
- **core**: borrow string tag values, tighten spill test, correct #874 docs
- **core**: keep packed rows sound: private schema table, length errors, bounded spill counts
- **core**: price export member spill under bounded only (#872)
- **core**: price member spill only when the run exports, at 3x the row price (#872)
- **core**: price export's member spill in the disk estimate; tidy disk.rs phases (#872)
- **core**: recheck disk at the end of the read when the plan over-counted its rows (#872)
- **core**: sample 65,536 rows before the one disk recheck; move a test's doc comment back (#872)
- **core**: price the archive a pyramid band exports while its intermediate exists (#872)
- **core**: charge the output archive to the tiles output volume only, and drop the cross-volume peak log (#872)
- **core**: price each level from measured bytes, release spill before the canonical level, price the export archive (#872)
- **cli**: scope bounded's mimalloc purge delay to each call and restore it after (#851)
- **cli**: test the purge decision in every build; say core does not configure the allocator (#851)
- **cli**: update profile help for the bounded purge delay; test the command-to-profile mapping (#851)
- **core**: signed inputs never skip TLS certificate checks (#843)
- **core**: do not send a signed input's query in the Referer of a redirect (#843)
- **core**: report a predicted disk shortage as a typed diagnostic; Python convert raises it as a UserWarning (#872)
- **core**: the pre-read staging warning names both frontends' remedies (#872)
- **core**: price the measured recheck from disk bytes per row, shared properties charged at every level (#872)
- **core**: name both frontends' remedies in the post-pass-1 disk warning; document where the intermediate check runs (#872)
- **core**: recheck disk once at the measured bytes per kept row (#872)
- **core**: warn up front only on staged input; price the post-pass-1 intermediate and spill compressed (#872)
- **core**: run the post-pass-1 disk recheck on the resolved sink backing (#872)
- **core**: count a pass-2 spill in the post-pass-1 disk recheck only when the sink resolves to spill (#872)
- **core**: recheck disk footprint after pass 1 from kept level rows; label the pre-read estimate an upper bound (#872)
- **core**: reset mismatched multipart provenance to identity (#749)
- **verifier**: explain decay cohort requirements (#749)
- **e2e**: limit decay expectations to built-in fixtures (#749)
- **core**: guard cascade part provenance lookups and skip the unused allocation (#749)
- **verifier**: reject vacuous placeholder decay checks (#749)
- **e2e**: track tiny-square cohorts for the decay invariant (#749)
- **core**: max-zoom simplification removes vertices only
- **core**: keep multi-part pieces under the max-zoom tolerance
- **core**: preserve canonical polygon parts through cascade collapse (#749)
- **export**: soften the drop hint; test mixed merged and lone over-cap tiles (#873)
- **export**: point only merged over-cap tiles at --size-valve drop (#873)
- **io**: join narrow dictionaries at a worker seam as a reader does (#857)
- **io**: refuse a footer schema nested past 64 levels (#789)
- **io**: cut batches by a listed narrow dictionary's values (#857)
- **io**: read row groups and later footer fields by id in the pre-check (#789)
- **io**: walk the footer as parquet-rs reads it, by field id (#789)
- **io**: read num_children as parquet-rs's truncated i32 in the footer pre-check (#789)
- **core**: dither cascade squares on the source area in every run (#749)
- **export**: leave coalesced_count off canonical-level tiles
- **core**: no cgroup memory-budget line under the memory override
- **python**: serve() on a background thread no longer crashes the exit
- **core**: keep a level group's queued output within its in-flight cap
- **core**: size an early level-store fill's wave grant to the members in flight
- **io**: decide the column split on the compressed bytes the file holds
- **ci**: keep the criterion gate's check name when the job is skipped
- **io**: size the split decision by each row group's own decode
- **io**: keep spawned split decodes inside the panic boundary
- **io**: saturate the column splits' footer byte weights
- **bench**: clamp source latitude at ±89.9, where both tools clamp (#869)
- **bench**: clamp source latitude to the Web Mercator edge (#869)
- **bench**: a banded direct cell is a verdict without the source (#869)
- **bench**: a zoom is unscored by its tiles, not its rows (#869)
- **bench**: kept ratios are reported not gated; self-test refuses other settings (#869)
- **bench**: a one-pixel tile is not a zoom's worst tile (#869)
- **bench**: no cells is not a pass, and EPSG "4326" is lon/lat (#869)
- **bench**: a breach PNG is written anew on every run (#869)
- **bench**: visual gate review fixes, with Hypothesis property tests (#869)
- **bench**: null attributes, point rounding and CRS in the visual metrics (#869)
- **core**: build the drain's batch ranges one decode window at a time
- **core**: cell-band intermediates take the run's --temp-compression
- lint expectations that Rust 1.100's clippy no longer fulfils
- **core**: reuse decode pool across buffered spill drains
- **core**: account for queued spill decode batches
- **core**: keep archive writeback optional and Linux-only
- **core**: fail a checkpoint on a failed background sync
- **python**: start the worker pool on the first call, not at import
- **core**: keep variable-depth probe work on the caller's rayon pool
- **core**: run streamed compute windows on the caller's rayon pool
- **export**: bounded groups of single-wave levels keep the floor (#815)
- **core**: end the streamed finest level unwritten when its producer fails
- **core**: share level rows only when two or more buffered levels use them
- **core**: flatten nested collections one row at a time in the geometry column
- **export**: bounded's in-flight cap is its largest wave plus the slack (#815)
- **coalesce**: skip coalescing past what one level's join can index instead of panicking
- **core**: poison the overview writer on any error inside a level
- **core**: end a pass-2 level unwritten when its drain source fails
- **core**: price a shared RAM sink by its input rows, not per output row
- **core**: keep helping the pool when a scoped_pipe consumer panics
- **core**: let a pool worker help the streamed level's windows it waits on
- **core**: rebuild RAM level batches on the writer's own pool
- **core**: help the pool until the drain's rebuild stops, on errors too
- **core**: help the pool while the RAM drain waits for rebuilt batches
- **coverage-fill**: the serial seed sweeps its buckets instead of rescanning per write (#830)
- **coverage-fill**: freed_before_open replays the encoder through the credits (#830)
- **coverage-fill**: the serial seed picks each write by the room left (#830)
- **coverage-fill**: the serial seed writes first what frees before its run opens (#830)
- **coverage-fill**: the serial seed writes existing runs' buckets first (#830)
- **coverage-fill**: the serial seed writes without a scheduling vector and frees room first (#830)
- **coverage-fill**: the serial seed stays inside the grant main completes in (#830)
- **coverage-fill**: charge the serial seed for the buckets waiting behind each write (#830)
- **core**: report VmHWM in convert's peak only after resetting it for this run
- **core**: drop the input-batch-sized position map once the levels index the shared rows
- **input**: size footer re-parses without re-reading part 0's footer (#815)
- **input**: drop the re-parsed footers of parts a selection prunes (#815)
- **io**: keep the decode row-group cap private (#829)
- **alloc**: set mimalloc's THP option only for a known mimalloc version (#828)
- **coalesce**: give back merged-run capacity that deduped vertices left, and count the stage test's pieces
- **convert**: size the chain-stage waves after what every level shares
- **docs**: load the Brazil map script from the site, not above it
- **layer-column**: stage only a dedicated covering out of reprojected input
- **layer-column**: leave a reprojected input's covering out of the staged files
- **layer-column**: stage reprojected input as lon/lat
- **input**: fail the read when the reprojection pool cannot start
- **input**: reproject on a pool of its own, so a busy caller pool cannot hang the read
- **input**: refuse to reproject a MultiPoint whose member carries an SRID
- **input**: refuse geographic CRSs off the Greenwich meridian
- **input**: keep a row group whose reprojected covering box wraps the antimeridian
- **input**: stop the pruning-bbox bisection at the domain edge; keep extension-only CRS differences refused
- **input**: bisect reprojected pruning bounds; allow per-part geometry CRS metadata
- **docs**: build the Brazil map popup from text, not HTML
- **docs**: undo the theme's admonition styling on the map credits
- **docs**: draw the map's credits icon once
- **merge**: keep the inputs' tilestats, summed per layer
- **docs**: load the Brazil map's archive without HTTP Range requests
- **input**: default the S3 region to us-east-1 for a custom endpoint
- **cells**: count H3 and A5 features in the cell holding their point at every zoom (#332)
- **tiles**: check a coarse job's shard binding before the cells read (#332)
- **tiles**: verify a cell-only coarse job's binding and report its rows (#332)
- **tiles**: cell-only coarse jobs, empty feature zooms, rollup collisions (#332)
- **auto-zoom**: cap the whole-row retry's sample by thinning accepted rows
- **auto-zoom**: re-sample every selected row once when --bbox, --mask or --filter leaves the stride sample empty
- **filter**: name CRSs as EPSG codes in the mask mismatch error
- **filter**: one-coordinate lines test as points; one owner for the mask row check
- **filter**: name a GeoParquet mask whose CRS was assumed
- **filter**: stream the mask hash, skip non-files, and sample with conversion's row check
- **filter**: --max-zoom auto samples only features inside the mask; key the mask cache on content
- **filter**: keep --bbox and --mask wrap copies apart; refuse unusable mask files
- **serve**: drop queued requests off the run thread so a stall cannot block the stop
- **serve**: queue pipelined requests per connection instead of blocking the loop
- **serve**: opt-in CORS, capped response threads, no mixed-version downloads
- **serve**: refuse a tile whose file changed while it was read
- **serve**: a changed archive that does not reopen stays refused until it does
- **serve**: refuse a file rewritten in place until it reopens; refuse an empty archive list
- **serve**: cap a tile body before the read; fit bounds on archive change
- **serve**: TileJSON bounds from the archive's usable bounds
- **serve**: honour If-Match and If-Range on archive ranges
- **serve**: percent-decode names, ETag on ranges, reserve index, warn on variable depth
- **core**: log environment overrides outside their locks (#804)
- **core**: rebuild a GCS store when SERVICE_ACCOUNT changes (#804)
- **core**: rebuild a signed URL origin's connection pool when client options change (#804)
- **core**: re-read environment overrides for every conversion (#804)
- **python**: take tile_format and mlt_profile in the layered convert overload
- **tiles**: refuse an input without geometry before the layer split
- **tiles**: refuse an unsupported CRS before the layer split
- **tiles**: refuse a knob on an excluded property before staging the layers
- **tiles**: bound the layer split's resources: disk preflight, remote staging, buffers and ledgers
- **tiles**: prune the layer split by --filter statistics, and report every part's version
- **python**: give the layered convert overload every parameter
- **tiles**: apply --bbox before the layer split, and restate a native CRS a geo block omits
- **tiles**: keep a native-only CRS and drop the stale descriptor when staging layers
- **pyramid**: cell bands use the selected memory profile
- **pyramid**: price only the pool threads a batch's chunks can occupy
- **pyramid**: hold a cell band to the memory allowance, priced at measured residency
- **pyramid**: price a batch's partial cell maps at their growth peak
- **pyramid**: price cell-band growth per shard and small inputs by their rows
- **pyramid**: charge a cell band's read-ahead and batch maps, price counts by capacity
- **pyramid**: charge only the cells a table's batch holds
- **pyramid**: charge a cell table's write buffers to the memory budget
- **pyramid**: a remote cell band spills to the caller's spill dir
- **pyramid**: cell bands keep caller read settings and refuse category-named rollups
- **pyramid**: fixed cell-band chunks and a pre-rollup budget check
- **pyramid**: bounded, deterministic cell tables and stricter cell-band options
- **core**: pass read_batch_rows its projection when a shard widens its selection
- **core**: size parallel read-ahead by the planned batch
- **io**: never copy a piece past the bound, and count fixed-size list dictionaries
- **core**: regroup parallel reads to the reader's own planned yield
- **io**: hold narrow-key dictionary reads to the rows their key indexes
- **io**: refuse an oversized row by the cut's own measure and join while decoding
- **io**: cut every read batch by one row-by-row byte rule, sequential or parallel
- **io**: fill joined batches to the join size, as the parallel regrouping does
- **io**: check only read columns for narrow dictionaries and record the API change
- **io**: plan the decode size for narrow-key dictionary schemas
- **io**: do not join batches past a narrow dictionary key
- **io**: yield a decoded piece at the byte bound before decoding the next
- **io**: keep the decode bound for list-heavy files and join what it decodes
- **io**: price list values per row so wide list schemas are not read one row per batch
- **python**: build the wheel's mimalloc with local-dynamic TLS
- **export**: fail the export, not the process, on an MLT conversion error
- **export**: keep feature order under every MLT profile
- **build**: declare Rust 1.98 for the CLI and wheel, parse MLT layers once
- **tiles**: refuse an empty MLT layer name before the convert runs
- **ci**: regenerate the v8 plan seeds
- **plan**: store chain WKB with 64-bit offsets
- **shard**: no empty-level warning for a level below the shard's pivot
- **shard**: cut an EPSG:3857 input by its rows, not by metres read as degrees
- **shard**: reload the test-only eager input after the chain reps widen it
- **check**: let a partial overview's finest level hold chains and clusters
- **plan**: refuse a plan whose finest level is not its last
- **shard**: keep every chain of a read row group, and the read margin past ±180
- **assign**: key super-cells by a full 64-bit Morton code so deep levels keep a tile's cells together
- **assign**: lay the super-cell grid on the exact Mercator tile edges
- **assign**: share the budget in one sweep, partition runs instead of sorting, and match tile rows at the equator
- **preset**: keep sparse tiles under the density budget and allow variable depth with no raw cap
- **cli**: name a presigned input without its query in labels, flag conflicts and single-file errors
- **input**: bypass the cloud store only for a signed query, and keep a queried band URL whole
- **pyramid**: never split a band's layer out of a URL query
- **input**: recognize Azure SAS by sig plus sv, and keep queries out of band layer names
- **pyramid**: redact a band URL's query after whitespace and in NoLayer
- **cli**: name a presigned input without its query in band and flag errors
- **input**: drop signing fields from a plan identity only when signed
- **input**: read signed cloud https URLs as web URLs and strip refusal bodies
- **input**: fetch presigned metadata by GET and keep query identity in plans
- **input**: keep the query string of http(s) inputs and redact it everywhere it is shown
- **python**: stats() refreshes the logging bridge like every other binding
- **python**: install the logging bridge through a PyOnceLock
- **cluster**: price and build category columns without a staging copy; reject negative counts
- **ci**: record the --cluster-categories public API; regenerate v7 plan seeds
- **check**: declared but empty category labels fail the count check
- **plan**: refuse saved plans whose category counts miss point_count
- **cluster**: charge allocated bytes, not lengths, to the category budget
- **cluster**: net the feature table into the category budget; skip rejected rows
- **cluster**: budget the category interner; check category counts on subsets
- **export**: simplify variable-depth stops at the canonical zoom; count only written tiles
- **stats**: decode --layers bodies on decode's capped pool
- **stats**: reject a layer field with the wrong wire type
- **stats**: measure layer bytes on the wire, not by re-encoding
- **cli**: don't name a preset's export values as ignored by --plan-only
- **bench**: give moldova-f0 its measured wall tolerance in the valve guard
- **mvt**: count a vertical edge's x in the strip sweep's density
- **decode**: carry decoded tiles past the cap instead of decoding again
- **decode**: close a decode chunk on its decoded bytes too
- **input**: bound the bytes prefetched footers hold before their parse
- **writer**: refuse an encode concurrency that crosses serial mid-file
- **writer**: drain widened encodes when the encode concurrency drops
- **export**: fail a level group when its wave write panics
- **convert**: price the drain's base slots at the WKB-chunk slot too (#817 B1)
- **convert**: fit the whole drain, writer grant included, below peak RSS (#817 B1)
- **convert**: widen the bounded drain only on memory pass 2 gave back (#817 B1)
- **export**: fill a multi-wave level's store only once earlier waves drain (#817)
- **export**: admit a multi-wave level's first wave before its store fills (#817)
- **bench**: gate 792-def's 16-to-full speedup in the valve guard (#817)
- **bench**: fail a stale valve-guard baseline on the pinned host (#817)
- **export**: keep the level group's member cap on its sum (#817)
- **convert**: price a widened drain slot at least at one WKB chunk (#817 B1)
- **export**: coverage fill seed writes serially when its batch does not fit
- **export**: coverage fill fits every budget the serial fill fit
- **export**: bounded collect finishes each group before clipping the next (#817)
- **export**: charge coverage fill's snapping and seed writers in full
- **export**: coverage fill review findings on budget checks and the slide
- **decode**: bound chunks by tile count, cap every job, stop on writer error
- **export**: close collect groups before the byte cap; bound variable-depth groups (#817)
- **export**: bound a concurrent level group by the export's largest wave (#817)
- **export**: keep band reads off the collecting thread, typed, and bounded (#817)
- **mvt**: pair boxes reaching many strips once, in x order
- **mvt**: keep strip sweeps no costlier than the x sweep on long edges
- **bench**: valve guard refuses incomplete baselines and prints skipped checks (#817)
- **bench**: valve guard fails on missing telemetry, pins thread env per arm, merges record per cell (#817)
- **overview**: size staging by the budget when TYLERTOO_STAGING_MEM_BUDGET_BYTES is set
- **overview**: size the tiles disk preflight from the --bbox/--shard selection
- **ci**: isolate the CPU telemetry tests; sync crate READMEs; prose lint
- **io**: price repeated leaves once per batch, not once per row
- **bench**: claims the record supports; CodSpeed paths for mvt/ and compression/ (#810)
- **bench**: provenance and peaks from the timed runs; RESULTS.md prose to the new record (#810)
- **io**: plan read batches over the projected columns only
- **python**: keep the VariableDepthReport docstring under 88 columns (#803)
- **scripts**: quote parity fixture paths as SQL literals (#803)
- **export**: free each level's stopped tiles once written (#803)
- **cli**: classify merge failures, so refusals exit 2 (#803)
- **export**: keep the variable-depth marker out of checkpoints (#803)
- **export**: refuse rule-A' columns that are missing or do not read (#803)
- **export**: never let a collapsed polygon stand in for a thinned point (#803)
- **export**: count the scan's variable-depth decode quietly (#803)
- **export**: refuse variable depth without the raw tile size cap (#803)
- **export**: refuse line thinning and line/polygon drops from the footer (#803)
- **export**: block early releases at each level by its own buffer (#803)
- **ci**: retry transient Pages deployment failures
- **bench**: tippecanoe tiles the reported converter's output; check cached corpus outputs; tighten fixture tests (#810)
- **corpus**: re-pin Overture to 2026-10-21.0 and check every corpus input (#696, #810)
- **core**: hide the prost and tylertoo-io payloads of DecodeError and InputError
- **core**: read v0.8 plans whose CrsAssumed diagnostic has no root; tighten the simplify-factor property
- **core**: do not log a missing geo version when a native column implies 2.0
- **core**: keep the gamma clamp for unvalidated callers; skip the gamma check when the density budget is off
- **core**: reject --drop-gamma below 1 and a negative --simplify-factor
- **core**: point PyramidOptions::validate docs at PyramidError variants
- **io**: refuse overlong footer varints and end-of-input page-header skips (#789)
- **core**: finish #443 default-id fallout and tighten #441 checks
- **core**: omit default MVT feature ids; declare the geo:overviews stability stance
- **core**: GeoVersion is non_exhaustive, Mixed carries null for a part with no version, one version classifier; PROJECTEDCRS/BASEGEODCRS (#708)
- **core**: honour TYLERTOO_AUTO_MEM_LIMIT_BYTES set after the memory probe is cached
- **core**: compare declared-vs-assumed WGS84 across partitions; reject WKT projected CRS embedding 4326 (#708)
- **core**: reserve intermediate paths in a private directory; kill mutation survivors
- **deps**: move the AWS SDK past pin-utils (RUSTSEC-2026-0441)
- qualify mimalloc opt-out for musl; valve guard stops on gate timeout
- **export**: price valve memory per vertex-heavy tile, keep preflight price, reuse draw-order buffer
- **ci**: make the valve guard fail the step, split input failures, set ratio from 4- and 64-core runs
- **core**: name the preflight escape hatch only where it works
- **bench**: keep runs whose load gate timed out out of the medians
- **bench**: refuse to resume a run with other binaries
- **core**: open the tiles input once so the auto sample is reused
- **core**: track landed byte ranges of a partly staged chunk
- **bench**: restore criterion_main in tile_compress_dedup for cargo-codspeed
- **cli**: refuse a shard plan cut for another input before the fleet rules
- **bench**: fetch with a User-Agent; coverage fill at z0-14 (#788)
- **bench**: let one --bin run a baseline (#734)
- **export**: the export-wide store and the wave width follow --profile (#788)
- **core**: auto's RAM-or-spill choice reads the run budget's own allowance (#716)
- **input**: bounded staging keeps main's 256 MiB in-flight budget
- **core**: size staged input through the shared staging plan
- **core**: consolidate conservative per-volume disk preflight
- **core**: keep each export's gzip cap and memo independent
- **export**: prune an antimeridian partition to both sides, not the world (#740)
- **core**: stage the --max-zoom auto sample under the run's profile (#788)
- **core**: remove ad-hoc tile gzip tracing
- **core**: bounded keeps main's caps per phase, not its floors (#788)
- **core**: verify unchanged geometry values in release (#556)
- **ci**: list simplify_parity for mutants; type the loss-report test
- **python**: preserve native loss paths until report delivery
- **core**: finish v6 plan framing and validate losses before replay
- **cli**: restore per-zoom export notes; refresh tutorial outputs
- **ci**: cap the mutation gate at four shards with a fixed sample
- **bench**: measure geographic cascades in geographic units
- **core**: stream exact losses beyond report memory budgets
- **ci**: size mutation shards from the native diff listing
- **core**: validate cached schemas through the canonical resolver
- **ci**: let mutation tests compile past strict unused lints
- **ci**: shard the complete workspace mutation diff
- **core**: preserve quality-matched cascade retention
- **ci**: refresh API baselines and Python reference; macOS-safe path test
- **core**: bound diagnostic state and preserve source row identity
- **core**: refuse incompatible saved plans before decoding metadata
- **core**: budget eager input before decoding
- **core**: preserve read memory bounds through parallel merging
- **io**: bound decoded batches before dictionary expansion
- **io**: pin remote range reads to captured entity tags
- **core**: explain legacy clustered shard incompatibility
- **core**: validate covering statistics before spatial pruning
- **core**: validate source-plan options before input access
- **ci**: mutate every Rust crate in PR diffs
- **core**: make zero-area polygon repair orientation independent
- **core**: bind saved plans to remote object ETags
- **core**: validate auto-zoom coverings and rename provenance
- **F2**: publish diagnostic reports in the generated Python reference
- **S6**: share scoped parsing and restore native doctest discovery
- **S3**: enforce the io boundary through cargo-deny
- **core**: core owns every option bound and validates before any I/O
- **core**: one read-plan owner that splits wide byte arrays safely
- **core**: keep recoverable polygon fill after snapping
- **ci**: require exact archive feature accounting
- **779**: document the process-wide scaled byte-array ceiling
- **779**: the exemption-ledger pins live in ci/ratchets.toml
- **779**: one owner of the ignored-export-option rule
- **779**: one level-plan helper for overview and tiles
- **779**: one core owner of the convert and export knob mapping
- **779**: g1-architecture-ratchet expects any count above the pin
- **779**: msgpack 1.2.4 for the Python Quality pip-audit step
- **779**: tutorial outputs after F6's tile-space repair
- **779**: rustdoc -D warnings on the integrated tree
- **779**: public-API baseline lists F6's two repair counters
- **779**: canary table names F6's f6-skip-repair, not g3-polygon-repair
- **779**: ratchet pins after F6 met F5 and G1
- **mvt**: one owner for tile-space polygon validity (#748)
- **779**: re-enable the four fuzz hold-outs F5 made clean
- **779**: canaries that apply to the integrated tree, all in the table
- **779**: re-key the similarity baseline after F3 met G1's gate
- **779**: the streaming preflight asks the ceiling owner inside its seam
- **779**: lower the ratchet pins F3 and G3 left stale
- **779**: drop F3's root skipif from the read-only spill-dir test
- **core**: validate checks the Arrow schema; the boundary owns its panics (#779)
- **core**: refuse an IPC block that lies outside the file (#779)
- **core**: count non-finite geometry at ingest and at the clip guard (#707)
- make core the only owner of every tiles option rule (#779)
- **deps**: bump zstd and filelock for RUSTSEC-2026-0391/0398/0403/0410 and filelock CVEs
- **export**: review fixes for the valve cache and the new caps (#758, #761)
- **export**: count a fill's failed encodes as valve passes (#750)
- **export**: coverage fill review fixes; the fill takes only the slack (#750)
- **export**: coverage fill keeps its area locally and yields to real features (#750)
- **export**: coverage fill review fixes (#750)
- **core**: don't deadlock when convert runs inside a rayon pool
- **bench**: review fixes for flags, cache keys, failures and tippecanoe (#755)
- **bench**: the dirty mark tracks only the binary's source (#755)
- **bench**: keep machine-local paths out of reports (#755)
- **bench**: a failed tippecanoe run is a result, cached and reported (#755)
- **bench**: scale the load gate with the core count (#755)
- **bench**: start each convert repeat without the previous overview; tippecanoe on Moldova (#755)
- **bench**: one report per profile in the run directory (#755)
- **python**: LevelReport and SkippedLevel zoom can be None
- **core**: refuse tiles outside their zoom's grid in the PMTiles writer
- **core**: read covering stats through typed parquet Statistics
- **core**: escape layer and field names in PMTiles metadata
- **export**: merge threshold stops at the quantile gap (#745)
- **export**: review follow-ups for the size valve (#745)
- **convert**: placeholder squares are one GSD whatever the simplify factor (#745)
- **deps**: bump certifi to 2026.10.19 (PYSEC-2026-4320)
- **deps**: bump rustls-webpki and webpki-root-certs (RUSTSEC-2026-0382, RUSTSEC-2026-0383)
- **export**: find the overview's geometry column by its geoarrow type, not its name (#676)
- **convert**: --bbox across the antimeridian wrap; report wrapped features (#643, #644) (#721)
- **export**: fail fast when one tile cannot fit in RAM (#665) (#713)
- **core**: guard polygon i_overlay entry points against non-finite/huge coordinates (#692) (#698)
- **githooks**: fail the pre-commit on clippy errors instead of swallowing them (#697)
- **core**: verify bytes on a dedup hash hit (#531) (#691)
- **deps**: bump rustls to 0.23.46 for RUSTSEC-2026-0346..0349 (#674)
- **properties**: close three --include/--exclude-property UX gaps (#401) (#663)
- **convert**: record the entry-zoom ladder in the footer (#446) (#662)
- **convert**: fail --no-streaming early when a byte-array column passes 2 GiB (#606) (#659)
- **decode**: print each error cause once; say why compression 0 is refused (#396) (#660)
- **validate**: cap the cluster sum-invariant read batch (#607) (#657)
- **ci**: keep the TMPDIR sweep off cargo-mutants' scratch tree (#635) (#656)
- **python**: put NOTICE at the sdist root so PyPI accepts it (#646)

### Refactor

- **export**: inline covering_row_bboxes into its one caller (#916)
- **export**: move TileTarget to collect and resolve_level_zooms to plan (#916)
- **export**: move single-stage tests next to their stage (#916)
- **export**: move columns stage to export/columns.rs (#916)
- **export**: move valve stage to export/valve.rs (#916)
- **export**: move encode stage to export/encode.rs (#916)
- **export**: move collect stage to export/collect.rs (#916)
- **export**: move scan stage to export/scan.rs (#916)
- **export**: move store stage to export/store.rs (#916)
- **export**: move driver stage to export/driver.rs (#916)
- **export**: move plan stage to export/plan.rs (#916)
- **export**: move tests to export/tests.rs (#916)
- **core**: predict_drop_need in one expression, not predict_need's shape again (#768)
- **core**: the predictor's scan sizes bounds through size_bboxes, not a copy of it (#768)
- **core**: one bucket encoder for single and layered exports (#838)
- **core**: carriers read a one-byte pre-budget level per feature, not the assignment table (#875)
- **core**: Diagnostics::count sums reduced-extent tiles across zooms (#873)
- **check**: the cluster-sum check reads through the reader's source (#833)
- **export**: read the overview file through one engine (#833)
- **core**: validate packed rows with the spill cursor, drop the duplicate reader (#874)
- **core**: drop the shared schema-name cache, state the calibration bound (#874)
- **io**: name the footer pre-check's nested struct tables (#789)
- **core**: one pass-reuse rule for the valve's unions and verdicts
- **core**: group coverage-fill shapes with the shared box-pair sweep
- pass-2 drain nits from the architecture review
- **io**: drop IpcStreamReader::read_batch, the unused second batch reader
- **core**: one box-pair sweep for every validity and crossing check
- **core**: keep the scratch-overview flag crate-private
- **core**: let MemoryBudget own the encode cap of a downgraded auto run
- **alloc**: turn THP off with libmimalloc-sys's no_thp feature (#828)
- **input**: reproject WKB through the shared WKB walk; refuse a coordinate list narrower than x/y
- **core**: auto-zoom selects row groups through the streaming owner
- **serve**: move the server to core; answer CORS preflight; stream responses off the loop
- **pyramid**: drop the too_many_lines expectation merge_prepared_bands no longer meets
- **plan**: bound the plan's level scalars in one helper
- **shard**: move the chain reps instead of cloning them
- **assign**: key a super-cell by its Morton code alone and find its tile by shifting
- **cli**: move the CLI into a library run() that the binary calls
- **bench**: one cell_phases, and with_record seeds the default floors
- **test**: move the rendezvous helper to overview::testutil
- **fill**: keep the slide's miss and peak counters to tests
- **pipeline**: the drain takes its encode inputs as values, not a closure
- **export**: one group_schedule plans the level groups and their gate
- **stream**: inline overlaps_levels at its one caller
- **stream**: one encode_price with a row floor prices drain slots too
- **stream**: one helper builds the coalesce exact-endpoint join
- **input**: requests_within reads the admission's own budget
- **export**: keep the injected write panic out of release builds
- **fill**: build the slide's leftovers alone, drop the wave loop
- **input**: one owner for the staging in-flight count
- **decode**: fold batch metadata with the overview writer's owner
- **export**: one owner each for covering rows, geometry bboxes and the first probed level (#803)
- **bench**: e2e times through run_manifest and alternates tools per round; write results with provenance; drop dead records (#810)
- **bench**: coalesce times its runs through run_manifest (#810)
- **export**: drop unread valve counters
- **export**: the member-store backing uses sink_backing; loss cap from the run budget (#788)
- **core**: staging's bounded cap lives in bounded_cap (#788)
- **core**: fill tile bytes and a typed fill workspace error (#764)
- **core**: one owner for the spill codec (#715)
- **core**: one owner for the sink backing rule; the record names each memory probe (#716)
- **core**: fill passes use the cached tile fit (#767)
- **export**: size waves and the member store from MemoryBudget (#788)
- **core**: name the budget's inline constants, fix dead doc links (#788)
- **core**: isolate the tile gzip cache budget fallback
- **core**: move the ordered-read layer to overview/read.rs (#538)
- consolidate shared logic and tighten duplication gates
- enforce strict workspace ownership lints
- **core**: share overview conversion stages
- core options are the single spec for CLI and Python
- **core**: one per-run diagnostics channel replaces global counters
- **core**: ResolvedSchema is the one owner of schema resolution
- **io**: parquet and IPC I/O live only in the tylertoo-io crate
- **mvt**: pinch splitting becomes the owner's first repair tier (#748)
- **core**: pass the similarity ratchet the merge brought (#779)
- **decode**: walk directories with the shared PMTiles walker (#779)
- **core**: one owner for Parquet and IPC reader construction (#779)
- **cli**: one ExportArgs, pure translation to core options (#779)
- **core**: one owner for tiles option rules (#779)
- **cli**: derive bare-form subcommands from clap
- **cli**: indicatif HumanCount for thousands separators
- **python**: pythonize the convert, export and decode reports
- **core**: FromStr for Mode, MemoryProfile, SortDirection, RowGroupSizePolicy
- **core**: one semver grammar for geo:overviews versions
- **core**: io::copy into Xxh3 for the plan checksum
- **core**: remove the in-memory PmtilesWriter and DeduplicationCache
- **core**: remove the unused world-space clip path
- **core**: sort_all_objects for deterministic geo metadata
- **core**: plan-file WKB on the wkb crate

### Perf

- **core**: auto and speed run on every core; bounded keeps 40 workers (#876)
- **core**: auto grows the fill's groups by their measured members (#876)
- **core**: the export fill gathers 4x larger groups, sized to the budget (#876)
- **core**: the fill's ordered gap walk is one serial loop over pool-projected bboxes (#876)
- **core**: auto starts a footer-priced sink in RAM up to 150% of the allowance (#876)
- **core**: split a traced finish_group into its gap walk, member chunks and appends (#876)
- **core**: trace where a band collect's wall goes (#876)
- **core**: in-tile maps claim batches instead of nesting par_iter (#876)
- **core**: cut export waves by the members in them, not the densest partition (#876)
- **core**: check each part alone once in the candidate path too (#908)
- **core**: test a part pair only on what lies between the parts (#908)
- **core**: sweep a giant merged feature's parts in parallel (#908)
- **core**: sweep a dense merged group once, and test part pairs without naming the defect (#908)
- **core**: a tiles data shard with no valve floors reads the old two-tile margin (#768)
- **core**: cut the smooth predictor's cost to at most 1.09x of tile (#768)
- **core**: sort 32-bit tags in the many-record pin check (#802)
- **core**: a pin is where a shared edge run ends, not any vertex two rings pass (#802)
- **core**: resolve pins a chunk already shows, cheaper far points (#802)
- **core**: look pins up through a bucket directory, reduce them in parallel (#802)
- **core**: reuse one joined buffer in the serial screen (#838)
- **core**: measure screened joins in parallel runs; prove the gzip bound (#838)
- **core**: report archive copies by gzip ISIZE instead of decoding them (#838)
- **core**: skip combining joins the refit replaces; reuse one joined buffer when bounded (#838)
- **core**: copy pre-budget levels only when a level accumulates (#875)
- **core**: skip the pre-budget level copy when no level accumulates (#875)
- **core**: copy pre-budget levels only when the budget covers polygons (#875)
- **io**: bound a decode by its group's excess list values, not all of them (#848)
- **core**: cap the shared-zoom combine at eight workers, serial under bounded
- **core**: combine shared-zoom merge tiles on rayon in bounded runs
- **core**: stage --layer-column row groups no longer than the source's
- **core**: encode a joined tile's layers in parallel on each valve pass (#838)
- **core**: fit each zoom's joined tiles as the merge reaches it, plans opened once (#838)
- **core**: fit over-cap joined tiles per zoom in one wave; keep source valves (#838)
- **core**: cast nested list and map children only over the rows a slice reaches (#874)
- **core**: reserve take_wave's drain only when the wave has spill segments
- **core**: shrink split i_overlay polygon pieces to fit
- **core**: reserve exact interior and line-string counts in to_finite_geometry, test all three shapes
- **core**: release drained member buckets and compact clipped polygon vectors
- **core**: hold bounded read-ahead and pass-2 in-flight batches at 4
- **core**: copy the schema tables once per spill segment on reload (#874)
- **core**: cast nested UTC timestamps and dictionaries once per batch (#874)
- **core**: copy the schema table per batch again, fix calibration table and test (#874)
- **core**: price packed property rows in the member cost model (#874)
- **core**: extract Arrow properties directly into packed rows
- **core**: scan packed properties with fixed text lengths
- **core**: validate packed text only at construction boundaries
- **core**: prototype packed export property rows
- **cli**: purge mimalloc pages immediately under --profile bounded (#851)
- **core**: stop counting disk bytes once the measured recheck has fired (#872)
- **core**: gzip tiles with libdeflate at level 6
- **core**: write exact doubles as floats and negative ints as sint
- **io**: weigh column splits by the reader's own row groups
- **core**: fill a multi-wave level's store while the coarser levels encode
- **io**: do not split a decode too small to pay for its threads
- **core**: overlap the finest level's collect with the coarser levels' encode tail
- **core**: decode each read batch's columns on several threads
- **core**: key the tag hasher with a u64 seed, not a 192-byte secret
- **core**: hash MVT tag tables with process-keyed xxh3
- **core**: measure row gaps per collect chunk, not per row
- **core**: prune polygon validity checks to edge pairs that can meet
- **core**: keep the valve's lone-feature verdicts in one byte each
- **core**: replay the valve's finalize verdict for an unchanged group
- **core**: parallelize budgeted spill decoding
- **io**: write the temporary intermediate overview with LZ4
- **core**: write the archive back while export runs
- **core**: shrink a clipped ring only when it wastes most of its buffer
- **core**: hold an unpooled batch's level geometries in one column (#852)
- **core**: pool a batch's level geometries only when levels reuse them (#852)
- **core**: raise the default worker cap from 32 to 40
- **core**: cap the default worker pool at 32 threads
- **export**: a memberless row builds no properties
- **core**: stop sharing level rows once auto downgrades to spill
- **coalesce**: consume a level's chains as they are assembled, and thin in place
- **coalesce**: join over flat walks and 24-byte incidents instead of borrowed pieces
- **export**: bounded overlaps a multi-wave level's waves, at a quarter target (#815)
- **mvt**: a quantized ring holds only its snapped vertices
- **core**: size the level geometry column from its geometries up front
- **core**: keep auto's encodes and raw spill when it spills up front
- **export**: bounded re-plans multi-wave levels at three quarters of the target (#815)
- **export**: a wave's partition buckets are sized before they fill
- **core**: rebuild shared level batches in parallel at the RAM drain
- **export**: a whole tile takes the decoded geometry instead of a copy
- **export**: a feature whole in one tile keeps its own exact copy
- **export**: points stay inline instead of sharing an Arc
- **export**: the cascade and the world-edge copy share whole features too
- **export**: tiles that hold a feature whole share its geometry
- **clip**: a clipped ring keeps only the points it holds
- **export**: bounded splits only the levels that already take several waves (#815)
- **export**: bounded waves hold a quarter of the members (#815)
- **export**: partitioning fill holds an unclipped feature once across its levels (#815)
- **core**: encode a geometry the pass-2 levels share once per input batch
- **core**: share each input batch's rows across pass-2 levels and price the sink from the footers
- **decode**: bound decode's GeoParquet row groups at 128 MiB (#829)
- **input**: keep only part 0's decoded footer once a multi-part source resolves (#815)
- **alloc**: turn off mimalloc THP and collect at phase boundaries (#828)
- **input**: release the footers of S3 parts a bbox prunes (#815)
- **decode**: size each batch's GeometryBuilder up front (#828, #829)
- **coalesce**: size merged chain runs up front instead of growing them
- **convert**: box the chain geometry so an emptied coalesce table slot is 32 B (#851)
- **coalesce**: assemble chain walks in place and pack the join table (#851)
- **convert**: move each chain out of its coalesce table when pass 2 emits it (#851)
- **input**: reproject batches on the rayon pool, not the reader threads
- **core**: intersect row-group selections in linear time; test bad GeoParquet masks
- **core**: parse the client overrides only when their variables change (#804)
- **pyramid**: write cell tables in one pass per batch, one row group each
- **pyramid**: build a cell table's category columns in parallel
- **pyramid**: read cell bands in 64k-row batches of 1024-row tasks
- **pyramid**: shard the cell map so batches and rollups merge on every core
- **io**: decode one row group at a time so a decode is charged one group
- **shard**: save the coalesce chains in the plan so a shard runs no chain stage
- **input**: share one connection pool per origin across signed URLs
- **compression**: gzip tiles at level 9 on the zlib-rs backend
- **convert**: coalesce_level_lines takes the exact join's runs, not copies
- **convert**: keep small exact joins off the rayon pool and stop copying members
- **mvt**: keep the x sweep where few boxes share an x range
- **export**: overlap a multi-wave level's waves in the level group (#817)
- **export**: bound a level group's members in flight, not the group (#817)
- **export**: bounded keeps sizing the scan batch by batch
- **export**: read the scan's bands in parallel and size a pool's worth of bboxes at once
- **convert**: overlap pass-2 level encodes under auto too (#817 W1)
- **convert**: widen bounded pass 2's spill drain (#817 B1)
- **convert**: accumulate tiny-polygon levels one at a time under bounded (#817 A1 review)
- **decode**: cap parallel decode at 16 threads and 2 MiB chunks
- **export**: build coverage fill pyramid tiles in a sliding window
- **export**: take coverage fill's kernel and per-tile work off its serial paths
- **decode**: bounded parallel decode with parallel batch encode
- **decode**: decode tiles in ordered parallel chunks
- **export**: measure size-valve gaps in the pool (#817 E1)
- **read**: let a caller cap the ordered read's read-ahead; the export takes two collect groups (#817)
- **export**: read bands with the ordered multi-worker reader, pipeline the collect (#817 E1/L1)
- **export**: size scan batches in a few tasks, not one per thread (#817 S1)
- **mvt**: bind repair holes smallest shell first, by x range (#772)
- **mvt**: skip the second validity check of a repair that passed (#772)
- **export**: fork a huge feature's tile cascade at any depth (M1, #817)
- **mvt**: sweep polygon validity checks in horizontal strips (#772)
- **bench**: 16-to-full speedup and core-seconds checks in the valve guard (#817)
- **bench**: effective-cores gate in the valve guard (#817)
- **convert**: overlap level encodes under the speed profile only (#817 W1)
- **convert**: keep the bounded profile's one-level encode envelope (#817 W1)
- **convert**: skip the shared exact join when no level coalesces (#817 A2 review)
- **convert**: overlap a level's row-group encodes with the next level's (#817 W1)
- **convert**: free convert's line scratch and chain tables across the pool (#817 T1)
- **convert**: share the GSD-independent exact endpoint join across levels (#817 A2)
- **convert**: run the tiny-polygon accumulator's levels concurrently (#817 A1)
- **input**: keep a staging budget's worth of remote requests in flight
- **export**: compare A' point counts per level in parallel (#803)
- **export**: hold the probe to a quarter of the partition wave (#803)
- **export**: draw each variable-depth level as soon as the probe releases it (#803)
- **export**: size the canonical band at the coarser zooms once, after the scan (#803)
- **export**: per-batch property-name handles, not one shared refcount (#803)
- **export**: place buffered tile rows without a tan per zoom in the scan (#803)
- **export**: skip the levels the probe can never stop at in the scan (#803)
- **export**: size the canonical band at every level zoom in one pass (#803)
- **export**: prune variable-depth reads to each tile (#803)
- **export**: overlap the variable-depth probe with the coarse levels (#803)
- **core**: fuse the non-finite guard into the passes that already walk coordinates (#773)
- **python**: build the wheel on mimalloc by default (#792)
- **convert**: publish the intermediate overview onto a missing path (#791)
- **export**: price the size valve's memory and stop its wasted transient
- **export**: encode the densest tiles of a level first
- **export**: carry overlap, quantization and draw order across size-valve passes
- **core**: restore grant-sized coverage fill parallelism
- **core**: spill snapped coverage fills in per-zoom stores (#764)
- **core**: bound coverage fill workspace and stop repeated fill work (#764, #767)
- **io**: compress bounded overview spill with transient LZ4 framing (#715)
- **core**: reuse tile fit gzip until the raw bytes change
- **export**: each level owns its zoom's coverage fill; fill builder granted from the budget (#788)
- **export**: price the member store's fill buffer from measured members (#740)
- **export**: keep a multi-wave level in RAM when the budget affords it (#740)
- **export**: read a multi-wave level's band once (#740)
- **export**: run the wave collect's per-row work in the pool (#740)
- **core**: read a fully staged remote source in parallel (#544)
- **auto-zoom**: resolve --max-zoom auto on the conversion's own source
- **input**: grant pass-0 staging from the run's memory budget
- **input**: pass0 profile record and reads_locally for the #544 gate
- **auto-zoom**: stage the remote sample's columns before reading them
- **input**: retry a failed staging request as its row groups (#727)
- **input**: stage every part from one queue (#727)
- **input**: stage only the projected column chunks (#400)
- **core**: serialize tile features without retaining the protobuf graph
- **core**: share bounded gzip work for repeated tile payloads
- **core**: pass 1 takes half the cores as readers (#723)
- **core**: one profile rule for every budget grant (#788)
- **core**: price each reader's row group; reserve spill cache when it fits (#716)
- **core**: lazily pair junction continuations with exact ordering (#608)
- **core**: grant read-ahead, window and encodes from one budget (#716, #545)
- **core**: reduce geometry finiteness traversal overhead
- **mvt**: hoist scalar point query vertical bounds
- **core**: reduce finite polygon bounds loop overhead
- **mvt**: batch repaired hole containment queries
- **mvt**: one-ring fast path in the validity check (#748)
- **export**: test the valve's part pairs only when grouped (#758)
- **export**: merge everything at once when the valve stalls (#758)
- **export**: reuse the merge valve's unions across passes (#758)
- **export**: quantize the merge valve's polygons once per tile (#758)
- **core**: dissolve coverage-fill squares and quarter the fill budget (#750)
- **mvt**: split pinched rings in one pass, not one sort per pinch
- **core**: GroupInterner allocates only for new class values
- **core**: select, not sort, for the auto-zoom percentile
- **core**: rstar for cluster orphan lookup
- **core**: --filter evaluation on arrow-ord/arrow-arith kernels
- **core**: Hilbert tile ids via fast_hilbert
- **export**: size the pre-export scan from the bbox covering (#730) (#737)
- **decode**: read tiles by offset instead of loading the whole archive (#736)
- **core**: staging coalescing window, HTTP client knobs, and the #290 host sweep (#729)
- **core**: no per-tile maps in the pyramid merge; one dedup keyspace; explain band counts (#403) (#725)
- **convert**: read pass 1 with the parallel row-group reader (#717) (#722)
- **convert**: scan pass-1 geometry in place (#650) (#712)
- **convert**: process several pass-2 batches concurrently (#494) (#714)
- **core**: resolve --max-zoom auto from the bbox covering column when present (#693) (#703)
- **core**: env overrides for the remote staging constants (#290) (#700)
- **export**: intern property keys instead of cloning per feature (#461) (#667)
- **export**: run small zoom levels concurrently (#647) (#652)

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
