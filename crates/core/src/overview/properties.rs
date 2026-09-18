//! Which property columns a conversion or export carries (#386).
//!
//! tippecanoe has `-x` / `-X` (exclude) and `-y` (include) for this; here the
//! same choice is [`PropertySelection`], resolved once against the input
//! schema into the set of root columns to read. The geometry column is never
//! a property and is always kept. Applied at *scan* time on `overview` /
//! `tiles` (the parquet reader skips the excluded column chunks, and the
//! intermediate overview file only carries what was asked for) and at
//! export time on `export-pmtiles` (the overview file is unchanged; the
//! tiles carry the selection).
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
    /// When `Some`, only these properties are kept (`-y`). An empty list
    /// keeps none — the same as [`Self::exclude_all`].
    pub include: Option<Vec<String>>,
    /// Properties dropped (`-x`). Applied after `include`.
    pub exclude: Vec<String>,
    /// Keep no properties at all (`-X`): geometry-only output.
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
    pub fn keeps(&self, name: &str) -> bool {
        if self.exclude_all {
            return false;
        }
        if let Some(include) = &self.include {
            if !include.iter().any(|n| n == name) {
                return false;
            }
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

        if let Some(include) = &self.include {
            for name in include {
                if name == geom_name {
                    return Err(PropertySelectionError::GeometryColumn { name: name.clone() });
                }
                if !has(name) {
                    return Err(PropertySelectionError::UnknownInclude {
                        name: name.clone(),
                        available: names
                            .iter()
                            .filter(|n| **n != geom_name)
                            .map(|n| format!("{n:?}"))
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
            if !has(name) {
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

        let kept: BTreeSet<usize> = names
            .iter()
            .enumerate()
            .filter(|&(i, n)| i == geom_idx || self.keeps(n))
            .map(|(i, _)| i)
            .collect();
        Ok(kept.into_iter().collect())
    }
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

    #[test]
    fn exclude_applies_after_include() {
        let sel = PropertySelection {
            include: Some(vec!["confidence".into(), "metrics:area".into()]),
            exclude: vec!["metrics:area".into()],
            ..Default::default()
        };
        assert_eq!(resolve(&sel, &[]).unwrap(), vec![1, 2]);
    }

    #[test]
    fn exclude_all_leaves_geometry_only() {
        let sel = PropertySelection {
            exclude_all: true,
            include: Some(vec!["confidence".into()]),
            ..Default::default()
        };
        assert_eq!(resolve(&sel, &[]).unwrap(), vec![2]);
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
}
