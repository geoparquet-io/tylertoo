//! A gap-free polygon coverage keeps every polygon at middle zooms at the
//! default settings (#745, #572).
//!
//! Before #745 the density budget thinned polygons like points, so the
//! Madagascar admin-4 fixture (17,465 polygons that tile the island) kept
//! 865 of them at z8 and drew about half the island. Tippecanoe keeps all
//! 17,465 there. This pins the default close to that: every polygon wider
//! than a pixel has its own feature at z8, and the size valve dropped
//! nothing.
#![allow(clippy::disallowed_methods)] // #779: test code; reads and writes its own trusted fixtures

use std::collections::HashSet;
use std::fs::File;

use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

#[test]
fn madagascar_adm4_keeps_every_polygon_at_z8_by_default() {
    use tylertoo_core::decode::{decode_pmtiles, DecodeOptions};
    use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
    use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};

    let Some(fixture_path) = fixture::realdata("fieldmaps-madagascar-adm4.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let overview = dir.path().join("overview.parquet");
    let archive = dir.path().join("out.pmtiles");
    let decoded = dir.path().join("z8.parquet");

    convert_to_overviews(
        &fixture_path,
        &overview,
        &ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 10,
            },
            ..Default::default()
        },
    )
    .expect("convert");
    let report = export_pmtiles(&overview, &archive, &ExportOptions::default()).expect("export");
    assert_eq!(
        report.valve_dropped_features, 0,
        "the size valve must merge, never drop, on this coverage"
    );

    decode_pmtiles(
        &archive,
        &decoded,
        &DecodeOptions {
            min_zoom: Some(8),
            max_zoom: Some(8),
            layer: None,
        },
    )
    .expect("decode z8");

    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(&decoded).unwrap())
        .unwrap()
        .build()
        .unwrap();
    let mut ids = HashSet::new();
    for batch in reader {
        let batch = batch.unwrap();
        let fid = batch
            .column_by_name("fid")
            .expect("fid property in the tiles");
        let fid = arrow_cast::cast(fid, &arrow_schema::DataType::Int64).unwrap();
        ids.extend(fid.as_primitive::<Int64Type>().iter().flatten());
    }
    // 17,456 measured. The other 9 are urban units 240-430 m across, under a
    // pixel at z8 (about 600 m): they fall below the visibility gate, and
    // placeholder squares, which carry a neighbour's id, draw their area.
    assert!(
        ids.len() >= 17_450,
        "admin-4 polygons must reach z8 at the default settings: {} of 17,465 \
         (865 before #745)",
        ids.len()
    );
}
