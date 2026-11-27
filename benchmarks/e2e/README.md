# End-to-end benchmarks

Use [the release runner](../release.sh) to reproduce the published benchmarks:

```bash
benchmarks/release.sh
```

See the [benchmark results and measurement method](../../docs/benchmarks.md#measurement-method)
for the recorded host, tool versions, flags, timing procedure, and input pins.
The release runner commits no files for you. It writes the results and
regenerates the documentation for you to review in the release PR.

## What the comparisons mean

The harness compares these paths:

| Path | What it measures |
|---|---|
| tylertoo defaults | GeoParquet to PMTiles with the default generalization settings |
| tylertoo quality-matched | The same path with thinning disabled and simplification retained |
| tippecanoe end to end | GeoParquet conversion to FlatGeobuf, then tiling to PMTiles |
| tippecanoe tile only | Tiling from an existing FlatGeobuf file |

The end-to-end comparison includes the conversion that tippecanoe needs
because it cannot read GeoParquet. The tile-only comparison excludes that
conversion. Default settings can produce different maps. Use the
quality-matched path to compare outputs with similar feature retention;
it does not guarantee identical output.

## Settings parity

[The harness](run_e2e.py) owns the flag mapping in `tylertoo_argv()` and
`tippecanoe_argv()`. The results record the arguments that each tool used.

| Setting | How the harness compares it |
|---|---|
| Zoom range and layer name | Pass the same values to both tools. Corpus zoom ranges come from the corpus manifest. |
| Tile buffer | Pass the same buffer to both tools. |
| Tile size | Match the compressed byte cap and disable tylertoo's raw cap, since tippecanoe has no raw cap. |
| Oversized tiles | Enable tippecanoe's iterative drop valve. This approximates tylertoo's drop pass. |
| Thinning | Keep each tool's defaults for the default comparison. The quality-matched path disables tylertoo's thinning. |
| Gamma and simplification | Retain each tool's own behavior. Gamma operates over different spatial groups, and simplification uses different units. |

The [corpus manifest](../../corpus/manifest.json) supplies additional
per-dataset tippecanoe settings. The
[setup script](setup_tippecanoe.sh) owns the tippecanoe pin. The harness
checks the binary's version, and the results record the measured version.

## Output parity

[The parity gate](compare_tippecanoe.py) imports the harness's flag mapping
and compares decoded archives without timing them:

```bash
uv run benchmarks/e2e/compare_tippecanoe.py
```

The gate converts fixtures to line-delimited GeoJSON while preserving row
order. The timed harness uses FlatGeobuf, whose spatial index can reorder
features. tippecanoe's polygon reduction depends on input order, so the
outputs can differ between these paths.

[The tolerance file](tippecanoe_tolerances.toml) defines the per-zoom bands
for tile counts, features, vertices, and bytes. When you intentionally
change output, update the affected band and its measurement comment in the
same PR. For comparisons against the source geometry, see the
[visual gate](../visual/README.md).
