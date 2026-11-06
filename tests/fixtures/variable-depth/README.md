# Variable-depth parity fixture (#803)

Reference stop sets for `--generate-variable-depth-tile-pyramid`, to compare
tylertoo against tippecanoe 2.79.0.

| File | Contents |
|------|----------|
| `points.parquet` | 9,598 building centroids (`id` = source row number, `geometry`), 139 KB: a dense core around Regensburg plus a sparse rural window |
| `manifest.json` | tool, version, exact command, header min/max zoom, per-zoom tile addresses and the stopped-tile set for tippecanoe and manby; a flag-off tippecanoe run for contrast (counts only) |

A stopped tile is a stored tile below the header max zoom with no stored child.
`runs.tippecanoe` is the reference. `runs.manby` is recorded as an observation:
on this input manby (0.1.1) does NOT reproduce tippecanoe's stop set (it writes
the full pyramid, like the flag-off run), so do not treat it as an oracle here.

## Attribution

Centroids of the Bayerische Vermessungsverwaltung building footprints,
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) (source file sha256
`8006727894b7fc7792fd70345a809784961fdc08779a764d1b5cdb679304093f`).

## Regenerate

```bash
uv run scripts/regen_variable_depth_parity.py \
    --buildings ~/bench-data/792/buildings.parquet \
    --tippecanoe ~/bench-data/793/tc-src/tippecanoe \
    --manby ~/bench-data/793/manby-target/release/manby
```

`--check` rebuilds in a temp dir and fails if `manifest.json` differs. The
.pmtiles outputs are not committed.
