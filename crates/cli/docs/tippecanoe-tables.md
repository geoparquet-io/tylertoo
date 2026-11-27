<!-- Generated file. Do not edit by hand.
Regenerate: cargo run -p tylertoo --features gen-docs -- gen-reference-docs tippecanoe > crates/cli/docs/tippecanoe-tables.md
-->

<!-- --8<-- [start:flags] -->
| tippecanoe v2.82.0 | tylertoo | Notes |
| --- | --- | --- |
| `--maximum-zoom` | `tiles --max-zoom` |  |
| `--minimum-zoom` | `tiles --min-zoom` |  |
| `--maximum-zoom=g` | `tiles --max-zoom auto` | Each tool estimates the maximum zoom from the input. |
| `--layer` | `tiles --layer-name` |  |
| `--named-layer` | `pyramid --band` | Use GeoParquet bands with explicit zoom ranges. |
| `--name` | `tiles --name` |  |
| `--description` | `tiles --description` |  |
| `--attribution` | `tiles --attribution` |  |
| `--buffer` | `tiles --tile-buffer` |  |
| `--include` | `tiles --include-property` |  |
| `--exclude` | `tiles --exclude-property` |  |
| `--exclude-all` | `tiles --exclude-all-properties` |  |
| `--drop-rate` | `tiles --drop-rate` |  |
| `--gamma` | `tiles --drop-gamma` | Both protect sparse areas, but the scales differ. |
| `--drop-lines` | `tiles --drop-lines` |  |
| `--drop-polygons` | `tiles --drop-polygons` |  |
| `--no-simplification-of-shared-nodes` | `tiles --no-simplification-of-shared-nodes` |  |
| `--simplification` | `tiles --simplify-factor` | Tile units in tippecanoe; ground sample distance in tylertoo. |
| `--maximum-tile-bytes` | `tiles --max-compressed-tile-size` |  |
| `--coalesce-densest-as-needed` | `tiles --size-valve merge` | Similar tile size control; the merge algorithms differ. |
| `--drop-densest-as-needed`, `--drop-fraction-as-needed` | `tiles --size-valve drop` | Similar tile size control; the feature selection algorithms differ. |
| `--cluster-distance` | `tiles --cluster` | A pixel radius in tippecanoe; thinning cells in tylertoo. |
| `--coalesce` | `tiles --no-coalesce-lines` | Line coalescing is on by default; this tylertoo switch turns it off. |
| `--use-attribute-for-id` | `tiles --feature-id` |  |
| `tile-join` | `merge` | Use merge for archives with disjoint tiles. |
| `tile-join` | `pyramid` | Use pyramid for zoom bands. |
| `tippecanoe-decode` | `decode` | Writes GeoParquet. |
<!-- --8<-- [end:flags] -->

<!-- --8<-- [start:preset] -->
Preset defaults compared with tippecanoe v2.82.0.

| Flag | tylertoo default | With `--preset tippecanoe` | tippecanoe source |
| --- | --- | --- | --- |
| `--drop-gamma` | `1.5` | `1.0` | `main.cpp:3192 (gamma disabled; uniform thinning here)` |
| `--simplify-factor` | `1.0` | `0.25` | `main.cpp:83,3707; geometry.cpp:220 (tile units converted to ground sample distance)` |
| `--max-compressed-tile-size` | `512,000` | `500,000` | `main.cpp:85 (compressed bytes)` |
| `--max-tile-size` | `2,097,152` | no cap | `tile.cpp:2871-2891 (only compressed bytes are capped)` |
| `--size-valve` | `merge` | `drop` | `tile.cpp:2867 (drop approximates detail reduction followed by failure)` |
| `--valve-scope` | `smooth` | `zoom` | `tile.cpp:3421-3422,3506-3509 (one gap threshold per zoom with density dropping)` |
| `--max-zoom-simplify-factor` | unset | `0.25` | `main.cpp:83,3707; geometry.cpp:220; tile.cpp:607 (also simplifies at maximum zoom)` |
<!-- --8<-- [end:preset] -->
