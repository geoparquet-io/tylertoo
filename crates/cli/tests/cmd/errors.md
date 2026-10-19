Exit codes: clap usage errors exit 2; runtime errors exit 1 with `Error: ...`
on stderr. `errors.out/` checks that the refused overwrite left
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
? 1
Error: cannot write no-such-dir/out.pmtiles: directory no-such-dir does not exist

```

An existing output needs `--force`.

```console
$ tylertoo overture-names.parquet existing.pmtiles
? 1
Error: existing.pmtiles exists (use --force to overwrite)

```
