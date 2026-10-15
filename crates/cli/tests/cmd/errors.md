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

```console
$ tylertoo missing.parquet out.pmtiles
? 1
Error: overview conversion failed: input error: io error: No such file or directory (os error 2)

```

An existing output needs `--force`.

```console
$ tylertoo overture-names.parquet existing.pmtiles
? 1
Error: existing.pmtiles exists (use --force to overwrite)

```
