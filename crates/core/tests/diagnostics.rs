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

#[test]
fn unusable_geometry_aggregates_both_losses_and_input_rows() {
    let sink = Diagnostics::default();
    sink.record(Diagnostic::UnusableGeometry {
        count: 2,
        total: 10,
    });
    sink.extend([Diagnostic::UnusableGeometry {
        count: 3,
        total: 20,
    }]);
    let events = sink.snapshot();
    assert_eq!(
        events,
        vec![Diagnostic::UnusableGeometry {
            count: 5,
            total: 30
        }]
    );
    assert_eq!(
        events[0].to_string(),
        "skipping 5 of 30 input rows with a null, empty, or non-finite geometry"
    );
}

#[test]
fn advisory_counts_are_queried_by_complete_message() {
    let sink = Diagnostics::default();
    let first = Diagnostic::Advisory {
        count: 2,
        message: "first warning".into(),
    };
    let second = Diagnostic::Advisory {
        count: 3,
        message: "second warning".into(),
    };
    sink.extend([first.clone(), second.clone()]);
    sink.once(first.clone());
    assert_eq!(sink.count(&first), 2);
    assert_eq!(sink.count(&second), 3);
    assert_eq!(
        sink.count(&Diagnostic::Advisory {
            count: 0,
            message: "absent warning".into()
        }),
        0
    );
    assert_eq!(sink.snapshot().len(), 2);
}

#[test]
fn range_aggregation_preserves_crs_and_combines_only_matching_contexts() {
    use tylertoo_core::diagnostics::RangeContext;
    use tylertoo_core::overview::level::Crs;

    // Exercise every range diagnostic through the public sink. CRS is part
    // of its identity: degrees and metres must never share a denominator.
    for make in [
        |context| Diagnostic::OutOfRange {
            count: 1,
            context,
            exemplars: vec![],
        },
        |context| Diagnostic::Unprojectable { count: 1, context },
        |context| Diagnostic::Wrapped {
            count: 1,
            context,
            exemplars: vec![],
        },
    ] {
        let degrees = RangeContext {
            total: 10,
            crs: Crs::Epsg4326,
            max_abs: 200.0,
        };
        let metres = RangeContext {
            total: 30,
            crs: Crs::Epsg3857,
            max_abs: 30_000_000.0,
        };
        let events = [
            make(Some(degrees)),
            make(Some(metres)),
            make(Some(RangeContext {
                total: 20,
                max_abs: 250.0,
                ..degrees
            })),
            make(None),
        ];
        for reverse in [false, true] {
            let sink = Diagnostics::default();
            let mut ordered = events.clone();
            if reverse {
                ordered.reverse();
            }
            sink.extend(ordered);
            let actual = sink.snapshot();
            let mut combined = make(Some(RangeContext {
                total: 30,
                max_abs: 250.0,
                ..degrees
            }));
            match &mut combined {
                Diagnostic::OutOfRange { count, .. }
                | Diagnostic::Unprojectable { count, .. }
                | Diagnostic::Wrapped { count, .. } => *count = 2,
                _ => unreachable!(),
            }
            assert_eq!(actual.len(), 3, "{actual:?}");
            for expected in [combined, make(Some(metres)), make(None)] {
                assert!(
                    actual.contains(&expected),
                    "missing {expected:?} in {actual:?}"
                );
            }
        }
    }
}
