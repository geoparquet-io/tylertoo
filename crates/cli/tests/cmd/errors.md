Exit codes: usage errors exit 2, both clap's and the option and output-path
rules core checks before reading anything (#779); runtime errors exit 1. Both
print `Error: ...` (clap: `error: ...`) on stderr. `errors.out/` checks that the refused overwrite left
`existing.pmtiles` empty.

An unknown flag on the bare form is reported against `tiles`.

```console
$ tylertoo --bogus
? 2
error: unexpected argument '--bogus' found

  tip: to pass '--bogus' as a value, use '-- --bogus'

Usage: tylertoo tiles [OPTIONS] [INPUT] [OUTPUT]

For more information, try '--help'.

```

Input errors name the file.

```console
$ tylertoo missing.parquet out.pmtiles
? 1
Error: overview conversion failed: input error: cannot open missing.parquet: No such file or directory (os error 2)

```

```console
$ tylertoo corrupt.parquet out.pmtiles
? 1
Error: overview conversion failed: input error: cannot read the Parquet footer of corrupt.parquet: Invalid Parquet file. Corrupt footer

```

An output in a missing directory is refused before the input is read.

```console
$ tylertoo overture-names.parquet no-such-dir/out.pmtiles
? 2
Error: cannot write no-such-dir/out.pmtiles: directory no-such-dir does not exist

```

An existing output needs `--force`.

```console
$ tylertoo overture-names.parquet existing.pmtiles
? 2
Error: existing.pmtiles exists (use --force to overwrite)

```

A `geo` key that is JSON but not an object is a GeoParquet read error.
(`geo-not-object.parquet` is one row of `overture-names.parquet` with its
`geo` metadata set to `[]`; `shard.pmtiles` is a copy of
`tests/fixtures/golden/open-buildings.pmtiles`.)

```console
$ tylertoo geo-not-object.parquet out.pmtiles
? 1
Error: overview conversion failed: input error: parquet error: geo-not-object.parquet: Failed to read GeoParquet file: the 'geo' metadata is valid JSON but not an object; rewrite the file's GeoParquet metadata (e.g. re-save it with `gpio convert`)

```

`merge` refuses inputs that claim the same tile; one shard named twice is
the usual cause.

```console
$ tylertoo merge out.pmtiles shard.pmtiles shard.pmtiles
? 1
Merging 2 archive(s) → out.pmtiles
Error: merge failed: Invalid configuration: shard.pmtiles and shard.pmtiles claim overlapping tile ids: both address tile id 1210 (z5/16/11). merge concatenates disjoint shards — it will not reconcile a tile two inputs both claim, so check how the shards were cut (a shard listed twice, or one left over from an earlier run, is the usual cause). The same archive named twice means its own directory addresses that id more than once, which no valid PMTiles archive does

```

A malformed `--band` spec is a usage error that names both spellings.

```console
$ tylertoo pyramid out.pmtiles --band overture-names.parquet
? 2
Error: band "overture-names.parquet": expected LO-HI:INPUT[:LAYER] or LO-HI=INPUT[=LAYER]

```

A class rank must be a finite number.

```console
$ tylertoo overture-names.parquet out.pmtiles --class-rank names:a=nan
? 2
Error: rank NaN for class "a" must be a finite number (NaN and infinity cannot be ordered against other classes)

```
