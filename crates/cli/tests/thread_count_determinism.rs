//! #423: byte-determinism across thread counts.
//!
//! `overview::assign::level_winner_positions` used to drain its per-cell
//! winner `HashMap` straight into the winner `Vec` via
//! `grid.into_values()`. `HashMap` iteration order is a function of Rust's
//! randomized per-process hasher state, not of the input or of
//! `RAYON_NUM_THREADS` — so nothing here was ever literally "thread-count
//! dependent", but it was the one place in the assignment pass where raw
//! `HashMap` order reached a `Vec` at all, one refactor away from mattering
//! (the consumer today folds winners through a commutative min, so the
//! order was harmless). This test spawns the SAME conversion as separate
//! CLI processes with `RAYON_NUM_THREADS` pinned to 1, 2, and 8 (a fresh
//! rayon global pool per process, so no cross-run pool contamination) and
//! asserts the PMTiles archive is byte-identical across all three.
//!
//! Both the streaming and `--no-streaming` engines are exercised: the
//! per-level parallel winner-grid pass this ticket touches
//! (`overview::assign::assign_levels`) is shared code, run by both, and on
//! this fixture every run completes in well under a second, so covering
//! both costs nothing.
//!
//! The intermediate overview Parquet is deliberately **not** byte-compared
//! here. Its GeoParquet footer's `geometry_types` field serializes a
//! `HashSet<String>` (see `overview::writer`, whose test helpers
//! `sorted_geometry_types` / `footer_geometry_types` exist specifically
//! because that field's raw JSON is "not byte-comparable" — its own doc
//! comment says so), so the raw overview bytes are not expected to be
//! stable across process runs independent of anything this ticket fixes.
//! That is a pre-existing, already-documented, and already-worked-around
//! source of byte drift, unrelated to `assign.rs`'s `HashMap`. Confirmed
//! empirically while building this test: the kept `--keep-overview`
//! Parquet differed by a handful of bytes at RAYON_NUM_THREADS=8 (the
//! `geometry_types` array elements in a different order) even though the
//! exported PMTiles archive was still byte-identical. `tiles_facade.rs`'s
//! own `tiles_keep_overview_retains_and_matches_two_step` test follows the
//! same rule: it byte-compares the *PMTiles* output, never the
//! intermediate overview file's raw bytes. The PMTiles archive is what the
//! byte-determinism promise in ARCHITECTURE.md, OVERVIEW_TUNING.md,
//! bounded-memory.md, and cli.md is actually about, and what #423 asks
//! this test to cover.

use std::path::Path;
use std::process::Command;

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

/// PMTiles v3 archives start with the 7-byte magic "PMTiles" followed by the
/// spec version byte 3.
const PMTILES_MAGIC: &[u8] = b"PMTiles\x03";

fn tylertoo_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tylertoo")
}

/// Runs one `tiles` conversion of `fixture` to `out` with a pinned
/// `RAYON_NUM_THREADS`, and returns the PMTiles bytes.
///
/// `max_zoom` is per-caller: the `open-buildings` arm uses 14 (its features
/// are tiny and only separate at high zoom), the `#460` arm a much lower one
/// (its fixture is 17k admin polygons — the point there is the pass-1 row
/// count, not the tile depth, and a deep pyramid would only make the test
/// slow).
fn run_tiles(
    fixture: &Path,
    out: &Path,
    threads: u32,
    no_streaming: bool,
    max_zoom: u8,
) -> Vec<u8> {
    let mut args = vec![
        "tiles".to_string(),
        fixture.to_str().unwrap().to_string(),
        out.to_str().unwrap().to_string(),
        "--min-zoom".to_string(),
        "0".to_string(),
        "--max-zoom".to_string(),
        max_zoom.to_string(),
        // Fixed so the streaming and non-streaming runs (and every thread
        // count) share one layer name and are directly comparable.
        "--layer-name".to_string(),
        "det423".to_string(),
    ];
    if no_streaming {
        args.push("--no-streaming".to_string());
    }

    let output = Command::new(tylertoo_bin())
        .args(&args)
        // One rayon global pool per spawned process: pinning via the env
        // var here, rather than a `RAYON_NUM_THREADS`-setting-in-process
        // trick, avoids contaminating a shared pool across iterations of
        // this test (rayon's global pool can only be configured once per
        // process).
        .env("RAYON_NUM_THREADS", threads.to_string())
        .output()
        .unwrap_or_else(|e| panic!("run tylertoo tiles (threads={threads}): {e}"));
    assert!(
        output.status.success(),
        "RAYON_NUM_THREADS={threads} no_streaming={no_streaming} exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let bytes = std::fs::read(out).expect("read pmtiles output");
    assert!(
        bytes.starts_with(PMTILES_MAGIC),
        "RAYON_NUM_THREADS={threads} no_streaming={no_streaming}: output does not start with \
         the PMTiles v3 magic"
    );
    bytes
}

#[test]
fn pmtiles_output_is_byte_identical_across_thread_counts() {
    let Some(fixture) = fixture::realdata("open-buildings.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");

    // Both engines share the `assign_levels` code path this ticket fixes.
    for no_streaming in [false, true] {
        let engine = if no_streaming {
            "no-streaming"
        } else {
            "streaming"
        };
        let mut baseline: Option<(u32, Vec<u8>)> = None;

        for threads in [1u32, 2, 8] {
            let out = dir.path().join(format!("{engine}-t{threads}.pmtiles"));
            let bytes = run_tiles(&fixture, &out, threads, no_streaming, 14);

            match &baseline {
                None => baseline = Some((threads, bytes)),
                Some((base_threads, base_bytes)) => {
                    assert_eq!(
                        base_bytes.len(),
                        bytes.len(),
                        "[{engine}] PMTiles size differs between RAYON_NUM_THREADS={base_threads} \
                         ({} bytes) and RAYON_NUM_THREADS={threads} ({} bytes) — #423 \
                         byte-determinism regression",
                        base_bytes.len(),
                        bytes.len()
                    );
                    assert!(
                        base_bytes == &bytes,
                        "[{engine}] PMTiles output differs between RAYON_NUM_THREADS={base_threads} \
                         and RAYON_NUM_THREADS={threads} (same {} byte length, different content) \
                         — #423 byte-determinism regression: a HashMap/HashSet iteration order is \
                         probably leaking into the output again",
                        bytes.len()
                    );
                }
            }
        }
    }
}

/// #460 review (S2-X1 / S3-b): the arm above is **vacuous for the parallel
/// pass-1 scan**. `open-buildings.parquet` holds 1000 rows — fewer than one
/// pass-1 scan chunk — so every batch is a single chunk at every thread
/// count, and the `par_iter` it is supposed to guard never has more than one
/// task to order. A merge or rebase that broke chunk-index rebasing would
/// sail straight through it.
///
/// `fieldmaps-madagascar-adm4.parquet` (17,465 rows) splits into real chunks,
/// so the chunked merge, the interner/ladder serial seam, and the
/// `stable_hash(index)` tie-break all actually vary with the thread count
/// here. Streaming only: `--no-streaming` runs the in-memory pipeline, which
/// has no pass-1 chunking to guard (the arm above already covers that engine
/// for `assign_levels`).
#[test]
fn parallel_pass1_output_is_byte_identical_across_thread_counts() {
    let Some(fixture) = fixture::realdata("fieldmaps-madagascar-adm4.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");

    // Low max-zoom: the pass-1 scan (what chunks) reads every input row
    // regardless of zoom, while pass 2 and the tile build scale with the
    // pyramid depth — so a shallow pyramid keeps this test to a few seconds
    // per run without weakening the coverage it exists for.
    const MAX_ZOOM: u8 = 8;

    let mut baseline: Option<(u32, Vec<u8>)> = None;
    for threads in [1u32, 2, 8] {
        let out = dir.path().join(format!("mada-t{threads}.pmtiles"));
        let bytes = run_tiles(&fixture, &out, threads, false, MAX_ZOOM);

        match &baseline {
            None => baseline = Some((threads, bytes)),
            Some((base_threads, base_bytes)) => {
                assert_eq!(
                    base_bytes.len(),
                    bytes.len(),
                    "PMTiles size differs between RAYON_NUM_THREADS={base_threads} ({} bytes) \
                     and RAYON_NUM_THREADS={threads} ({} bytes) — the parallel pass-1 scan \
                     (#460) is not thread-count invariant",
                    base_bytes.len(),
                    bytes.len()
                );
                assert!(
                    base_bytes == &bytes,
                    "PMTiles output differs between RAYON_NUM_THREADS={base_threads} and \
                     RAYON_NUM_THREADS={threads} (same {} byte length, different content) — \
                     the parallel pass-1 scan (#460) is not thread-count invariant: a \
                     chunk-local index is probably reaching an accumulator unrebased, or a \
                     per-chunk result is being merged out of order",
                    bytes.len()
                );
            }
        }
        // The kept overview Parquet is NOT compared here — see the module
        // docs: its GeoParquet footer's `geometry_types` still serializes a
        // `HashSet`. Once that is deterministic, this arm should compare it
        // too (it is the artifact pass 1 actually produces).
    }
}
