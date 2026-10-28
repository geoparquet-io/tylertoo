use serde_json::json;
use tylertoo_core::overview::convert::ConvertOptions;
use tylertoo_core::overview::export::ExportOptions;

#[test]
fn ordinary_keywords_deserialize_into_convert_options() {
    let options: ConvertOptions = serde_json::from_value(json!({
        "row_group_size": 1234,
        "read_batch_size": 512,
        "cascade": false,
        "point_thinning": 3.5,
        "drop_gamma": 2.0,
        "include_properties": ["id"]
    }))
    .unwrap();
    assert_eq!(options.max_row_group_size, 1234);
    assert_eq!(options.read_batch_size, 512);
    assert!(!options.simplify.cascade);
    assert_eq!(options.assign.point_thinning, 3.5);
    assert_eq!(options.density.gamma, 2.0);
    assert_eq!(options.properties.include, Some(vec!["id".into()]));
    options.validate().unwrap();
}

#[test]
fn ordinary_keywords_deserialize_into_export_options() {
    let options: ExportOptions = serde_json::from_value(json!({
        "extent": 8192,
        "max_compressed_tile_size": 1234,
        "simple_clip_fastpath": false,
        "name": "Example"
    }))
    .unwrap();
    assert_eq!(options.extent, 8192);
    assert_eq!(options.compressed_tile_size_limit, Some(1234));
    assert!(!options.simple_clip_fastpath);
    assert_eq!(options.metadata.name.as_deref(), Some("Example"));
    options.validate().unwrap();
}

#[test]
fn unknown_keywords_are_refused_including_nested_groups() {
    for key in ["extnet", "sort_direciton", "properties", "read_bach_size"] {
        let result = serde_json::from_value::<ConvertOptions>(json!({key: 1}));
        assert!(result.unwrap_err().to_string().contains(key));
    }
    assert!(serde_json::from_value::<ExportOptions>(json!({"extnet": 1})).is_err());
}

#[test]
fn validation_returns_an_immutable_configuration() {
    let options = ConvertOptions::default();
    let validated = options.validate().unwrap();
    assert_eq!(validated.options().read_batch_size, options.read_batch_size);
}

#[test]
fn non_finite_generalization_values_are_configuration_errors() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut options = ConvertOptions::default();
        options.simplify.factor = value;
        assert!(options
            .validate()
            .unwrap_err()
            .to_string()
            .contains("simplify-factor"));
        options.simplify.factor = 1.0;
        options.density.gamma = value;
        assert!(options
            .validate()
            .unwrap_err()
            .to_string()
            .contains("drop-gamma"));
    }
}

#[test]
fn invalid_level_plans_cannot_produce_a_validation_proof() {
    use tylertoo_core::overview::convert::LevelPlan;
    for levels in [
        LevelPlan::ZoomRange {
            min_zoom: 7,
            max_zoom: 2,
        },
        LevelPlan::Gsds(vec![]),
    ] {
        let options = ConvertOptions {
            levels,
            ..Default::default()
        };
        assert!(options.validate().is_err());
    }
}

#[test]
fn export_zoom_bounds_are_checked_without_an_input() {
    let options = ExportOptions {
        min_zoom: Some(255),
        ..Default::default()
    };
    assert!(options
        .validate()
        .unwrap_err()
        .to_string()
        .contains("min-zoom"));
}

#[test]
fn accumulation_specs_round_trip_without_losing_order_or_repeated_columns() {
    use tylertoo_core::overview::convert::parse_accumulate;
    let options = ConvertOptions {
        cluster: true,
        accumulate: vec![
            parse_accumulate("z:sum").unwrap(),
            parse_accumulate("a:max").unwrap(),
            parse_accumulate("z:mean").unwrap(),
        ],
        ..Default::default()
    };
    let encoded = serde_json::to_value(&options).unwrap();
    let decoded: ConvertOptions = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded.accumulate.len(), options.accumulate.len());
    for (actual, expected) in decoded.accumulate.iter().zip(options.accumulate) {
        assert_eq!(actual.column, expected.column);
        assert_eq!(actual.op, expected.op);
    }
}

#[test]
fn type_errors_name_the_keyword_in_flattened_and_optional_groups() {
    use tylertoo_core::overview::config::keywords;
    for (key, value) in [
        ("read_batch_size", json!(true)),
        ("simplify_factor", json!("wrong")),
        ("include_properties", json!("id")),
        ("gsd_base", json!("wrong")),
    ] {
        let error = keywords::convert(
            json!({key: value}).as_object().unwrap().clone(),
            ConvertOptions::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains(key), "{error}");
    }
}

#[test]
fn convert_syntax_preserves_explicit_overrides() {
    use tylertoo_core::overview::config::keywords;
    let options = keywords::convert(
        json!({
            "verbatim": true, "point_thinning": 3.0,
            "collapse_square": true, "class_rank_column": "kind",
            "class_ranks": {"road": 5.0}, "accumulate_attributes": {"n": "sum"},
            "cluster": true, "drop_lines": true
        })
        .as_object()
        .unwrap()
        .clone(),
        ConvertOptions::default(),
    )
    .unwrap();
    options.validate().unwrap();
    assert_eq!(options.assign.point_thinning, 3.0);
    assert_eq!(options.assign.line_thinning, 0.0);
    assert_eq!(options.class_ranking.unwrap().column, "kind");
    assert_eq!(options.accumulate.len(), 1);
    assert!(options.density.kinds.lines);
}

#[test]
fn command_defaults_have_no_internal_engine_keys() {
    use tylertoo_core::overview::config::keywords;
    for command in ["convert", "overview", "export_pmtiles", "pyramid"] {
        let values = keywords::command_defaults(command);
        for key in [
            "levels",
            "ZoomRange",
            "class_ranking",
            "collapse_mode",
            "kinds",
            "explicit_coalesce_lines",
            "shard",
        ] {
            assert!(!values.contains_key(key), "{command}: {key}");
        }
    }
}

#[test]
fn tiles_keywords_match_core_defaults() {
    use tylertoo_core::overview::{config::keywords, tiles::TilesOptions};
    let options = keywords::tiles(Default::default()).unwrap();
    options.validate().unwrap();
    let expected = TilesOptions::default()
        .convert
        .with_generalization(&Default::default(), false)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&options.convert).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(
        options.export.tile_size_limit,
        TilesOptions::default().export.tile_size_limit
    );
}

#[test]
fn automatic_caps_are_resolved_by_each_command() {
    use tylertoo_core::overview::config::keywords;
    let options = keywords::tiles(
        json!({"verbatim": true, "tile_size_limit": "auto", "max_compressed_tile_size": 1200})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap();
    options.validate().unwrap();
    assert_eq!(options.export.tile_size_limit, None);
    assert_eq!(options.export.compressed_tile_size_limit, Some(1200));
    let options = keywords::pyramid(
        json!({"name": "Pyramid", "extent": 8192})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap();
    options.validate().unwrap();
    assert_eq!(options.export.tile_size_limit, None);
    assert_eq!(options.export.metadata.name, None);
    assert_eq!(options.metadata.name.as_deref(), Some("Pyramid"));
}
