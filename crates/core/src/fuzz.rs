//! Fuzz hooks over public entry points (#633).
//!
//! Hooks that need a private parser live next to it (`decode`, `pyramid`,
//! `overview::plan_state`, ...). These ones only call public API, but keeping
//! their bodies in core rather than in `fuzz/fuzz_targets/` lets
//! `tests/fuzz_seeds.rs` replay the committed seeds through exactly what the
//! nightly run fuzzes. Only the `fuzz/` crate and that test enable the
//! `fuzzing` feature; nothing here is part of the public API baseline.

use std::io::Write as _;
use std::str::FromStr as _;

use crate::archive_index::ArchiveIndex;
use crate::compression::{decompress_capped, Compression};
use crate::shard::{ShardPlan, ShardRole, TileRange};
use crate::tile::MAX_ZOOM;

/// Largest output [`fuzz_decompress`] allows, so a bomb is an `Err` in
/// microseconds rather than a slow run.
const DECOMPRESS_CAP: u64 = 1 << 20;

/// Write `bytes` to a temp file: the readers below are path-based by design
/// (`pread`, #498), so this is how a byte input reaches them.
fn temp_file(bytes: &[u8]) -> std::io::Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(bytes)?;
    file.flush()?;
    Ok(file)
}

/// `bytes` as a PMTiles archive handed to `stats`, `merge` or `--band`,
/// through [`ArchiveIndex`] alone: header, directory walk, metadata JSON,
/// every addressed tile body, and [`crate::stats::compute_stats`].
///
/// `band_archive` reaches [`ArchiveIndex::open`] too, but `pyramid` rejects
/// an archive that is not gzip MVT right after it, so the rest of the index
/// was never fuzzed on any other kind of archive. Returns the tiles read.
///
/// # Errors
///
/// Any error the archive reader or the stats pass reports.
pub fn fuzz_archive_index(bytes: &[u8]) -> crate::Result<u64> {
    let file = temp_file(bytes)?;
    let index = ArchiveIndex::open(file.path())?;
    let _ = index.bounds();
    let _ = index.metadata_json();
    let _ = index.tile_id_range()?;
    let _ = index.actual_zoom_range()?;
    let mut tiles = 0u64;
    for tile in index.tiles() {
        index.read_range(tile?.range)?;
        tiles += 1;
    }
    crate::stats::compute_stats(&index, 10)?;
    Ok(tiles)
}

/// `data` as a compressed section of a PMTiles archive.
///
/// That is a directory, the metadata or a tile body. The first byte picks the
/// codec, the rest is the compressed bytes. Panics if [`decompress_capped`]
/// hands back more than its cap.
///
/// # Errors
///
/// The codec's error, or the cap's.
pub fn fuzz_decompress(data: &[u8]) -> std::io::Result<usize> {
    let Some((&codec, body)) = data.split_first() else {
        return Ok(0);
    };
    let compression = match codec % 4 {
        0 => Compression::None,
        1 => Compression::Gzip,
        2 => Compression::Brotli,
        _ => Compression::Zstd,
    };
    let out = decompress_capped(body, compression, DECOMPRESS_CAP)?;
    assert!(
        out.len() as u64 <= DECOMPRESS_CAP,
        "{compression:?} returned {} bytes past a {DECOMPRESS_CAP}-byte cap",
        out.len()
    );
    Ok(out.len())
}

/// `bytes` as a `--shard-plan` file: [`ShardPlan::load`] (JSON, format and
/// version, structure checks), then every query `convert --shard` makes of a
/// loaded plan.
///
/// # Errors
///
/// Any error loading or querying the plan.
pub fn fuzz_shard_plan(bytes: &[u8]) -> Result<u64, crate::shard::ShardError> {
    let file = temp_file(bytes).map_err(|source| crate::shard::ShardError::Io {
        path: "fuzz".to_string(),
        source,
    })?;
    let plan = ShardPlan::load(file.path())?;
    let _ = plan.cut_digest_hex();
    let _ = plan.check_max_zoom(MAX_ZOOM, file.path());
    let shards = plan.shards();
    let _ = plan.range_for(ShardRole::Coarse, file.path());
    let _ = plan.range_for(
        ShardRole::Shard {
            index: 0,
            shards: shards.max(1),
        },
        file.path(),
    );
    let mut tiles = 0;
    for i in 0..shards {
        tiles += exercise_tile_range(&plan.range(i)?);
    }
    Ok(tiles)
}

/// The queries made of a parsed [`TileRange`], for both its sources (a plan
/// file and the `--tile-range` flag). Returns the pivot tiles it spans.
fn exercise_tile_range(range: &TileRange) -> u64 {
    for z in 0..=MAX_ZOOM {
        let _ = range.ids_at(z);
    }
    let _ = range.tile_bounds();
    let _ = range.read_bounds();
    range.hi() - range.lo() + 1
}

/// `s` as the value of each string flag the CLI and Python bindings hand
/// to a core parser.
///
/// Those are `--bounds`, `--representation`, `--band`, `--tile-range`,
/// `--shard`, `--max-zoom`, `--feature-order`, and the URL scheme sniff every
/// input path goes through. They take what the person running the command
/// typed rather than a file, but a crash on a typo is still a crash.
pub fn fuzz_cli_args(s: &str) {
    let _ = crate::covering::parse_bounds(s);
    let _ = crate::overview::convert::parse_representation_spec(s);
    let _ = crate::pyramid::Band::parse(s);
    if let Ok(range) = TileRange::parse(s) {
        exercise_tile_range(&range);
    }
    let _ = ShardRole::parse(s);
    let _ = crate::overview::auto_zoom::MaxZoom::from_str(s);
    let _ = crate::overview::export::FeatureOrder::from_str(s);
    let _ = crate::input::url_scheme(s);
}
