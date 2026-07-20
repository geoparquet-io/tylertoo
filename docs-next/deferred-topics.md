# Deferred topics (Getting Started sidecar)

Digressions kept out of the tutorial to protect its narrative. Reviewed after
Pass C as Diving Deeper candidates. Most map to already-planned DD topics.

- **Why row-group sizing bounds memory** — the streaming model (memory ≈
  O(row group)) behind the `--row-group-size-mb 128` step. → maps to DD
  "Keeping memory bounded".
- **Covering-stats / `--bbox` footer pushdown** — how the São Paulo preview
  skips non-matching row groups (and byte ranges on remote input). → maps to DD
  "Tiling remote and multi-file inputs".
- **The quality ladder** — class ranking, visibility gates, density dropping,
  simplification that decide what shows at each zoom. Deliberately not opened in
  Getting Started. → maps to DD "Tuning what appears at each zoom".
- **One-shot vs two-step tradeoff** — when the facade is enough vs. when you
  want the inspectable overview artifact. Named in the tutorial's last step;
  full treatment belongs in DD "Working with the overview file".
- **Viewer hosting mechanics** — PMTiles needs HTTP range requests; pmtiles.io
  wants a URL, not a local file. Tutorial shows the minimal local-server path;
  deeper hosting notes are out of scope.
