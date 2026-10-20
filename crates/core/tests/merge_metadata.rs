//! Malformed shard metadata remains mergeable, with a warning for each input.
//! A separate binary keeps the process-global logger isolated from unit tests.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use serde_json::json;
use tylertoo_core::archive_index::ArchiveIndex;
use tylertoo_core::compression::{self, Compression};
use tylertoo_core::merge::{merge_shards, MergeOptions};
use tylertoo_core::overview::export::write_empty_archive;
use tylertoo_core::pmtiles_writer::{Header, StreamingPmtilesWriter, HEADER_BYTES};
use tylertoo_core::TilesetMetadata;

static WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if record.level() == log::Level::Warn {
            WARNINGS.lock().unwrap().push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

fn tiles_of(path: &Path) -> BTreeMap<(u8, u32, u32), Vec<u8>> {
    let index = ArchiveIndex::open(path).unwrap();
    index
        .tiles()
        .map(|tile| {
            let tile = tile.unwrap();
            (
                (tile.z, tile.x, tile.y),
                index.read_range(tile.range).unwrap(),
            )
        })
        .collect()
}

#[test]
fn unparseable_metadata_warns_and_keeps_tiles_without_empty_shard_zooms() {
    log::set_logger(&Capture).expect("the only logger in this binary");
    log::set_max_level(log::LevelFilter::Warn);

    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data.pmtiles");
    let empty = dir.path().join("empty.pmtiles");
    let out = dir.path().join("out.pmtiles");
    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
    writer.set_layer_name("parks");
    writer.set_metadata(TilesetMetadata::new().with_attribution("discarded"));
    writer.add_tile(6, 0, 0, &[0x1a, 0x02, 0x08, 0x00]).unwrap();
    writer.finalize(&data).unwrap();
    write_empty_archive(&empty, "parks", 0, 0, &TilesetMetadata::new()).unwrap();

    // Missing metadata and invalid JSON carry no fields, whether or not
    // the shard holds tiles. The tiles themselves are still copyable.
    for absent in [false, true] {
        for path in [&data, &empty] {
            let mut raw = std::fs::read(path).unwrap();
            let mut header = Header::from_bytes(&raw).unwrap();
            if absent {
                header.json_metadata_length = 0;
            } else {
                let gz = compression::compress(b"{", Compression::Gzip).unwrap();
                header.json_metadata_offset = raw.len() as u64;
                header.json_metadata_length = gz.len() as u64;
                raw.extend_from_slice(&gz);
            }
            raw[..HEADER_BYTES].copy_from_slice(&header.to_bytes());
            std::fs::write(path, raw).unwrap();
        }
        WARNINGS.lock().unwrap().clear();
        let report = merge_shards(
            &[data.clone(), empty.clone()],
            &out,
            &MergeOptions {
                metadata: TilesetMetadata::new().with_name("Parks"),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(tiles_of(&out), tiles_of(&data));
        assert_eq!((report.min_zoom, report.max_zoom), (6, 6));
        // On the report as well as in the log: each may have lost a credit.
        assert_eq!(
            report.inputs_without_metadata,
            vec![data.display().to_string(), empty.display().to_string()]
        );
        let metadata = ArchiveIndex::open(&out).unwrap().metadata_json().unwrap();
        assert_eq!(metadata["name"], "Parks");
        assert!(metadata.get("description").is_none());
        assert!(metadata.get("attribution").is_none());
        // No usable declarations: the writer supplies its default layer
        // over the actual tile range, with no inherited field types.
        assert_eq!(
            metadata["vector_layers"],
            json!([{"id":"layer","minzoom":6,"maxzoom":6,"fields":{}}])
        );
        let warnings = WARNINGS.lock().unwrap();
        for path in [&data, &empty] {
            let messages: Vec<_> = warnings
                .iter()
                .filter(|message| message.starts_with(&format!("{}: metadata", path.display())))
                .collect();
            assert_eq!(messages.len(), 1, "absent={absent}: {warnings:?}");
            assert!(messages[0].contains("absent or not valid JSON"));
            assert!(messages[0].contains("no name, description or attribution is inherited"));
        }
    }
}
