The top-level help: the subcommand list a new user sees first. Each
subcommand's own `--help` is pinned by `docs/reference/cli.md`'s drift guard.

```console
$ tylertoo --help
Convert GeoParquet to PMTiles vector tiles and multi-resolution overviews.

`tylertoo INPUT OUTPUT` with no subcommand runs `tiles`.

Usage: tylertoo <COMMAND>

Commands:
  tiles           Generate PMTiles vector tiles from GeoParquet (the default command)
  overview        Build a multi-resolution overview GeoParquet file
  validate        Check a GeoParquet overview file against the overviews spec
  export-pmtiles  Export a PMTiles archive from an overview GeoParquet file
  decode          Decode a PMTiles vector-tile archive back to GeoParquet
  stats           Report per-zoom tile sizes for a PMTiles archive, from its directory alone
  pyramid         Build one archive from several inputs, each owning a zoom range
  merge           Combine PMTiles archives that hold disjoint tiles into one
  shard-plan      Cut a dataset's tile space into N disjoint shards for a sharded build
  help            Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version

```
