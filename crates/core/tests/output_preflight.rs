//! #452: every public entry point that writes a file checks that the output
//! can be written before it reads a byte of input. Each case passes an input
//! that does not exist and an output in a directory that does not exist: the
//! output error coming back (not the input one) proves the order, and the
//! message must name the output's directory.

use std::path::{Path, PathBuf};

use tylertoo_core::decode::{decode_pmtiles, DecodeOptions};
use tylertoo_core::merge::{merge_shards, MergeOptions};
use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions};
use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};
use tylertoo_core::overview::tiles::{convert_to_pmtiles, TilesInput, TilesOptions};
use tylertoo_core::pyramid::{build_pyramid, merge_bands_with_options, Band, PyramidOptions};

struct Case {
    _dir: tempfile::TempDir,
    input: PathBuf,
    missing_dir: PathBuf,
}

impl Case {
    fn new(input_name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join(input_name);
        let missing_dir = dir.path().join("no-such-dir");
        Self {
            _dir: dir,
            input,
            missing_dir,
        }
    }

    fn output(&self, name: &str) -> PathBuf {
        self.missing_dir.join(name)
    }

    #[track_caller]
    fn assert_output_error(&self, what: &str, msg: &str) {
        assert!(
            msg.contains(&self.missing_dir.display().to_string()) && msg.contains("does not exist"),
            "{what}: expected the missing output directory, got: {msg}"
        );
        assert!(
            !msg.contains(&self.input.display().to_string()),
            "{what}: the input was read before the output was checked: {msg}"
        );
    }
}

fn band(input: &Path) -> Band {
    Band {
        input: input.to_path_buf(),
        layer: "l".to_string(),
        min_zoom: 0,
        max_zoom: 2,
    }
}

#[test]
fn convert_to_overviews_checks_the_output_first() {
    let c = Case::new("in.parquet");
    let err = convert_to_overviews(
        &c.input,
        c.output("out.parquet"),
        &ConvertOptions::default(),
    )
    .unwrap_err();
    c.assert_output_error("convert_to_overviews", &err.to_string());
}

#[test]
fn convert_to_pmtiles_checks_the_output_first() {
    let c = Case::new("in.parquet");
    let err = convert_to_pmtiles(
        &TilesInput::Path(c.input.clone()),
        &c.output("out.pmtiles"),
        &TilesOptions::default(),
    )
    .unwrap_err();
    c.assert_output_error("convert_to_pmtiles", &err.to_string());
}

#[test]
fn export_pmtiles_checks_the_output_first() {
    let c = Case::new("in.parquet");
    let err =
        export_pmtiles(&c.input, c.output("out.pmtiles"), &ExportOptions::default()).unwrap_err();
    c.assert_output_error("export_pmtiles", &err.to_string());
}

#[test]
fn decode_pmtiles_checks_the_output_first() {
    let c = Case::new("in.pmtiles");
    let err =
        decode_pmtiles(&c.input, c.output("out.parquet"), &DecodeOptions::default()).unwrap_err();
    // DecodeError keeps the cause in `source()` (#396), so read the chain.
    let mut msg = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(e) = source {
        msg.push_str(&format!(": {e}"));
        source = e.source();
    }
    c.assert_output_error("decode_pmtiles", &msg);
}

#[test]
fn merge_shards_checks_the_output_first() {
    let c = Case::new("in.pmtiles");
    let err = merge_shards(
        std::slice::from_ref(&c.input),
        &c.output("out.pmtiles"),
        &MergeOptions::default(),
    )
    .unwrap_err();
    c.assert_output_error("merge_shards", &err.to_string());
}

#[test]
fn build_pyramid_checks_the_output_first() {
    let c = Case::new("in.parquet");
    let err = build_pyramid(
        &[band(&c.input)],
        &c.output("out.pmtiles"),
        &PyramidOptions::default(),
    )
    .unwrap_err();
    c.assert_output_error("build_pyramid", &err.to_string());

    let c = Case::new("in.pmtiles");
    let err =
        merge_bands_with_options(&[band(&c.input)], &c.output("out.pmtiles"), false).unwrap_err();
    c.assert_output_error("merge_bands_with_options", &err.to_string());
}
