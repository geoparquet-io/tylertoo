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
//! The intermediate overview Parquet (`--keep-overview`) IS now byte-compared
//! too (tightened once #508 landed). It used to be excluded: the GeoParquet
//! footer's `geometry_types` field serialized a `HashSet<String>` (see
//! `overview::writer`), whose raw JSON element order varied run to run
//! independent of anything this ticket touches — confirmed empirically while
//! building this test, the kept `--keep-overview` Parquet differed by a
//! handful of bytes at RAYON_NUM_THREADS=8 (the `geometry_types` array in a
//! different order) even though the exported PMTiles archive was still
//! byte-identical. #508 fixed that source at the writer
//! (`geo_metadata_json_deterministic` sorts the array before it is ever
//! serialized), so the overview file's raw bytes are now expected to be
//! stable across thread counts too, and this test checks that directly
//! rather than only inferring it from the exported PMTiles. `tiles_facade.rs`'s
//! `tiles_keep_overview_retains_and_matches_two_step` test still only
//! byte-compares the *PMTiles* output (a one-step-vs-two-step parity check,
//! not a thread-count one) — that is an unrelated, narrower guarantee and is
//! unaffected by this change. The PMTiles archive remains the primary
//! byte-determinism promise in ARCHITECTURE.md, OVERVIEW_TUNING.md,
//! bounded-memory.md, and cli.md, and what #423 asks this test to cover; the
//! overview-file comparison is additional coverage, not a replacement.

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
/// `RAYON_NUM_THREADS`, retaining the intermediate overview at `overview_out`
/// (`--keep-overview`, #508), and returns `(pmtiles bytes, overview parquet
/// bytes)`.
///
/// `max_zoom` is per-caller: the `open-buildings` arm uses 14 (its features
/// are tiny and only separate at high zoom), the `#460` arm a much lower one
/// (its fixture is 17k admin polygons — the point there is the pass-1 row
/// count, not the tile depth, and a deep pyramid would only make the test
/// slow).
fn run_tiles(
    fixture: &Path,
    out: &Path,
    overview_out: &Path,
    threads: u32,
    no_streaming: bool,
    max_zoom: u8,
) -> (Vec<u8>, Vec<u8>) {
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
        "--keep-overview".to_string(),
        overview_out.to_str().unwrap().to_string(),
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
    let overview_bytes = std::fs::read(overview_out).expect("read kept overview output");
    assert!(
        overview_bytes.starts_with(b"PAR1"),
        "RAYON_NUM_THREADS={threads} no_streaming={no_streaming}: kept overview does not start \
         with the parquet magic"
    );
    (bytes, overview_bytes)
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
        let mut baseline: Option<(u32, Vec<u8>, Vec<u8>)> = None;

        for threads in [1u32, 2, 8] {
            let out = dir.path().join(format!("{engine}-t{threads}.pmtiles"));
            let overview_out = dir
                .path()
                .join(format!("{engine}-t{threads}-overview.parquet"));
            let (bytes, overview_bytes) =
                run_tiles(&fixture, &out, &overview_out, threads, no_streaming, 14);

            match &baseline {
                None => baseline = Some((threads, bytes, overview_bytes)),
                Some((base_threads, base_bytes, base_overview_bytes)) => {
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
                    // #508: the intermediate overview's raw bytes must also be
                    // stable across thread counts now that the writer sorts
                    // `geometry_types` before serializing the footer.
                    assert!(
                        base_overview_bytes == &overview_bytes,
                        "[{engine}] kept overview Parquet differs between \
                         RAYON_NUM_THREADS={base_threads} ({} bytes) and RAYON_NUM_THREADS={threads} \
                         ({} bytes) — #508 regression: the geo footer's `geometry_types` (or some \
                         other HashMap/HashSet-backed field) is leaking nondeterministic order again",
                        base_overview_bytes.len(),
                        overview_bytes.len()
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

    let mut baseline: Option<(u32, Vec<u8>, Vec<u8>)> = None;
    for threads in [1u32, 2, 8] {
        let out = dir.path().join(format!("mada-t{threads}.pmtiles"));
        let overview_out = dir.path().join(format!("mada-t{threads}-overview.parquet"));
        let (bytes, overview_bytes) =
            run_tiles(&fixture, &out, &overview_out, threads, false, MAX_ZOOM);

        match &baseline {
            None => baseline = Some((threads, bytes, overview_bytes)),
            Some((base_threads, base_bytes, base_overview_bytes)) => {
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
                // The kept overview Parquet is the artifact pass 1 actually
                // produces, so compare it directly too (#508 made its footer
                // byte-stable; before that this would have failed for an
                // unrelated reason).
                assert!(
                    base_overview_bytes == &overview_bytes,
                    "kept overview Parquet differs between RAYON_NUM_THREADS={base_threads} \
                     and RAYON_NUM_THREADS={threads} ({} vs {} bytes) — the parallel pass-1 \
                     scan (#460) is not thread-count invariant",
                    base_overview_bytes.len(),
                    overview_bytes.len()
                );
            }
        }
    }
}
