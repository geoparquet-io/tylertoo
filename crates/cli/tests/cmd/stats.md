`stats` over the committed golden archive. It prints no paths or timings, so
the output is pinned whole.

```console
$ tylertoo stats ../../tests/fixtures/golden/road-detections.pmtiles
 z  tiles   total   mean    p50     p99     max
 0      1     506    506    506     506     506
 1      1     685    685    685     685     685
 2      1   1,163  1,163  1,163   1,163   1,163
 3      1   2,163  2,163  2,163   2,163   2,163
 4      1   3,463  3,463  3,463   3,463   3,463
 5      1   5,045  5,045  5,045   5,045   5,045
 6      1   7,081  7,081  7,081   7,081   7,081
 7      2  10,566  5,283  1,390   9,176   9,176
 8      2  12,514  6,257  1,242  11,272  11,272
 9      2  13,525  6,762    847  12,678  12,678
10      2  17,946  8,973    914  17,032  17,032

Largest 10 tile(s):
 z    x    y   bytes
10  338  472  17,032
 9  169  236  12,678
 8   84  118  11,272
 7   42   59   9,176
 6   21   29   7,081
 5   10   14   5,045
 4    5    7   3,463
 3    2    3   2,163
 7   42   58   1,390
 8   84  117   1,242

```

`--largest` caps the second table; `--json` is the machine-readable form.

```console
$ tylertoo stats ../../tests/fixtures/golden/road-detections.pmtiles --largest 2 --json
{
  "per_zoom": [
    {
      "z": 0,
      "tile_count": 1,
      "total_bytes": 506,
      "mean_bytes": 506,
      "p50_bytes": 506,
      "p99_bytes": 506,
      "max_bytes": 506
    },
    {
      "z": 1,
      "tile_count": 1,
      "total_bytes": 685,
      "mean_bytes": 685,
      "p50_bytes": 685,
      "p99_bytes": 685,
      "max_bytes": 685
    },
    {
      "z": 2,
      "tile_count": 1,
      "total_bytes": 1163,
      "mean_bytes": 1163,
      "p50_bytes": 1163,
      "p99_bytes": 1163,
      "max_bytes": 1163
    },
    {
      "z": 3,
      "tile_count": 1,
      "total_bytes": 2163,
      "mean_bytes": 2163,
      "p50_bytes": 2163,
      "p99_bytes": 2163,
      "max_bytes": 2163
    },
    {
      "z": 4,
      "tile_count": 1,
      "total_bytes": 3463,
      "mean_bytes": 3463,
      "p50_bytes": 3463,
      "p99_bytes": 3463,
      "max_bytes": 3463
    },
    {
      "z": 5,
      "tile_count": 1,
      "total_bytes": 5045,
      "mean_bytes": 5045,
      "p50_bytes": 5045,
      "p99_bytes": 5045,
      "max_bytes": 5045
    },
    {
      "z": 6,
      "tile_count": 1,
      "total_bytes": 7081,
      "mean_bytes": 7081,
      "p50_bytes": 7081,
      "p99_bytes": 7081,
      "max_bytes": 7081
    },
    {
      "z": 7,
      "tile_count": 2,
      "total_bytes": 10566,
      "mean_bytes": 5283,
      "p50_bytes": 1390,
      "p99_bytes": 9176,
      "max_bytes": 9176
    },
    {
      "z": 8,
      "tile_count": 2,
      "total_bytes": 12514,
      "mean_bytes": 6257,
      "p50_bytes": 1242,
      "p99_bytes": 11272,
      "max_bytes": 11272
    },
    {
      "z": 9,
      "tile_count": 2,
      "total_bytes": 13525,
      "mean_bytes": 6762,
      "p50_bytes": 847,
      "p99_bytes": 12678,
      "max_bytes": 12678
    },
    {
      "z": 10,
      "tile_count": 2,
      "total_bytes": 17946,
      "mean_bytes": 8973,
      "p50_bytes": 914,
      "p99_bytes": 17032,
      "max_bytes": 17032
    }
  ],
  "largest": [
    {
      "z": 10,
      "x": 338,
      "y": 472,
      "bytes": 17032
    },
    {
      "z": 9,
      "x": 169,
      "y": 236,
      "bytes": 12678
    }
  ]
}

```

`--layers` decodes each distinct tile once and adds `per_layer`.

```console
$ tylertoo stats ../../tests/fixtures/golden/road-detections.pmtiles --largest 0 --layers --json
{
  "per_zoom": [
    {
      "z": 0,
      "tile_count": 1,
      "total_bytes": 506,
      "mean_bytes": 506,
      "p50_bytes": 506,
      "p99_bytes": 506,
      "max_bytes": 506
    },
    {
      "z": 1,
      "tile_count": 1,
      "total_bytes": 685,
      "mean_bytes": 685,
      "p50_bytes": 685,
      "p99_bytes": 685,
      "max_bytes": 685
    },
    {
      "z": 2,
      "tile_count": 1,
      "total_bytes": 1163,
      "mean_bytes": 1163,
      "p50_bytes": 1163,
      "p99_bytes": 1163,
      "max_bytes": 1163
    },
    {
      "z": 3,
      "tile_count": 1,
      "total_bytes": 2163,
      "mean_bytes": 2163,
      "p50_bytes": 2163,
      "p99_bytes": 2163,
      "max_bytes": 2163
    },
    {
      "z": 4,
      "tile_count": 1,
      "total_bytes": 3463,
      "mean_bytes": 3463,
      "p50_bytes": 3463,
      "p99_bytes": 3463,
      "max_bytes": 3463
    },
    {
      "z": 5,
      "tile_count": 1,
      "total_bytes": 5045,
      "mean_bytes": 5045,
      "p50_bytes": 5045,
      "p99_bytes": 5045,
      "max_bytes": 5045
    },
    {
      "z": 6,
      "tile_count": 1,
      "total_bytes": 7081,
      "mean_bytes": 7081,
      "p50_bytes": 7081,
      "p99_bytes": 7081,
      "max_bytes": 7081
    },
    {
      "z": 7,
      "tile_count": 2,
      "total_bytes": 10566,
      "mean_bytes": 5283,
      "p50_bytes": 1390,
      "p99_bytes": 9176,
      "max_bytes": 9176
    },
    {
      "z": 8,
      "tile_count": 2,
      "total_bytes": 12514,
      "mean_bytes": 6257,
      "p50_bytes": 1242,
      "p99_bytes": 11272,
      "max_bytes": 11272
    },
    {
      "z": 9,
      "tile_count": 2,
      "total_bytes": 13525,
      "mean_bytes": 6762,
      "p50_bytes": 847,
      "p99_bytes": 12678,
      "max_bytes": 12678
    },
    {
      "z": 10,
      "tile_count": 2,
      "total_bytes": 17946,
      "mean_bytes": 8973,
      "p50_bytes": 914,
      "p99_bytes": 17032,
      "max_bytes": 17032
    }
  ],
  "largest": [],
  "per_layer": [
    {
      "z": 0,
      "layer": "road-detections",
      "feature_count": 34,
      "bytes": 1170,
      "geometry_types": {
        "LineString": 34
      }
    },
    {
      "z": 1,
      "layer": "road-detections",
      "feature_count": 49,
      "bytes": 1632,
      "geometry_types": {
        "LineString": 49
      }
    },
    {
      "z": 2,
      "layer": "road-detections",
      "feature_count": 99,
      "bytes": 3099,
      "geometry_types": {
        "LineString": 99
      }
    },
    {
      "z": 3,
      "layer": "road-detections",
      "feature_count": 213,
      "bytes": 6383,
      "geometry_types": {
        "LineString": 213
      }
    },
    {
      "z": 4,
      "layer": "road-detections",
      "feature_count": 345,
      "bytes": 10246,
      "geometry_types": {
        "LineString": 345
      }
    },
    {
      "z": 5,
      "layer": "road-detections",
      "feature_count": 508,
      "bytes": 14594,
      "geometry_types": {
        "LineString": 508
      }
    },
    {
      "z": 6,
      "layer": "road-detections",
      "feature_count": 721,
      "bytes": 20246,
      "geometry_types": {
        "LineString": 721
      }
    },
    {
      "z": 7,
      "layer": "road-detections",
      "feature_count": 991,
      "bytes": 27664,
      "geometry_types": {
        "LineString": 991
      }
    },
    {
      "z": 8,
      "layer": "road-detections",
      "feature_count": 1043,
      "bytes": 29513,
      "geometry_types": {
        "LineString": 1043
      }
    },
    {
      "z": 9,
      "layer": "road-detections",
      "feature_count": 1010,
      "bytes": 29424,
      "geometry_types": {
        "LineString": 1010
      }
    },
    {
      "z": 10,
      "layer": "road-detections",
      "feature_count": 1004,
      "bytes": 34030,
      "geometry_types": {
        "LineString": 1004
      }
    }
  ]
}

```
