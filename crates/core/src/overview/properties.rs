//! Which property columns a conversion or export carries (#386).
//!
//! tippecanoe has `-x` / `-X` (exclude) and `-y` (include) for this; here the
//! same choice is [`PropertySelection`], resolved once against the input
//! schema into the set of root columns to read. The geometry column is never
//! a property and is always kept. Neither is the geometry's bbox covering
//! struct (#401): the writer regenerates it from the geometry, so a
//! selection never reads the input's and cannot name it. Applied at *scan*
//! time on `overview` / `tiles` (the excluded columns are not decoded, and
//! the intermediate
//! overview file only carries what was asked for) and at export time on
//! `export-pmtiles` (the overview file is unchanged; the tiles carry the
//! selection). The flags combine as tippecanoe's do: an include list is the
//! whole answer and the exclude flags are then ignored (see
//! [`PropertySelection::keeps`]).
//!
//! A column a tuning knob reads (`--sort-key`, `--filter`,
//! `--accumulate-attribute`, `--magnitude-ladder` / `--entry-zoom`,
//! `--class-rank`) must stay included: the knobs evaluate over the projected
//! batches, so excluding it would make them silently inert. That is rejected
//! with the knob's name rather than papered over.

use std::collections::BTreeSet;

use arrow_schema::Schema;
use thiserror::Error;

/// Include/exclude choice over property columns. Names are exact
/// (parquet column names are case-sensitive).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PropertySelection {
    /// When `Some`, only these properties are kept (`-y`), and `exclude` /
    /// `exclude_all` are ignored, as in tippecanoe. An empty list keeps
    /// none — the same as [`Self::exclude_all`].
    pub include: Option<Vec<String>>,
    /// Properties dropped (`-x`). Ignored when `include` is given.
    pub exclude: Vec<String>,
    /// Keep no properties at all (`-X`): geometry-only output. Ignored when
    /// `include` is given.
    pub exclude_all: bool,
}

/// Why a selection could not be applied to a schema.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PropertySelectionError {
    /// An `include` name matches no input column: asking for a column that
    /// is not there is a typo, not a preference.
    #[error("included property {name:?} is not a column of the input (columns: {available})")]
    UnknownInclude { name: String, available: String },

    /// The geometry column was named as a property.
    #[error("{name:?} is the geometry column, not a property; it is always kept")]
    GeometryColumn { name: String },

    /// The geometry's bbox covering struct was named in an include list
    /// (#401). The writer drops the input's covering and regenerates its
    /// own, so including it would change nothing.
    #[error(
        "{name:?} is the geometry's bbox covering, not a property; the writer regenerates \
         it, so it cannot be selected"
    )]
    CoveringColumn { name: String },

    /// A column a tuning knob reads was excluded.
    #[error(
        "property {name:?} is excluded but {knob} reads it; keep it in the selection or \
         drop the knob"
    )]
    RequiredByKnob { name: String, knob: String },
}

impl PropertySelection {
    /// Keep every property (the default).
    pub fn all() -> Self {
        Self::default()
    }

    /// `true` when the selection changes nothing.
    pub fn is_identity(&self) -> bool {
        self.include.is_none() && self.exclude.is_empty() && !self.exclude_all
    }

    /// Whether a property named `name` survives the selection.
    ///
    /// The three fields are read the way tippecanoe reads `-y` / `-x` / `-X`
    /// (`main.cpp` sets `exclude_all` for `-y`; `serial.cpp` then keeps a
    /// key iff it is in `include`, never consulting `exclude`): an include
    /// list is the whole answer, so `-X -y foo` keeps `foo` and
    /// `-y a -y b -x b` keeps both. Without one, `-X` keeps nothing and `-x`
    /// drops what it names.
    pub fn keeps(&self, name: &str) -> bool {
        if let Some(include) = &self.include {
            return include.iter().any(|n| n == name);
        }
        if self.exclude_all {
            return false;
        }
        !self.exclude.iter().any(|n| n == name)
    }

    /// Resolve the selection against `schema` into the sorted root-column
    /// indices to keep: the geometry column plus every kept property.
    ///
    /// `required` lists `(column, knob)` pairs — columns a tuning knob reads
    /// and the knob's display name — which must survive the selection.
    /// Unknown `exclude` names are reported through `warn` (excluding a
    /// column that is not there is harmless); unknown `include` names are an
    /// error.
    ///
    /// # Errors
    ///
    /// Returns a [`PropertySelectionError`] if an `include` name is the
    /// geometry column or not a column of `schema`, or if the selection drops a
    /// column a knob in `required` reads.
    pub fn resolve(
        &self,
        schema: &Schema,
        geom_idx: usize,
        required: &[(String, String)],
        warn: &mut dyn FnMut(String),
    ) -> Result<Vec<usize>, PropertySelectionError> {
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        let geom_name = names[geom_idx];
        let has = |n: &str| names.contains(&n);
        let properties = property_indices(schema, geom_idx);
        let is_covering = |n: &str| {
            names
                .iter()
                .position(|c| *c == n)
                .is_some_and(|i| i != geom_idx && !properties.contains(&i))
        };

        if let Some(include) = &self.include {
            for name in include {
                if name == geom_name {
                    return Err(PropertySelectionError::GeometryColumn { name: name.clone() });
                }
                if is_covering(name) {
                    return Err(PropertySelectionError::CoveringColumn { name: name.clone() });
                }
                if !has(name) {
                    return Err(PropertySelectionError::UnknownInclude {
                        name: name.clone(),
                        available: properties
                            .iter()
                            .map(|&i| format!("{:?}", names[i]))
                            .collect::<Vec<_>>()
                            .join(", "),
                    });
                }
            }
        }
        for name in &self.exclude {
            if name == geom_name {
                return Err(PropertySelectionError::GeometryColumn { name: name.clone() });
            }
            if is_covering(name) {
                warn(format!(
                    "excluded property {name:?} is the geometry's bbox covering, not a \
                     property; the writer regenerates it, so it cannot be selected"
                ));
            } else if !has(name) {
                warn(format!(
                    "excluded property {name:?} is not a column of the input; nothing to drop"
                ));
            }
        }
        for (name, knob) in required {
            if name != geom_name && has(name) && !self.keeps(name) {
                return Err(PropertySelectionError::RequiredByKnob {
                    name: name.clone(),
                    knob: knob.clone(),
                });
            }
        }

        // The covering is not read: the writer builds its own from the
        // geometry, so decoding the input's would only be thrown away.
        let kept: BTreeSet<usize> = std::iter::once(geom_idx)
            .chain(properties.iter().copied().filter(|&i| self.keeps(names[i])))
            .collect();
        Ok(kept.into_iter().collect())
    }
}

/// The indices of `schema`'s property columns: every column except the
/// geometry and its bbox covering struct
/// ([`super::writer::is_covering_column`], #401), in schema order.
pub(crate) fn property_indices(schema: &Schema, geom_idx: usize) -> Vec<usize> {
    let geom = schema.field(geom_idx).name().as_str();
    schema
        .fields()
        .iter()
        .enumerate()
        .filter(|&(i, f)| i != geom_idx && !super::writer::is_covering_column(f, Some(geom), None))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_schema::{DataType, Field};

    fn schema() -> Schema {
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("confidence", DataType::Float32, true),
            Field::new("geometry", DataType::Binary, false),
            Field::new("metrics:area", DataType::Float32, true),
            Field::new("admin:country_code", DataType::Utf8, true),
        ])
    }

    fn resolve(
        sel: &PropertySelection,
        required: &[(&str, &str)],
    ) -> Result<Vec<usize>, PropertySelectionError> {
        let required: Vec<(String, String)> = required
            .iter()
            .map(|(c, k)| (c.to_string(), k.to_string()))
            .collect();
        sel.resolve(&schema(), 2, &required, &mut |_| {})
    }

    #[test]
    fn identity_keeps_every_column() {
        let sel = PropertySelection::all();
        assert!(sel.is_identity());
        assert_eq!(resolve(&sel, &[]).unwrap(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn include_keeps_only_the_named_properties_plus_geometry() {
        let sel = PropertySelection {
            include: Some(vec!["confidence".into(), "metrics:area".into()]),
            ..Default::default()
        };
        assert_eq!(resolve(&sel, &[]).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn exclude_drops_the_named_properties() {
        let sel = PropertySelection {
            exclude: vec!["id".into(), "admin:country_code".into()],
            ..Default::default()
        };
        assert_eq!(resolve(&sel, &[]).unwrap(), vec![1, 2, 3]);
    }

    /// tippecanoe `-y a -y b -x b` keeps both: `serial.cpp` consults only
    /// the include set once `-y` is given, so `-x` is ignored.
    #[test]
    fn include_wins_over_exclude() {
        let sel = PropertySelection {
            include: Some(vec!["confidence".into(), "metrics:area".into()]),
            exclude: vec!["metrics:area".into()],
            ..Default::default()
        };
        assert_eq!(resolve(&sel, &[]).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn exclude_all_leaves_geometry_only() {
        let sel = PropertySelection {
            exclude_all: true,
            ..Default::default()
        };
        assert_eq!(resolve(&sel, &[]).unwrap(), vec![2]);
        // ... and `-x` on top changes nothing.
        let sel = PropertySelection {
            exclude_all: true,
            exclude: vec!["confidence".into()],
            ..Default::default()
        };
        assert_eq!(resolve(&sel, &[]).unwrap(), vec![2]);
    }

    /// tippecanoe `-X -y foo` keeps `foo`: `-y` itself implies `-X`, and the
    /// include set is what is consulted.
    #[test]
    fn include_wins_over_exclude_all() {
        let sel = PropertySelection {
            exclude_all: true,
            include: Some(vec!["confidence".into()]),
            ..Default::default()
        };
        assert_eq!(resolve(&sel, &[]).unwrap(), vec![1, 2]);
    }

    #[test]
    fn empty_include_list_keeps_no_properties() {
        let sel = PropertySelection {
            include: Some(vec![]),
            ..Default::default()
        };
        assert_eq!(resolve(&sel, &[]).unwrap(), vec![2]);
    }

    #[test]
    fn unknown_include_is_an_error_listing_the_columns() {
        let sel = PropertySelection {
            include: Some(vec!["confidnece".into()]),
            ..Default::default()
        };
        let err = resolve(&sel, &[]).unwrap_err();
        match &err {
            PropertySelectionError::UnknownInclude { name, available } => {
                assert_eq!(name, "confidnece");
                assert!(available.contains("\"confidence\""), "{available}");
                assert!(!available.contains("geometry"), "{available}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unknown_exclude_only_warns() {
        let sel = PropertySelection {
            exclude: vec!["nope".into()],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        let kept = sel
            .resolve(&schema(), 2, &[], &mut |w| warnings.push(w))
            .unwrap();
        assert_eq!(kept, vec![0, 1, 2, 3, 4]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("\"nope\""));
    }

    #[test]
    fn geometry_cannot_be_named_as_a_property() {
        let inc = PropertySelection {
            include: Some(vec!["geometry".into()]),
            ..Default::default()
        };
        assert!(matches!(
            resolve(&inc, &[]),
            Err(PropertySelectionError::GeometryColumn { .. })
        ));
        let exc = PropertySelection {
            exclude: vec!["geometry".into()],
            ..Default::default()
        };
        assert!(matches!(
            resolve(&exc, &[]),
            Err(PropertySelectionError::GeometryColumn { .. })
        ));
    }

    #[test]
    fn excluding_a_column_a_knob_reads_is_an_error_naming_the_knob() {
        let sel = PropertySelection {
            exclude: vec!["confidence".into()],
            ..Default::default()
        };
        let err = resolve(&sel, &[("confidence", "--sort-key")]).unwrap_err();
        assert_eq!(
            err,
            PropertySelectionError::RequiredByKnob {
                name: "confidence".into(),
                knob: "--sort-key".into()
            }
        );
        // A knob column that is kept is fine, and one absent from the
        // schema is the knob's own problem to report.
        assert!(resolve(&sel, &[("id", "--sort-key")]).is_ok());
        assert!(resolve(&sel, &[("missing", "--sort-key")]).is_ok());
    }

    /// A GeoParquet input carrying its `bbox` covering struct (#401).
    fn schema_with_covering() -> Schema {
        let float = |n: &str| Field::new(n, DataType::Float64, true);
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("geometry", DataType::Binary, false),
            Field::new(
                "bbox",
                DataType::Struct(
                    vec![float("xmin"), float("ymin"), float("xmax"), float("ymax")].into(),
                ),
                true,
            ),
        ])
    }

    /// #401: the writer regenerates the covering, so naming it in an
    /// include list would change nothing. It is not a property, the way the
    /// geometry is not, and asking for it is an error that says why.
    #[test]
    fn including_the_bbox_covering_is_an_error() {
        let sel = PropertySelection {
            include: Some(vec!["id".into(), "bbox".into()]),
            ..Default::default()
        };
        let err = sel
            .resolve(&schema_with_covering(), 1, &[], &mut |_| {})
            .unwrap_err();
        assert_eq!(
            err,
            PropertySelectionError::CoveringColumn {
                name: "bbox".into()
            }
        );
        assert!(err.to_string().contains("regenerate"), "{err}");
    }

    /// #401: excluding the covering is harmless (it never reaches the tiles
    /// as a property), so it warns rather than failing, and a selection
    /// never reads the input covering: the writer builds its own.
    #[test]
    fn the_bbox_covering_is_outside_the_selectable_set() {
        let sel = PropertySelection {
            exclude: vec!["bbox".into()],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        let kept = sel
            .resolve(&schema_with_covering(), 1, &[], &mut |w| warnings.push(w))
            .unwrap();
        assert_eq!(kept, vec![0, 1]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("\"bbox\""), "{warnings:?}");
        assert!(warnings[0].contains("regenerate"), "{warnings:?}");

        // An unknown-include error does not offer the covering as a choice.
        let sel = PropertySelection {
            include: Some(vec!["nope".into()]),
            ..Default::default()
        };
        match sel.resolve(&schema_with_covering(), 1, &[], &mut |_| {}) {
            Err(PropertySelectionError::UnknownInclude { available, .. }) => {
                assert_eq!(available, "\"id\"");
            }
            other => panic!("unexpected {other:?}"),
        }

        // A struct that merely shares the name but is not a covering stays
        // an ordinary property.
        let schema = Schema::new(vec![
            Field::new("geometry", DataType::Binary, false),
            Field::new(
                "bbox",
                DataType::Struct(vec![Field::new("label", DataType::Utf8, true)].into()),
                true,
            ),
        ]);
        let sel = PropertySelection {
            include: Some(vec!["bbox".into()]),
            ..Default::default()
        };
        assert_eq!(
            sel.resolve(&schema, 0, &[], &mut |_| {}).unwrap(),
            vec![0, 1]
        );
    }
}
