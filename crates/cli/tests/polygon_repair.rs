//! Real coarse-zoom footprints lost by the pre-overlay pinch splitter (#779).
use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;
use parquet::arrow::ArrowWriter;
use std::process::Command;
use tylertoo_core::untrusted::open_parquet;

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "#779: extracts trusted fixture rows to temporary files"
)]
fn madagascar_adm4_small_features_survive_verbatim_at_z2() {
    let Some(path) = fixture::realdata("fieldmaps-madagascar-adm4.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut found = Vec::new();
    for batch in open_parquet(std::fs::File::open(path).unwrap())
        .unwrap()
        .build()
        .unwrap()
    {
        let batch = batch.unwrap();
        let fid = batch
            .column_by_name("fid")
            .unwrap()
            .as_primitive::<Int64Type>();
        for row in 0..batch.num_rows() {
            let id = fid.value(row);
            if ![259735, 263349].contains(&id) {
                continue;
            }
            found.push(id);
            let input = dir.path().join(format!("{id}.parquet"));
            let output = dir.path().join(format!("{id}.pmtiles"));
            let mut writer =
                ArrowWriter::try_new(std::fs::File::create(&input).unwrap(), batch.schema(), None)
                    .unwrap();
            writer.write(&batch.slice(row, 1)).unwrap();
            writer.close().unwrap();
            let result = Command::new(env!("CARGO_BIN_EXE_tylertoo"))
                .arg("tiles")
                .arg(&input)
                .arg(&output)
                .args(["--verbatim", "--min-zoom", "2", "--max-zoom", "2"])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "fid={id}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let decoded = dir.path().join(format!("{id}-decoded.parquet"));
            tylertoo_core::decode::decode_pmtiles(&output, &decoded, &Default::default()).unwrap();
            let rows: usize = open_parquet(std::fs::File::open(decoded).unwrap())
                .unwrap()
                .build()
                .unwrap()
                .map(|b| b.unwrap().num_rows())
                .sum();
            assert!(rows > 0, "fid={id} lost its recoverable z2 footprint");
        }
    }
    found.sort_unstable();
    assert_eq!(found, [259735, 263349]);
}
