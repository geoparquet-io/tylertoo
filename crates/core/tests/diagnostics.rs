use tylertoo_core::diagnostics::{Diagnostic, Diagnostics};

#[test]
fn diagnostics_are_aggregated_and_isolated_between_runs() {
    let first = Diagnostics::default();
    let second = Diagnostics::default();
    rayon::scope(|scope| {
        for _ in 0..20 {
            scope.spawn(|_| first.record(Diagnostic::NonFiniteGeometry { count: 1 }));
        }
    });
    second.record(Diagnostic::NonFiniteGeometry { count: 1 });
    assert_eq!(
        first.snapshot(),
        vec![Diagnostic::NonFiniteGeometry { count: 20 }]
    );
    assert_eq!(
        second.snapshot(),
        vec![Diagnostic::NonFiniteGeometry { count: 1 }]
    );
    assert!(Diagnostics::default().snapshot().is_empty());
}

#[test]
fn diagnostics_serialize_as_typed_dictionaries() {
    let value = serde_json::to_value(Diagnostic::NonFiniteGeometry { count: 2 }).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"kind": "non_finite_geometry", "count": 2})
    );
    assert!(Diagnostic::NonFiniteGeometry { count: 2 }
        .to_string()
        .contains("2"));
}

#[test]
fn out_of_range_display_preserves_excess_and_source_identity() {
    for (axis, value) in [
        ("lon", 540.000_000_1),
        ("lat", -90.000_2),
        ("x", 70_000_000.0),
    ] {
        let event = Diagnostic::OutOfRange {
            context: None,
            count: 1,
            exemplars: vec![tylertoo_core::overview::convert::OutOfRangeExemplar {
                part: Some(2),
                row: 7,
                axis,
                value,
            }],
        };
        let message = event.to_string();
        assert!(message.contains(&value.to_string()), "{message}");
        assert!(message.contains("part 2, row 7"), "{message}");
    }
}

#[test]
fn tile_losses_are_disjoint_and_cancelled_by_any_surviving_member() {
    use tylertoo_core::diagnostics::SourceLossCause;
    let sink = Diagnostics::default();
    sink.source_loss(3, Some(4), SourceLossCause::EncodeDropped)
        .unwrap();
    sink.source_loss(3, Some(4), SourceLossCause::EncodeDropped)
        .unwrap();
    sink.source_loss(9, Some(4), SourceLossCause::ValveDropped)
        .unwrap();
    sink.source_emitted(3, 4).unwrap();
    let losses = sink.source_losses().unwrap();
    assert_eq!(losses.len(), 1);
    assert_eq!(losses[0].id, 9);
    assert_eq!(losses[0].cause, SourceLossCause::ValveDropped);
    sink.source_emitted(9, 4).unwrap();
    sink.source_loss(9, Some(4), SourceLossCause::EncodeDropped)
        .unwrap();
    assert!(sink.source_losses().unwrap().is_empty());
}

#[test]
fn child_contexts_are_preserved_independently_of_arrival_order() {
    let events = vec![
        Diagnostic::RowGroupAutoscaled {
            count: 1,
            cap: 1000,
        },
        Diagnostic::RowGroupAutoscaled {
            count: 1,
            cap: 4000,
        },
        Diagnostic::CrsAssumed {
            count: 1,
            input: "a.parquet".into(),
        },
        Diagnostic::CrsAssumed {
            count: 1,
            input: "b.parquet".into(),
        },
        Diagnostic::EmptyLevels {
            count: 1,
            levels: vec![0],
            gsd_min: 1.0,
            gsd_max: 2.0,
            input_features: 10,
            first_written_gsd: 0.5,
            first_written_zoom: Some(5),
        },
        Diagnostic::EmptyLevels {
            count: 1,
            levels: vec![1],
            gsd_min: 1.0,
            gsd_max: 2.0,
            input_features: 10,
            first_written_gsd: 0.5,
            first_written_zoom: Some(5),
        },
    ];
    let forward = Diagnostics::default();
    forward.extend(events.clone());
    let reverse = Diagnostics::default();
    reverse.extend(events.iter().rev().cloned());
    assert_eq!(forward.snapshot(), reverse.snapshot());
    assert_eq!(forward.snapshot().len(), events.len());
}

#[test]
fn counter_queries_preserve_diagnostic_subtypes() {
    use tylertoo_core::diagnostics::PolygonRepairKind;
    let sink = Diagnostics::default();
    sink.record(Diagnostic::PolygonRepaired {
        count: 2,
        kind: PolygonRepairKind::CollapsedRing,
    });
    sink.record(Diagnostic::PolygonRepaired {
        count: 3,
        kind: PolygonRepairKind::PartsOverlap,
    });
    assert_eq!(
        sink.count(&Diagnostic::PolygonRepaired {
            count: 0,
            kind: PolygonRepairKind::CollapsedRing
        }),
        2
    );
}
