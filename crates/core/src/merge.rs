//! Concatenate disjoint PMTiles archives into one, by blob copy.
//!
//! This is the second half of a sharded build (#498). Pass 1 tiles each shard
//! of the input independently — a shard being a contiguous slice of the
//! Hilbert-sorted source, so the tile ids it produces are a contiguous slice
//! of the archive's id space — and this merges the shard archives into the
//! archive that ships.
//!
//! Because the shards are disjoint *by construction*, the merge is a copy:
//! every tile is written out exactly as it came in, still compressed, never
//! decoded and never re-encoded. What it has to get right is the bookkeeping
//! around the tiles — the directory ordering that makes the output genuinely
//! clustered, the header's bounds and zoom range, the `vector_layers` union —
//! and the check that the shards really were disjoint.
//!
//! **Disjointness is validated, not assumed.** Each input's tile-id *range*
//! (its directory's first and last id) must not overlap any other's. A
//! mis-specified shard set — the same shard listed twice, a shard from an
//! earlier run, overlapping bounds in the pass-1 split — would otherwise
//! produce an archive whose tiles silently shadow each other, and the failure
//! would surface much later as missing geometry on a map. The check reads
//! directories only, so it costs nothing next to the merge it guards.
//!
//! Overlap is a *range* check rather than a per-tile one on purpose: it is
//! strictly stronger than what the merge needs, it is O(inputs) instead of
//! O(tiles), and a shard set that trips it is a bug in how the shards were
//! cut, not something to reconcile. The merge itself is nonetheless written
//! as a proper k-way heap merge over the inputs' tile streams, so supporting
//! genuinely interleaved inputs later means relaxing the validation, not
//! rewriting the loop.
//!
//! The per-zoom tile counts in [`MergeReport`] are the point of the report:
//! a sharded build's parity oracle is "merging N shards yields the same tiles
//! per zoom as tiling the whole input in one pass", and these are the numbers
//! that gets compared.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

use crate::archive_index::{ArchiveIndex, TileRef};
use crate::compression::Compression;
use crate::dedup::TileHasher;
use crate::pmtiles_writer::{tile_id_to_zxy, StreamingPmtilesWriter, TileType};
use crate::pyramid::{vector_layers_json, LayerMeta};
use crate::tile::TileBounds;
use crate::{Error, Result};

/// Knobs for [`merge_shards`].
#[derive(Debug, Clone, Default)]
pub struct MergeOptions {
    /// Directory for the writer's spool file, which holds the merged tile
    /// data until the archive is assembled. Defaults to the system temp
    /// directory — worth setting for a shard merge, where the spool is the
    /// size of the output and `/tmp` is often a small tmpfs.
    pub work_dir: Option<PathBuf>,
}

/// What a [`merge_shards`] run produced.
#[derive(Debug, Clone, Serialize)]
pub struct MergeReport {
    /// How many input archives were merged.
    pub inputs: usize,
    /// Tiles written, counting a deduplicated tile once per id it is
    /// addressed by (so this is the merged archive's directory entry count,
    /// expanded).
    pub tiles_total: u64,
    /// Tiles per zoom. The parity oracle for a sharded build: merging N
    /// shards must yield the same per-zoom counts as tiling the whole input
    /// in one pass.
    pub per_zoom_tile_counts: BTreeMap<u8, u64>,
    /// The merged header's zoom range: the union of the inputs' *declared*
    /// ranges, not of the zooms that happened to hold tiles.
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// Distinct tile bodies actually written — `tiles_total` minus whatever
    /// the writer's dedup collapsed (shard boundaries duplicate nothing, but
    /// empty tiles across shards are identical).
    pub unique_tiles: u64,
    /// Bytes of tile data in the output.
    pub bytes_written: u64,
    /// Every input whose header carried no usable bounds, and which was
    /// therefore excluded from the merged archive's bounds. Non-empty means
    /// the output's extent describes only the inputs that did (#495's
    /// `bands_without_bounds`, for shards).
    pub inputs_without_bounds: Vec<String>,
    pub duration_secs: f64,
}

/// One input's expanded tile-id range, read from its directory alone.
struct IdRange {
    src: usize,
    lo: u64,
    hi: u64,
}

/// Merge `inputs` — PMTiles archives holding disjoint tile ids — into a
/// single archive at `output`.
///
/// Tiles are copied still-compressed; nothing is decoded. The output is
/// clustered by construction (tiles are written in ascending tile-id order),
/// its bounds are the union of the inputs' usable bounds, its zoom range the
/// union of their declared ranges, and its `vector_layers` the #492 union of
/// theirs.
///
/// Errors if the inputs disagree on tile type or compression, or if any two
/// of them claim overlapping tile ids. See the module docs for why the
/// overlap check is a range check.
pub fn merge_shards(
    inputs: &[PathBuf],
    output: &Path,
    options: &MergeOptions,
) -> Result<MergeReport> {
    let started = Instant::now();
    if inputs.is_empty() {
        return Err(Error::InvalidConfig(
            "merge needs at least one input archive".to_string(),
        ));
    }

    // Header + directories + metadata for every input. Even a hundred
    // multi-gigabyte shards cost megabytes here, which is the whole reason
    // `ArchiveIndex` exists.
    let indexes: Vec<ArchiveIndex> = inputs
        .iter()
        .map(|p| ArchiveIndex::open(p))
        .collect::<Result<_>>()?;

    // Tile type is validated (and must be MVT, which is all the writer can
    // stamp); compression is validated AND adopted, since the merged header
    // has to declare whatever the copied bodies actually are.
    let tile_compression = check_inputs_agree(&indexes)?;
    check_disjoint(&indexes)?;

    let mut writer = match &options.work_dir {
        Some(dir) => StreamingPmtilesWriter::with_temp_dir(tile_compression, dir.clone()),
        None => StreamingPmtilesWriter::new(tile_compression),
    }
    .map_err(|e| Error::PMTilesWrite(format!("failed to create streaming writer: {e}")))?;

    // The union of what the inputs *declare*, not of the zooms that happen to
    // hold tiles: a shard covering a sliver of the world legitimately has no
    // tile at the build's deepest zoom, and deriving the merged maximum from
    // the deepest tile copied would narrow the range every such shard set
    // declares.
    let min_zoom = indexes.iter().map(|i| i.header().min_zoom).min().unwrap();
    let max_zoom = indexes.iter().map(|i| i.header().max_zoom).max().unwrap();
    writer.set_declared_min_zoom(min_zoom);
    writer.set_declared_max_zoom(max_zoom);

    let mut inputs_without_bounds = Vec::new();
    let mut union: Option<TileBounds> = None;
    for idx in &indexes {
        match idx.bounds() {
            Some(b) => match union.as_mut() {
                Some(u) => u.expand(&b),
                None => union = Some(b),
            },
            // Named, not just counted: "one input had no bounds" is useless
            // when a merge takes a hundred shards and the output's extent is
            // visibly wrong.
            None => {
                let label = idx.path().display().to_string();
                log::warn!(
                    "{label}: header carries no usable bounds; it is excluded from the merged \
                     archive's bounds"
                );
                inputs_without_bounds.push(label);
            }
        }
    }
    match union {
        Some(b) => writer.set_bounds(&b),
        None => log::warn!(
            "no input archive carried usable bounds; the merged archive's bounds are left unset"
        ),
    }

    let layers = collect_layers(&indexes);
    if let Some(first) = layers.first().map(|l| l.id.clone()) {
        // Only the metadata's `name`; `vector_layers` below is authoritative
        // for what a client actually reads.
        writer.set_layer_name(&first);
        writer.set_vector_layers_json(vector_layers_json(layers).to_string());
    }

    let mut per_zoom_tile_counts: BTreeMap<u8, u64> = BTreeMap::new();
    let mut tiles_total = 0u64;

    // k-way merge by tile id. With validated-disjoint ranges the heap never
    // holds a genuine tie and this degenerates to reading the archives in
    // ascending-range order — which is exactly what makes the output
    // clustered — but written as a merge so interleaved inputs are a
    // validation change rather than a rewrite.
    let mut iters: Vec<_> = indexes.iter().map(ArchiveIndex::tiles).collect();
    let mut heads: Vec<Option<TileRef>> = vec![None; indexes.len()];
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = BinaryHeap::new();
    for (i, it) in iters.iter_mut().enumerate() {
        if let Some(tile) = it.next() {
            let t = tile.map_err(|e| at(&indexes[i], e))?;
            heap.push(Reverse((t.id, i)));
            heads[i] = Some(t);
        }
    }

    while let Some(Reverse((_, i))) = heap.pop() {
        let t = heads[i]
            .take()
            .expect("heap entry without a head is a bug in the merge loop");
        let data = indexes[i].read_range(t.range.clone())?;
        // Hash the COMPRESSED bytes, matching the pyramid merge: nothing is
        // decompressed here, so the uncompressed hash the writer's own path
        // keys on is unavailable. Conservative in the right direction —
        // identical compressed bytes are certainly identical tiles, so this
        // can miss a duplicate (two shards written with different gzip
        // settings) but never invents one.
        let hash = TileHasher::hash(&data);
        writer
            .add_tile_precompressed(t.z, t.x, t.y, hash, &data, data.len(), 0)
            .map_err(|e| Error::PMTilesWrite(format!("failed to add tile: {e}")))?;
        *per_zoom_tile_counts.entry(t.z).or_insert(0) += 1;
        tiles_total += 1;

        if let Some(tile) = iters[i].next() {
            let next = tile.map_err(|e| at(&indexes[i], e))?;
            heap.push(Reverse((next.id, i)));
            heads[i] = Some(next);
        }
    }

    let stats = writer
        .finalize(output)
        .map_err(|e| Error::PMTilesWrite(format!("failed to write {}: {e}", output.display())))?;

    log::info!(
        "[merge] {} input(s) → {}: {tiles_total} tile(s), z{min_zoom}-z{max_zoom}",
        inputs.len(),
        output.display()
    );

    Ok(MergeReport {
        inputs: inputs.len(),
        tiles_total,
        per_zoom_tile_counts,
        min_zoom,
        max_zoom,
        unique_tiles: stats.unique_tiles,
        bytes_written: stats.bytes_written,
        inputs_without_bounds,
        duration_secs: started.elapsed().as_secs_f64(),
    })
}

/// Prefix an error with the archive it came from.
fn at(idx: &ArchiveIndex, e: Error) -> Error {
    Error::PMTilesWrite(format!("{}: {e}", idx.path().display()))
}

/// Every input must agree on tile type and tile compression, since the merge
/// copies bodies verbatim under one header that declares both for all of
/// them. Mirrors the pyramid's up-front checks; the mismatch names both
/// archives, because "these disagree" is unactionable without knowing which.
fn check_inputs_agree(indexes: &[ArchiveIndex]) -> Result<Compression> {
    let first = &indexes[0];
    let (tile_type, compression) = (first.header().tile_type, first.header().tile_compression);
    for idx in &indexes[1..] {
        if idx.header().tile_type != tile_type {
            return Err(Error::InvalidConfig(format!(
                "inputs disagree on tile type: {} holds {tile_type:?} but {} holds {:?}; \
                 merge copies tile bodies verbatim under one header and cannot describe both",
                first.path().display(),
                idx.path().display(),
                idx.header().tile_type,
            )));
        }
        if idx.header().tile_compression != compression {
            return Err(Error::InvalidConfig(format!(
                "inputs disagree on tile compression: {} is {compression:?} but {} is {:?}; \
                 merge copies tile bodies verbatim, so a mixed set would mislabel every tile \
                 of one of them — re-tile the odd one out",
                first.path().display(),
                idx.path().display(),
                idx.header().tile_compression,
            )));
        }
    }
    // The writer stamps `TileType::Mvt` into the header it assembles, so an
    // agreed-on non-MVT set would still come out mislabelled.
    if tile_type != TileType::Mvt {
        return Err(Error::InvalidConfig(format!(
            "{}: tile type is {tile_type:?}, but merge writes MVT archives",
            first.path().display()
        )));
    }
    Ok(compression)
}

/// No two inputs may claim overlapping tile ids. See the module docs for why
/// this is a range check.
fn check_disjoint(indexes: &[ArchiveIndex]) -> Result<()> {
    let mut ranges: Vec<IdRange> = Vec::new();
    for (src, idx) in indexes.iter().enumerate() {
        if let Some((lo, hi)) = idx.tile_id_range().map_err(|e| at(idx, e))? {
            ranges.push(IdRange { src, lo, hi });
        }
    }
    // Sorted by `lo`, an overlap is always between an input and whichever
    // earlier one reaches deepest — tracked as a running high-water mark, so
    // a wide range that swallows several later ones is still reported against
    // the range that actually swallows them.
    ranges.sort_by_key(|r| (r.lo, r.hi));
    let mut furthest: Option<&IdRange> = None;
    for r in &ranges {
        if let Some(prev) = furthest {
            if r.lo <= prev.hi {
                // The first id both claim: `r` starts inside `prev`, so it is
                // `r`'s own first id.
                let zxy = tile_id_to_zxy(r.lo)
                    .map(|(z, x, y)| format!("z{z}/{x}/{y}"))
                    .unwrap_or_else(|_| "?".to_string());
                return Err(Error::InvalidConfig(format!(
                    "{} and {} claim overlapping tile ids: {} holds ids {}..={} and {} holds \
                     {}..={}, colliding first at tile id {} ({zxy}). merge concatenates \
                     disjoint shards — it will not reconcile a tile two inputs both claim, so \
                     check how the shards were cut (a shard listed twice, or one left over \
                     from an earlier run, is the usual cause)",
                    indexes[prev.src].path().display(),
                    indexes[r.src].path().display(),
                    indexes[prev.src].path().display(),
                    prev.lo,
                    prev.hi,
                    indexes[r.src].path().display(),
                    r.lo,
                    r.hi,
                    r.lo,
                )));
            }
            if r.hi > prev.hi {
                furthest = Some(r);
            }
        } else {
            furthest = Some(r);
        }
    }
    Ok(())
}

/// Every `vector_layers` entry every input declares, in input order.
///
/// Unparseable or absent metadata is not fatal — the tiles are still
/// copyable, they just describe no fields — but it is worth a warning, since
/// a merged archive that declares no layers renders as nothing in most
/// clients.
fn collect_layers(indexes: &[ArchiveIndex]) -> Vec<LayerMeta> {
    let mut layers = Vec::new();
    for idx in indexes {
        let Some(meta) = idx.metadata_json() else {
            log::warn!(
                "{}: metadata is absent or not valid JSON; its layers are not declared in the \
                 merged archive",
                idx.path().display()
            );
            continue;
        };
        let Some(arr) = meta.get("vector_layers").and_then(Value::as_array) else {
            log::warn!(
                "{}: metadata declares no vector_layers; its layers are not declared in the \
                 merged archive",
                idx.path().display()
            );
            continue;
        };
        for l in arr {
            let Some(id) = l.get("id").and_then(Value::as_str) else {
                continue;
            };
            layers.push(LayerMeta {
                id: id.to_string(),
                minzoom: l
                    .get("minzoom")
                    .and_then(Value::as_u64)
                    .unwrap_or(u64::from(idx.header().min_zoom)) as u8,
                maxzoom: l
                    .get("maxzoom")
                    .and_then(Value::as_u64)
                    .unwrap_or(u64::from(idx.header().max_zoom)) as u8,
                fields: l
                    .get("fields")
                    .filter(|f| f.is_object())
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default())),
            });
        }
    }
    layers
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::pmtiles_writer::{verify_clustered, Header};

    /// A shard-like archive: one layer, one zoom band, the given tiles.
    fn write_shard(
        path: &Path,
        layer: &str,
        tiles: &[(u8, u32, u32)],
        bounds: TileBounds,
        fields: &[(&str, &str)],
    ) {
        write_shard_with(path, layer, tiles, Some(bounds), fields, Compression::Gzip);
    }

    fn write_shard_with(
        path: &Path,
        layer: &str,
        tiles: &[(u8, u32, u32)],
        bounds: Option<TileBounds>,
        fields: &[(&str, &str)],
        compression: Compression,
    ) {
        let mut w = StreamingPmtilesWriter::new(compression).unwrap();
        w.set_layer_name(layer);
        if let Some(b) = bounds {
            w.set_bounds(&b);
        }
        if !fields.is_empty() {
            w.set_fields(
                fields
                    .iter()
                    .map(|&(k, v)| (k.to_string(), v.to_string()))
                    .collect::<HashMap<String, String>>(),
            );
        }
        for (i, (z, x, y)) in tiles.iter().enumerate() {
            // Distinct per tile so dedup cannot hide a mixed-up copy.
            w.add_tile(*z, *x, *y, &[0x1a, 0x02, 0x08, i as u8])
                .unwrap();
        }
        w.finalize(path).unwrap();
    }

    /// Every (z, x, y) → tile bytes an archive holds, read the slow way.
    fn tiles_of(path: &Path) -> BTreeMap<(u8, u32, u32), Vec<u8>> {
        let idx = ArchiveIndex::open(path).unwrap();
        idx.tiles()
            .map(|t| {
                let t = t.unwrap();
                ((t.z, t.x, t.y), idx.read_range(t.range).unwrap())
            })
            .collect()
    }

    fn header_of(path: &Path) -> Header {
        Header::from_bytes(&std::fs::read(path).unwrap()).unwrap()
    }

    fn metadata_of(path: &Path) -> Value {
        ArchiveIndex::open(path).unwrap().metadata_json().unwrap()
    }

    /// The merge's contract: every input tile is present, byte for byte, and
    /// the output is genuinely clustered — not merely stamped so. Clustered
    /// is what lets a client stream the archive in directory order, and it is
    /// the property a naive "append each archive in argument order" merge
    /// breaks the moment the arguments are not in tile-id order.
    #[test]
    fn merge_shards_output_is_clustered_and_complete() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        let b = dir.path().join("b.pmtiles");
        let c = dir.path().join("c.pmtiles");
        // Deliberately not in id order, and z6 before z5 across shards: the
        // merge must reorder, not concatenate.
        write_shard(
            &b,
            "l",
            &[(6, 40, 40), (6, 41, 41)],
            TileBounds::new(0.0, 0.0, 1.0, 1.0),
            &[],
        );
        write_shard(
            &a,
            "l",
            &[(6, 0, 0), (6, 1, 1)],
            TileBounds::new(-2.0, -2.0, -1.0, -1.0),
            &[],
        );
        write_shard(
            &c,
            "l",
            &[(6, 60, 60)],
            TileBounds::new(2.0, 2.0, 3.0, 3.0),
            &[],
        );

        let out = dir.path().join("merged.pmtiles");
        let report = merge_shards(
            &[b.clone(), a.clone(), c.clone()],
            &out,
            &MergeOptions::default(),
        )
        .unwrap();

        assert!(
            verify_clustered(&out).unwrap(),
            "the merged archive must be genuinely clustered"
        );
        assert!(header_of(&out).clustered, "and stamped so");

        let mut expected = BTreeMap::new();
        for shard in [&a, &b, &c] {
            expected.extend(tiles_of(shard));
        }
        assert_eq!(tiles_of(&out), expected);

        assert_eq!(report.tiles_total, 5);
        assert_eq!(report.inputs, 3);
        assert_eq!(report.per_zoom_tile_counts, BTreeMap::from([(6u8, 5u64)]));
        assert!(report.inputs_without_bounds.is_empty());
    }

    /// The check that exists so a mis-cut shard set fails before the merge
    /// burns hours, rather than producing an archive with silently shadowed
    /// tiles. The message has to name BOTH archives: "some inputs overlap" is
    /// unactionable across a hundred shards.
    #[test]
    fn merge_shards_rejects_overlapping_tile_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("east.pmtiles");
        let b = dir.path().join("west.pmtiles");
        let bounds = TileBounds::new(-1.0, -1.0, 1.0, 1.0);
        write_shard(&a, "l", &[(6, 0, 0), (6, 10, 10)], bounds, &[]);
        // Starts inside a's range.
        write_shard(&b, "l", &[(6, 5, 5), (6, 20, 20)], bounds, &[]);

        let out = dir.path().join("merged.pmtiles");
        let err = merge_shards(&[a.clone(), b.clone()], &out, &MergeOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("east.pmtiles"), "{err}");
        assert!(err.contains("west.pmtiles"), "{err}");
        assert!(err.contains("overlapping tile ids"), "{err}");
        assert!(!out.exists(), "a rejected merge must write nothing");
    }

    /// The same shard listed twice — the commonest way to mis-specify a shard
    /// set — is exactly an overlap, and must be caught by the same check.
    #[test]
    fn merge_shards_rejects_the_same_input_twice() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        write_shard(
            &a,
            "l",
            &[(6, 0, 0)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        let out = dir.path().join("merged.pmtiles");
        let err = merge_shards(&[a.clone(), a.clone()], &out, &MergeOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("overlapping tile ids"), "{err}");
    }

    /// A header with no usable bounds reads back inverted (min > max) and
    /// would swallow every real input's extent if unioned. It is dropped and
    /// NAMED, so an operator can tell which shard to look at.
    #[test]
    fn merge_shards_warns_and_reports_input_without_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.pmtiles");
        let blank = dir.path().join("blank.pmtiles");
        write_shard(
            &good,
            "l",
            &[(6, 0, 0)],
            TileBounds::new(-10.0, -5.0, 0.0, 5.0),
            &[],
        );
        // No set_bounds at all: TileBounds::empty() saturates into the header
        // and reads back inverted.
        write_shard_with(&blank, "l", &[(6, 40, 40)], None, &[], Compression::Gzip);

        let out = dir.path().join("merged.pmtiles");
        let report = merge_shards(
            &[good.clone(), blank.clone()],
            &out,
            &MergeOptions::default(),
        )
        .unwrap();

        assert_eq!(report.inputs_without_bounds.len(), 1, "{report:?}");
        assert!(
            report.inputs_without_bounds[0].contains("blank"),
            "{:?}",
            report.inputs_without_bounds
        );
        // The union still reflects the one input that did carry bounds.
        let h = header_of(&out);
        assert!((h.min_lon + 10.0).abs() < 1e-6, "min_lon {}", h.min_lon);
        assert!((h.max_lon - 0.0).abs() < 1e-6, "max_lon {}", h.max_lon);
    }

    /// The merged zoom range is the union of what the inputs DECLARE, not of
    /// the zooms that happened to hold tiles: a shard covering a sliver of
    /// the world legitimately has no tile at the build's coarsest or deepest
    /// zoom, and narrowing the header to what was copied would hide zooms the
    /// build genuinely produces.
    #[test]
    fn merge_shards_header_zoom_range_is_union() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        let b = dir.path().join("b.pmtiles");
        let bounds = TileBounds::new(-1.0, -1.0, 1.0, 1.0);
        // `a` declares z2-z4 but only holds a z4 tile: #380's case, where a
        // band's coarse zooms generalized away to nothing.
        let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        w.set_layer_name("l");
        w.set_bounds(&bounds);
        w.set_declared_min_zoom(2);
        w.add_tile(4, 0, 0, &[0x1a, 0x02, 0x08, 0x01]).unwrap();
        w.finalize(&a).unwrap();
        // `b` declares z5-z9 but only holds z5 and z7 — a shard covering a
        // sliver of the world, whose deepest zooms are empty out there. The
        // declaration is patched into the header byte a real export stamps
        // (byte 101), so this asserts against the FORMAT, not against the
        // writer API the merge happens to use.
        write_shard(&b, "l", &[(5, 20, 20), (7, 100, 100)], bounds, &[]);
        let mut raw = std::fs::read(&b).unwrap();
        raw[101] = 9;
        std::fs::write(&b, &raw).unwrap();

        assert_eq!((header_of(&a).min_zoom, header_of(&a).max_zoom), (2, 4));
        assert_eq!((header_of(&b).min_zoom, header_of(&b).max_zoom), (5, 9));

        let out = dir.path().join("merged.pmtiles");
        let report = merge_shards(&[a.clone(), b.clone()], &out, &MergeOptions::default()).unwrap();

        // z2 comes from a's declaration (its coarsest tile is z4) and z9 from
        // b's (its deepest tile is z7): the union of what the inputs declare,
        // not of what they hold.
        let h = header_of(&out);
        assert_eq!((h.min_zoom, h.max_zoom), (2, 9), "{h:?}");
        assert_eq!((report.min_zoom, report.max_zoom), (2, 9));
        // `vector_layers` is unioned from the inputs' own layer declarations,
        // which is a different (and narrower) thing from the header's zoom
        // range: this fixture patched only b's header byte, so its layer
        // still declares z5-z7 and the merged layer spans z2-z7. A real
        // export writes the two consistently; the merge does not invent a
        // layer range the inputs never declared.
        let layer = metadata_of(&out)["vector_layers"][0].clone();
        assert_eq!(layer["minzoom"], json!(2), "{layer}");
        assert_eq!(layer["maxzoom"], json!(7), "{layer}");
        // The per-zoom counts, by contrast, report only the zooms that hold
        // tiles — they are the parity oracle, not a declaration.
        assert_eq!(
            report.per_zoom_tile_counts,
            BTreeMap::from([(4u8, 1u64), (5, 1), (7, 1)])
        );
    }

    /// Shards of one build share a layer, and #389 made even identical
    /// schemas legitimately diverge between them (a counter withheld where it
    /// never rose above 1). Declaring the layer twice, or with one shard's
    /// field set, would hide attributes from every client that introspects
    /// `vector_layers`.
    #[test]
    fn merge_shards_vector_layers_union_fields() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        let b = dir.path().join("b.pmtiles");
        let bounds = TileBounds::new(-1.0, -1.0, 1.0, 1.0);
        write_shard(&a, "roads", &[(6, 0, 0)], bounds, &[("name", "String")]);
        write_shard(
            &b,
            "roads",
            &[(6, 40, 40)],
            bounds,
            &[("lanes", "Number"), ("name", "String")],
        );

        let out = dir.path().join("merged.pmtiles");
        merge_shards(&[a, b], &out, &MergeOptions::default()).unwrap();

        let layers = metadata_of(&out)["vector_layers"].clone();
        let arr = layers.as_array().unwrap();
        assert_eq!(arr.len(), 1, "one layer, not one per shard: {layers}");
        assert_eq!(arr[0]["id"], json!("roads"));
        assert_eq!(arr[0]["fields"]["name"], json!("String"));
        assert_eq!(arr[0]["fields"]["lanes"], json!("Number"));
    }

    /// Bodies are copied verbatim under one header that declares one
    /// compression for all of them, so a mixed set would mislabel every tile
    /// of whichever shard lost. Refused up front, naming both.
    #[test]
    fn merge_shards_rejects_mixed_compression() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("gzip.pmtiles");
        let b = dir.path().join("zstd.pmtiles");
        let bounds = Some(TileBounds::new(-1.0, -1.0, 1.0, 1.0));
        write_shard_with(&a, "l", &[(6, 0, 0)], bounds, &[], Compression::Gzip);
        write_shard_with(&b, "l", &[(6, 40, 40)], bounds, &[], Compression::Zstd);

        let out = dir.path().join("merged.pmtiles");
        let err = merge_shards(&[a, b], &out, &MergeOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("tile compression"), "{err}");
        assert!(err.contains("gzip.pmtiles"), "{err}");
        assert!(err.contains("zstd.pmtiles"), "{err}");
    }

    /// A merge with nothing to merge is a caller bug, not an empty archive.
    #[test]
    fn merge_shards_rejects_no_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("merged.pmtiles");
        let err = merge_shards(&[], &out, &MergeOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least one input"), "{err}");
    }
}
