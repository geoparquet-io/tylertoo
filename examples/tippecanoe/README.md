`01-preset.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

SRC=open-buildings.parquet
if [ ! -f "$SRC" ]; then
  curl -fsSLO "https://github.com/geoparquet-io/tylertoo/releases/download/fixtures-v1/$SRC"
fi

tylertoo tiles "$SRC" andorra.pmtiles --preset tippecanoe --layer-name buildings
tylertoo stats andorra.pmtiles
```

```text
...
✓ Converted open-buildings.parquet → andorra.pmtiles
  86 tiles across z0..z14 in 0.06s
...
 z  tiles   total   mean    p50     p99     max
 6      1     191    191    191     191     191
 7      1     253    253    253     253     253
 8      1     568    568    568     568     568
 9      2   2,663  1,331    938   1,725   1,725
10      2   6,529  3,264  2,197   4,332   4,332
11      2  14,365  7,182  4,241  10,124  10,124
12      6  24,939  4,156  1,923  13,961  13,961
13     20  34,480  1,724    399  13,687  13,687
14     51  44,286    868    333   9,014   9,014

Largest 10 tile(s):
 z     x     y   bytes
12  2066  1511  13,961
13  4133  3023  13,687
11  1033   755  10,124
14  8267  6046   9,014
12  2067  1512   5,339
14  8266  6046   4,983
13  4135  3024   4,850
14  8270  6048   4,687
10   516   377   4,332
11  1033   756   4,241
```
