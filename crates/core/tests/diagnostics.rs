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
    sink.source_loss(3, Some(4), SourceLossCause::EncodeDropped);
    sink.source_loss(3, Some(4), SourceLossCause::EncodeDropped);
    sink.source_loss(9, Some(4), SourceLossCause::ValveDropped);
    sink.source_emitted(3, 4);
    let losses = sink.source_losses();
    assert_eq!(losses.len(), 1);
    assert_eq!(losses[0].id, 9);
    assert_eq!(losses[0].cause, SourceLossCause::ValveDropped);
    sink.source_emitted(9, 4);
    sink.source_loss(9, Some(4), SourceLossCause::EncodeDropped);
    assert!(sink.source_losses().is_empty());
}
