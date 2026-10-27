use super::*;
use tylertoo_io::test_support::FileMetaData;
use tylertoo_io::KeyValue;

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
