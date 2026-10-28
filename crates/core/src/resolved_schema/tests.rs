use super::*;
use tylertoo_io::test_support::FileMetaData;
use tylertoo_io::KeyValue;

fn covering_metadata(values: [f64; 4], statistics: bool, crs: &str) -> ParquetMetaData {
    use arrow_array::{ArrayRef, BinaryArray, Float64Array, RecordBatch, StructArray};
    use arrow_schema::{Field, Fields};
    use tylertoo_io::test_support::{ArrowWriter, EnabledStatistics, WriterProperties};
    let children: Fields = ["xmin", "ymin", "xmax", "ymax"]
        .map(|n| Field::new(n, DataType::Float64, false))
        .to_vec()
        .into();
    let arrays: Vec<ArrayRef> = values
        .into_iter()
        .map(|v| Arc::new(Float64Array::from(vec![v])) as ArrayRef)
        .collect();
    let bbox = StructArray::new(children.clone(), arrays, None);
    let schema = Arc::new(Schema::new_with_metadata(
        vec![
            Field::new("geometry", DataType::Binary, false),
            Field::new("bbox", DataType::Struct(children), false),
        ],
        [(
            "geo".into(),
            serde_json::json!({
                "version": "1.1.0", "primary_column": "geometry", "columns": {
                    "geometry": {"encoding": "WKB", "crs": crs, "covering": {"bbox": {
                        "xmin": ["bbox", "xmin"], "ymin": ["bbox", "ymin"],
                        "xmax": ["bbox", "xmax"], "ymax": ["bbox", "ymax"]
                    }}}
                }
            })
            .to_string(),
        )]
        .into(),
    ));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from(vec![b"unused".as_slice()])),
            Arc::new(bbox),
        ],
    )
    .unwrap();
    let props = WriterProperties::builder()
        .set_statistics_enabled(if statistics {
            EnabledStatistics::Chunk
        } else {
            EnabledStatistics::None
        })
        .build();
    let mut bytes = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut bytes, schema.clone(), Some(props)).unwrap();
    writer.write(&batch).unwrap();
    writer.append_key_value_metadata(KeyValue::new(
        "geo".into(),
        schema.metadata()["geo"].clone(),
    ));
    writer.close().unwrap();
    (*tylertoo_io::read_parquet_metadata(&bytes::Bytes::from(bytes)).unwrap()).clone()
}

#[test]
fn covering_fast_path_requires_footer_statistics_in_crs_range() {
    for (values, statistics, crs, accepted) in [
        ([1.0, 2.0, 3.0, 4.0], true, "EPSG:4326", true),
        ([1000.0, 2000.0, 3000.0, 4000.0], true, "EPSG:4326", false),
        ([1000.0, 2000.0, 3000.0, 4000.0], true, "EPSG:3857", true),
        ([1.0, 2.0, 3.0, 4.0], false, "EPSG:4326", false),
        ([1.0, 2.0, f64::INFINITY, 4.0], true, "EPSG:4326", false),
        // Dateline envelopes and the supported one-world wrap remain usable.
        ([179.0, -1.0, -179.0, 1.0], true, "EPSG:4326", true),
        ([359.0, -1.0, 361.0, 1.0], true, "EPSG:4326", true),
    ] {
        let md = covering_metadata(values, statistics, crs);
        let resolved = ResolvedSchema::from_metadata(&md).unwrap();
        assert_eq!(
            ResolvedSchema::auto_zoom_covering([&resolved], &Diagnostics::silent()).is_some(),
            accepted,
            "{values:?}, {statistics}, {crs}"
        );
        // A serialized descriptor cannot bypass validation of the actual footer.
        let persisted = with_descriptor(&md, resolved.footer_json().unwrap());
        assert_eq!(
            ResolvedSchema::auto_zoom_covering(
                [&ResolvedSchema::from_metadata(&persisted).unwrap()],
                &Diagnostics::silent()
            )
            .is_some(),
            accepted
        );
    }
}

#[test]
fn rejected_covering_records_a_typed_diagnostic_per_run() {
    let md = covering_metadata([1000.0, 2000.0, 3000.0, 4000.0], true, "EPSG:4326");
    let resolved = ResolvedSchema::from_metadata(&md).unwrap();
    for _ in 0..2 {
        let sink = Diagnostics::silent();
        ResolvedSchema::auto_zoom_covering([&resolved], &sink);
        ResolvedSchema::auto_zoom_covering([&resolved], &sink);
        let events: Vec<_> = sink
            .snapshot()
            .into_iter()
            .filter(|e| serde_json::to_value(e).unwrap()["kind"] == "covering_rejected")
            .collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].count(), 1);
        assert!(events[0].to_string().contains("coordinate range"));
    }
}

#[test]
fn every_row_group_is_checked_including_inverted_endpoints() {
    let good = covering_metadata([1.0, 2.0, 3.0, 4.0], true, "EPSG:4326");
    // Only checking min(xmin)/max(xmax) would miss this inverted outlier.
    let bad = covering_metadata([1000.0, 2.0, 3.0, 4.0], true, "EPSG:4326");
    let md = ParquetMetaData::new(
        good.file_metadata().clone(),
        vec![good.row_group(0).clone(), bad.row_group(0).clone()],
    );
    let resolved = ResolvedSchema::from_metadata(&md).unwrap();
    let sink = Diagnostics::silent();
    assert!(ResolvedSchema::auto_zoom_covering([&resolved], &sink).is_none());
    assert!(sink.snapshot().iter().any(|e| matches!(
        e,
        Diagnostic::CoveringRejected {
            reason: crate::diagnostics::CoveringWarning::OutOfCrsRange(1),
            ..
        }
    )));
}

#[test]
fn covering_decline_reasons_are_typed_and_serializable() {
    use crate::diagnostics::CoveringWarning;
    let md = covering_metadata([1.0, 2.0, 3.0, 4.0], true, "EPSG:4326");
    let base = ResolvedSchema::from_metadata(&md).unwrap();
    for (replacement, expected) in [
        (serde_json::Value::Null, CoveringWarning::NoCovering),
        (
            serde_json::json!({"bbox": {
                "xmin": ["xmin"], "ymin": ["ymin"],
                "xmax": ["xmax"], "ymax": ["ymax"]
            }}),
            CoveringWarning::UnsupportedPath,
        ),
        (
            serde_json::json!({"bbox": {
                "xmin": ["bbox", "xmin"], "ymin": ["bbox", "xmin"],
                "xmax": ["bbox", "xmax"], "ymax": ["bbox", "ymax"]
            }}),
            CoveringWarning::DuplicateColumns,
        ),
        (
            serde_json::json!({"bbox": {
                "xmin": ["bbox", "absent"], "ymin": ["bbox", "ymin"],
                "xmax": ["bbox", "xmax"], "ymax": ["bbox", "ymax"]
            }}),
            CoveringWarning::MissingColumn,
        ),
    ] {
        let mut metadata = base.schema().metadata().clone();
        let mut geo: Value = serde_json::from_str(&metadata["geo"]).unwrap();
        if replacement.is_null() {
            geo["columns"]["geometry"]
                .as_object_mut()
                .unwrap()
                .remove("covering");
        } else {
            geo["columns"]["geometry"]["covering"] = replacement;
        }
        metadata.insert("geo".into(), geo.to_string());
        let schema = Schema::new_with_metadata(base.schema().fields().clone(), metadata);
        let resolved = ResolvedSchema::resolve(&schema).unwrap();
        let sink = Diagnostics::silent();
        assert!(ResolvedSchema::auto_zoom_covering([&resolved], &sink).is_none());
        let event = Diagnostic::CoveringRejected {
            count: 1,
            reason: expected,
        };
        assert_eq!(sink.snapshot(), vec![event.clone()]);
        assert_eq!(
            serde_json::from_value::<Diagnostic>(serde_json::to_value(&event).unwrap()).unwrap(),
            event
        );
    }
}

fn native_metadata(crs: Option<&str>) -> ParquetMetaData {
    use tylertoo_io::test_support::{SchemaDescriptor, Type};
    use tylertoo_io::PhysicalType;
    let geometry = Type::primitive_type_builder("SHAPE", PhysicalType::BYTE_ARRAY)
        .with_logical_type(Some(LogicalType::geometry(crs.map(str::to_owned))))
        .build()
        .unwrap();
    let schema = Type::group_type_builder("schema")
        .with_fields(vec![Arc::new(geometry)])
        .build()
        .unwrap();
    let schema = Arc::new(SchemaDescriptor::new(Arc::new(schema)));
    ParquetMetaData::new(FileMetaData::new(2, 0, None, None, schema, None), vec![])
}

fn with_descriptor(md: &ParquetMetaData, json: String) -> ParquetMetaData {
    let file = md.file_metadata();
    let mut kv = file.key_value_metadata().cloned().unwrap_or_default();
    kv.push(KeyValue::new(RESOLVED_SCHEMA_KEY.to_owned(), json));
    ParquetMetaData::new(
        FileMetaData::new(
            file.version(),
            file.num_rows(),
            file.created_by().map(str::to_owned),
            Some(kv),
            file.schema_descr_ptr(),
            file.column_orders().cloned(),
        ),
        md.row_groups().to_vec(),
    )
}

#[test]
fn native_logical_type_resolves_shape_and_crs_without_geo_key() {
    for (id, expected) in [
        (None, Some(Crs::Epsg4326)),
        (Some("srid:0"), Some(Crs::Epsg4326)),
        (Some("EPSG:3857"), Some(Crs::Epsg3857)),
        (Some("EPSG:32633"), None),
    ] {
        let descriptor = ResolvedSchema::from_metadata(&native_metadata(id)).unwrap();
        assert_eq!(descriptor.geometry_name(), Some("SHAPE"));
        assert_eq!(descriptor.geometry_index(), Some(0));
        assert_eq!(descriptor.version(), Some("2.0.0"));
        assert_eq!(descriptor.crs(), expected);
        assert!(!descriptor.crs_info().assumed_wgs84);
    }
}

#[test]
fn persisted_descriptor_cannot_change_the_declared_crs() {
    let md = native_metadata(Some("EPSG:3857"));
    let descriptor = ResolvedSchema::from_metadata(&md).unwrap();
    let mut json: Value = serde_json::from_str(&descriptor.footer_json().unwrap()).unwrap();
    json["crs"]["identifier"] = Value::String("EPSG:4326".into());
    json["crs"]["is_wgs84"] = Value::Bool(true);
    let tampered = with_descriptor(&md, json.to_string());
    assert!(ResolvedSchema::from_metadata(&tampered).is_err());
}

#[test]
fn persisted_descriptor_round_trips_and_checks_indices() {
    let md = native_metadata(None);
    let descriptor = ResolvedSchema::from_metadata(&md).unwrap();
    let json = descriptor.footer_json().unwrap();
    assert_eq!(
        ResolvedSchema::from_metadata(&with_descriptor(&md, json.clone()))
            .unwrap()
            .geometry_name(),
        Some("SHAPE")
    );
    let mut json: Value = serde_json::from_str(&json).unwrap();
    json["geometry_index"] = Value::from(99);
    assert!(ResolvedSchema::from_metadata(&with_descriptor(&md, json.to_string())).is_err());
    json["geometry_name"] = Value::Null;
    assert!(ResolvedSchema::from_metadata(&with_descriptor(&md, json.to_string())).is_err());
}

#[test]
fn persisted_descriptor_round_trips_a_resolved_primary_fallback() {
    let md = native_metadata(None);
    let file = md.file_metadata();
    let geo = serde_json::json!({
        "version": "1.1.0", "primary_column": "missing",
        "columns": {"SHAPE": {"encoding": "WKB"}}
    });
    let md = ParquetMetaData::new(
        FileMetaData::new(
            file.version(),
            file.num_rows(),
            None,
            Some(vec![KeyValue::new("geo".to_owned(), geo.to_string())]),
            file.schema_descr_ptr(),
            file.column_orders().cloned(),
        ),
        vec![],
    );
    let resolved = ResolvedSchema::from_metadata(&md).unwrap();
    assert_eq!(resolved.primary_column(), Some("missing"));
    assert_eq!(resolved.geometry_name(), Some("SHAPE"));
    let json = resolved.footer_json().unwrap();
    let restored = ResolvedSchema::from_metadata(&with_descriptor(&md, json.clone())).unwrap();
    assert_eq!(restored.footer_json().unwrap(), json);
}

#[test]
fn persisted_descriptor_rejects_each_altered_fact() {
    let md = native_metadata(None);
    let resolved = ResolvedSchema::from_metadata(&md).unwrap();
    let json: Value = serde_json::from_str(&resolved.footer_json().unwrap()).unwrap();
    for (field, value) in [
        ("descriptor_version", serde_json::json!(2)),
        ("fields", serde_json::json!([])),
        ("geometry_index", serde_json::json!(99)),
        ("geometry_name", serde_json::json!("other")),
        ("primary_column", serde_json::json!("other")),
        ("geometry_columns", serde_json::json!([])),
        ("properties", serde_json::json!([99])),
        ("version", serde_json::json!("9.0.0")),
        ("renames", serde_json::json!({"other": "SHAPE"})),
        ("geo_json", serde_json::json!("null")),
        ("source_is_subset", serde_json::json!(true)),
    ] {
        let mut altered = json.clone();
        altered[field] = value;
        let restored = ResolvedSchema::from_metadata(&with_descriptor(&md, altered.to_string()));
        assert!(restored.is_err(), "accepted altered {field}");
    }
}

#[test]
fn persisted_descriptor_cannot_hide_properties() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/invariants/shape-column.parquet");
    let md = tylertoo_io::read_parquet_metadata(&std::fs::File::open(path).unwrap()).unwrap();
    let descriptor = ResolvedSchema::from_metadata(&md).unwrap();
    assert!(!descriptor.properties().is_empty());
    let mut json: Value = serde_json::from_str(&descriptor.footer_json().unwrap()).unwrap();
    json["properties"] = serde_json::json!([]);
    assert!(ResolvedSchema::from_metadata(&with_descriptor(&md, json.to_string())).is_err());
}

#[test]
fn persisted_descriptor_cannot_claim_a_spatial_subset() {
    let md = native_metadata(None);
    let descriptor = ResolvedSchema::from_metadata(&md).unwrap();
    let mut json: Value = serde_json::from_str(&descriptor.footer_json().unwrap()).unwrap();
    json["source_is_subset"] = Value::Bool(true);
    assert!(ResolvedSchema::from_metadata(&with_descriptor(&md, json.to_string())).is_err());
}

#[test]
fn cached_schema_warnings_are_replayed_once_for_each_run() {
    use crate::diagnostics::{CrsWarning, Diagnostic, Diagnostics};
    let schema = Schema::new_with_metadata(
        vec![arrow_schema::Field::new(
            "shape",
            arrow_schema::DataType::Binary,
            true,
        )],
        [(
            "geo".to_owned(),
            serde_json::json!({
                "version": "9.0.0", "primary_column": "shape",
                "columns": {"shape": {"encoding": "WKB", "crs": null}}
            })
            .to_string(),
        )]
        .into(),
    );
    let resolved = ResolvedSchema::resolve(&schema).unwrap();
    for _ in 0..2 {
        let sink = Diagnostics::silent();
        resolved.record_diagnostics(&sink);
        resolved.record_diagnostics(&sink);
        let mut expected = vec![
            Diagnostic::CrsMetadata {
                count: 1,
                reason: CrsWarning::UnknownVersion("9.0.0".into()),
            },
            Diagnostic::CrsMetadata {
                count: 1,
                reason: CrsWarning::NullCrsDegrees,
            },
        ];
        expected.sort_by_cached_key(|event| serde_json::to_string(event).unwrap());
        assert_eq!(sink.snapshot(), expected);
    }
}

#[test]
fn native_declared_crs_has_no_assumption_diagnostics() {
    let metadata = native_metadata(Some("EPSG:3857"));
    let resolved = ResolvedSchema::from_metadata(&metadata).unwrap();
    let sink = crate::diagnostics::Diagnostics::silent();
    resolved.record_diagnostics(&sink);
    assert!(sink.snapshot().is_empty());
}

#[test]
fn transformed_schema_preserves_crs_and_only_records_this_conversions_renames() {
    let mut descriptor =
        ResolvedSchema::from_metadata(&native_metadata(Some("EPSG:3857"))).unwrap();
    descriptor
        .facts
        .renames
        .insert("SHAPE".into(), "old_shape".into());
    let schema = Arc::new(Schema::new(vec![descriptor
        .schema
        .field(0)
        .clone()
        .with_name("geom")]));
    let transformed = descriptor.transformed(schema, &[("SHAPE".into(), "geom".into())]);
    assert_eq!(transformed.crs(), Some(Crs::Epsg3857));
    assert_eq!(transformed.geometry_name(), Some("geom"));
    assert_eq!(transformed.geometry_index(), Some(0));
    assert_eq!(
        transformed.renames(),
        &BTreeMap::from([("geom".into(), "SHAPE".into())])
    );
}
