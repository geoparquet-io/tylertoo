//! #422: byte-identity of the streaming engine's output across
//! `MemoryProfile` × `in_flight_batches` on real fixtures-v1 inputs.
//!
//! `overview::stream`'s module doc (and `resolve_in_flight_batches`'s) promise
//! that neither knob reaches the output: the memory profile only picks the
//! pass-2 sink backing (RAM vs Arrow IPC spill, #294) and the in-flight depth
//! only sets read/compute overlap. The in-tree sweep in `convert::tests`
//! (`pipelined_matches_serial_across_profiles`) varies the profile on tiny
//! synthetic inputs and compares structurally; nothing varied the in-flight
//! depth, and nothing ran the sweep over a real file. This does both and
//! compares raw bytes of the overview Parquet, the way
//! `crates/cli/tests/thread_count_determinism.rs` does for thread counts.
//!
//! Slow set (`.config/nextest.toml`): every case is a full conversion of a
//! real fixture, run once per profile × depth combination.

use std::path::{Path, PathBuf};

use tylertoo_core::overview::convert::{
    convert_to_overviews, ConvertOptions, LevelPlan, IN_FLIGHT_BATCHES_AUTO, IN_FLIGHT_BATCHES_MAX,
};
use tylertoo_core::overview::level::{MemoryProfile, Mode};

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

/// One conversion; returns the overview file's raw bytes.
fn convert_bytes(input: &Path, out: &Path, options: &ConvertOptions) -> Vec<u8> {
    let report = convert_to_overviews(input, out, options).unwrap_or_else(|e| {
        panic!(
            "convert {} (profile={:?}, in_flight={}): {e}",
            input.display(),
            options.profile,
            options.in_flight_batches
        )
    });
    assert!(
        !report.levels.is_empty(),
        "{}: a conversion with no levels proves nothing",
        input.display()
    );
    let bytes = std::fs::read(out).expect("read overview output");
    assert!(
        bytes.starts_with(b"PAR1") && bytes.ends_with(b"PAR1"),
        "{}: output is not a Parquet file",
        input.display()
    );
    bytes
}

/// Run `base` over every profile × in-flight depth and assert every output
/// is byte-identical to the first.
fn assert_parity(name: &str, input: &Path, base: &ConvertOptions) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut baseline: Option<(String, Vec<u8>)> = None;
    for profile in [
        MemoryProfile::Auto,
        MemoryProfile::Speed,
        MemoryProfile::Bounded,
    ] {
        for in_flight in [IN_FLIGHT_BATCHES_AUTO, 1, 2, IN_FLIGHT_BATCHES_MAX] {
            let label = format!("{name}/profile={profile:?}/in_flight={in_flight}");
            let options = ConvertOptions {
                profile,
                in_flight_batches: in_flight,
                ..base.clone()
            };
            let out = dir
                .path()
                .join(format!("{name}-{profile:?}-{in_flight}.parquet"));
            let started = std::time::Instant::now();
            let bytes = convert_bytes(input, &out, &options);
            eprintln!(
                "{label}: {} bytes in {:.1}s",
                bytes.len(),
                started.elapsed().as_secs_f64()
            );
            match &baseline {
                None => baseline = Some((label, bytes)),
                Some((base_label, base_bytes)) => {
                    assert_eq!(
                        base_bytes.len(),
                        bytes.len(),
                        "{label}: overview size differs from {base_label} ({} vs {} bytes) — \
                         the memory profile or the in-flight depth reached the output",
                        base_bytes.len(),
                        bytes.len()
                    );
                    assert!(
                        base_bytes == &bytes,
                        "{label}: overview bytes differ from {base_label} (same length, \
                         different content) — the memory profile or the in-flight depth \
                         reached the output"
                    );
                }
            }
        }
    }
}

/// Small read batches so the in-flight depth actually governs how many
/// batches are ever resident at once (1000 rows / 64 = 16 batches; a single
/// batch would make every depth equivalent by construction).
fn base_options(min_zoom: u8, max_zoom: u8, read_batch_size: usize) -> ConvertOptions {
    ConvertOptions {
        mode: Mode::Duplicating,
        levels: LevelPlan::ZoomRange { min_zoom, max_zoom },
        read_batch_size,
        ..ConvertOptions::default()
    }
}

fn fixture_or_skip(name: &str) -> Option<PathBuf> {
    fixture::realdata(name)
}

/// 1,000 building polygons: the quick real-data fixture. Deep enough a
/// pyramid that several levels simplify and the finest is streamed verbatim.
#[test]
fn open_buildings_output_is_byte_identical_across_profiles_and_in_flight() {
    let Some(input) = fixture_or_skip("open-buildings.parquet") else {
        return;
    };
    assert_parity("open-buildings", &input, &base_options(4, 12, 64));
}

/// ~1,000 road detections: the line fixture, so line coalescing (on by
/// default) and its pass-1 scratch run under every profile too.
#[test]
fn road_detections_output_is_byte_identical_across_profiles_and_in_flight() {
    let Some(input) = fixture_or_skip("road-detections.parquet") else {
        return;
    };
    assert_parity("road-detections", &input, &base_options(4, 12, 64));
}

/// 17,465 admin polygons (28 MB): the fixture with real row-group structure
/// and a pass-1 scan that genuinely chunks, so `Bounded`'s spill decision and
/// the in-flight depth both have something to bite on. Shallow pyramid: the
/// scan reads every row whatever the zoom, and the point is the engine
/// plumbing, not tile depth.
#[test]
fn madagascar_adm4_output_is_byte_identical_across_profiles_and_in_flight() {
    let Some(input) = fixture_or_skip("fieldmaps-madagascar-adm4.parquet") else {
        return;
    };
    assert_parity("madagascar-adm4", &input, &base_options(0, 6, 2048));
}
