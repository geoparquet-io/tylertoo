//! Archive-level publication metadata: the top-level `name`, `description`
//! and `attribution` strings of a PMTiles archive's JSON metadata.
//!
//! These describe the published tileset as a whole, separately from the MVT
//! layer name (`vector_layers[].id`) and from feature properties. Readers such
//! as the PMTiles JavaScript adapter copy them into TileJSON, where MapLibre
//! shows `attribution` in its attribution control.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::compression::MAX_INTERNAL_BYTES;

/// Publication metadata written at the top level of an archive's JSON metadata.
///
/// `None` leaves a key out; `Some("")` writes an empty string. Values are
/// stored exactly as given, HTML included.
///
/// # Examples
///
/// ```
/// use tylertoo_core::TilesetMetadata;
///
/// let meta = TilesetMetadata::new()
///     .with_name("Park Tiles")
///     .with_attribution(r#"<a href="https://www.nps.gov/">National Park Service</a>"#);
/// assert_eq!(meta.name.as_deref(), Some("Park Tiles"));
/// assert_eq!(meta.description, None);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TilesetMetadata {
    /// Human-readable tileset name.
    pub name: Option<String>,
    /// Tileset description.
    pub description: Option<String>,
    /// Attribution text or HTML.
    pub attribution: Option<String>,
}

impl TilesetMetadata {
    /// Create metadata with no fields set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the tileset name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the tileset description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the attribution text or HTML.
    #[must_use]
    pub fn with_attribution(mut self, attribution: impl Into<String>) -> Self {
        self.attribution = Some(attribution.into());
        self
    }

    /// The set fields, keyed by their JSON name, in the fixed output order.
    fn fields(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("name", self.name.as_deref()),
            ("description", self.description.as_deref()),
            ("attribution", self.attribution.as_deref()),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|v| (key, v)))
    }

    /// Append `,"key":<json string>` for each set field to an open JSON
    /// object. Values go through `serde_json`, never between hand-written
    /// quotes, so HTML, quotes, backslashes and newlines stay valid JSON.
    pub(crate) fn append_json_fields(&self, out: &mut String) {
        for (key, value) in self.fields() {
            out.push_str(",\"");
            out.push_str(key);
            out.push_str("\":");
            // Serializing a `str` into a `String` cannot fail.
            out.push_str(&serde_json::to_string(value).expect("JSON string serialization"));
        }
    }

    /// Bytes [`Self::append_json_fields`] appends, counted without building
    /// the escaped strings.
    pub(crate) fn serialized_len(&self) -> u64 {
        self.fields()
            // `,"key":` plus the escaped, quoted value
            .map(|(key, value)| key.len() as u64 + 4 + json_string_len(value))
            .sum()
    }

    /// Reject values that cannot fit an archive's metadata on their own.
    ///
    /// The structural metadata adds bytes and the final encoded section is
    /// checked again when the archive is assembled; this is the cheap lower
    /// bound a caller can apply before any conversion work starts.
    pub(crate) fn check_size(&self) -> Result<(), String> {
        // A standalone object of just these fields: the leading comma
        // becomes `{`, plus the closing `}`.
        let len = self.serialized_len() + 1;
        if len <= MAX_INTERNAL_BYTES {
            return Ok(());
        }
        let keys: Vec<&str> = self.fields().map(|(key, _)| key).collect();
        Err(format!(
            "publication metadata ({}) serializes to {len} bytes, over the \
             {MAX_INTERNAL_BYTES}-byte limit on an archive's metadata",
            keys.join(", ")
        ))
    }
}

/// One of the three publication fields.
#[derive(Debug, Clone, Copy)]
enum Field {
    Name,
    Description,
    Attribution,
}

impl Field {
    const ALL: [Field; 3] = [Field::Name, Field::Description, Field::Attribution];

    /// The JSON key, which is also the CLI flag's name.
    fn key(self) -> &'static str {
        match self {
            Field::Name => "name",
            Field::Description => "description",
            Field::Attribution => "attribution",
        }
    }

    fn of(self, meta: &TilesetMetadata) -> Option<&str> {
        match self {
            Field::Name => meta.name.as_deref(),
            Field::Description => meta.description.as_deref(),
            Field::Attribution => meta.attribution.as_deref(),
        }
    }
}

/// Joins inherited attribution credits.
const CREDIT_SEPARATOR: &str = " | ";

/// Resolves the publication metadata of an archive composed from others
/// (`merge`, `pyramid`).
///
/// Each field resolves on its own. An explicit override (including `""`)
/// wins and that field's inherited values are not even read. Otherwise:
/// a name or description the inputs agree on is kept, and two different
/// ones are an error asking for an override; attribution credits are all
/// kept, deduplicated exactly, sorted by Rust string order and joined with
/// `" | "` (an inherited `""` counts only when no input has a credit).
/// Absent and `null` fields are no value; any other non-string is an error.
///
/// The result does not depend on the order inputs are added in.
///
/// DIVERGENCE FROM TIPPECANOE: tile-join keeps the last input's attribution
/// and description, joins differing names with " + ", and ignores an empty
/// override (tile-join.cpp, near L1220). We keep every input's credit so a
/// composed map still credits all of its sources, refuse to guess a combined
/// name or description, and let `""` clear a field explicitly.
#[derive(Debug)]
pub(crate) struct MetadataCollector {
    overrides: TilesetMetadata,
    /// The inherited name and the first input that carried it.
    name: Option<(String, String)>,
    /// The inherited description and the first input that carried it.
    description: Option<(String, String)>,
    /// Distinct non-empty attribution credits.
    credits: BTreeSet<String>,
    /// Escaped bytes of every credit in `credits`, without quotes or
    /// separators: what the joined value costs in the metadata JSON.
    credit_bytes: u64,
    /// Whether some input carried `"attribution": ""`.
    empty_credit: bool,
}

impl MetadataCollector {
    /// A collector whose result applies `overrides` field by field.
    pub(crate) fn new(overrides: &TilesetMetadata) -> Self {
        Self {
            overrides: overrides.clone(),
            name: None,
            description: None,
            credits: BTreeSet::new(),
            credit_bytes: 0,
            empty_credit: false,
        }
    }

    fn overridden(&self, field: Field) -> bool {
        field.of(&self.overrides).is_some()
    }

    /// Add one input archive's metadata JSON, by its whole root.
    ///
    /// A root that is not an object carries no publication fields. Unknown
    /// keys are ignored.
    ///
    /// # Errors
    ///
    /// Returns a message naming `label` if a field the output inherits is
    /// neither a string nor `null`, or conflicts with an earlier input.
    pub(crate) fn add_json(&mut self, label: &str, root: &Value) -> Result<(), String> {
        let Value::Object(map) = root else {
            return Ok(());
        };
        for field in Field::ALL {
            if self.overridden(field) {
                continue;
            }
            match map.get(field.key()) {
                None | Some(Value::Null) => {}
                Some(Value::String(value)) => self.add_value(label, field, value)?,
                // Named, never printed: the value could be anything, of any size.
                Some(_) => {
                    return Err(format!(
                        "{label}: {key} must be a string or null; set --{key} to override it",
                        key = field.key()
                    ))
                }
            }
        }
        Ok(())
    }

    /// Add values an input is known to carry, such as the export metadata a
    /// generated pyramid band will be written with.
    ///
    /// # Errors
    ///
    /// As [`Self::add_json`].
    pub(crate) fn add_metadata(
        &mut self,
        label: &str,
        meta: &TilesetMetadata,
    ) -> Result<(), String> {
        for field in Field::ALL {
            if let (false, Some(value)) = (self.overridden(field), field.of(meta)) {
                self.add_value(label, field, value)?;
            }
        }
        Ok(())
    }

    fn add_value(&mut self, label: &str, field: Field, value: &str) -> Result<(), String> {
        let slot = match field {
            Field::Name => &mut self.name,
            Field::Description => &mut self.description,
            Field::Attribution => return self.add_credit(value),
        };
        match slot {
            None => *slot = Some((value.to_string(), label.to_string())),
            Some((seen, _)) if seen == value => {}
            // Labels only, not the values: either could be long HTML.
            Some((_, first)) => {
                return Err(format!(
                    "conflicting PMTiles {key}s in {first} and {label}; set --{key} for the output",
                    key = field.key()
                ))
            }
        }
        Ok(())
    }

    fn add_credit(&mut self, credit: &str) -> Result<(), String> {
        if credit.is_empty() {
            self.empty_credit = true;
            return Ok(());
        }
        if self.credits.contains(credit) {
            return Ok(());
        }
        self.credit_bytes += json_string_len(credit) - 2;
        self.credits.insert(credit.to_string());
        // Checked as credits arrive, so a merge of many inputs fails at the
        // first credit that cannot fit rather than after joining them all.
        // `{"attribution":"` + credits + separators + `"}`.
        let separators = (self.credits.len() as u64 - 1) * CREDIT_SEPARATOR.len() as u64;
        let len = 16 + self.credit_bytes + separators + 2;
        if len > MAX_INTERNAL_BYTES {
            return Err(format!(
                "the inputs' attribution credits join to over {MAX_INTERNAL_BYTES} bytes, the \
                 limit on an archive's metadata; set --attribution for the output"
            ));
        }
        Ok(())
    }

    /// The output's publication metadata.
    ///
    /// # Errors
    ///
    /// Returns a message if the resolved values are too large for an
    /// archive's metadata.
    pub(crate) fn resolve(&self) -> Result<TilesetMetadata, String> {
        let inherited = |slot: &Option<(String, String)>| slot.as_ref().map(|(v, _)| v.clone());
        let attribution = if !self.credits.is_empty() {
            let credits: Vec<&str> = self.credits.iter().map(String::as_str).collect();
            Some(credits.join(CREDIT_SEPARATOR))
        } else if self.empty_credit {
            Some(String::new())
        } else {
            None
        };
        let resolved = TilesetMetadata {
            name: self
                .overrides
                .name
                .clone()
                .or_else(|| inherited(&self.name)),
            description: self
                .overrides
                .description
                .clone()
                .or_else(|| inherited(&self.description)),
            attribution: self.overrides.attribution.clone().or(attribution),
        };
        resolved.check_size()?;
        Ok(resolved)
    }
}

/// Counts bytes written to it and discards them.
struct ByteCounter(u64);

impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Length of `value` once serialized as a JSON string, quotes included.
fn json_string_len(value: &str) -> u64 {
    let mut counter = ByteCounter(0);
    // Writing a `str` to an infallible sink cannot fail.
    serde_json::to_writer(&mut counter, value).expect("JSON string serialization");
    counter.0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// HTML, Unicode, quotes, a backslash and a newline in one value.
    pub(crate) const HOSTILE: &str =
        "© NPS — <a href=\"https://www.nps.gov/\">Parks & Trails</a>\\data\nSecond line";

    fn object_of(meta: &TilesetMetadata) -> serde_json::Map<String, serde_json::Value> {
        let mut json = String::from(r#"{"generator":"tylertoo""#);
        meta.append_json_fields(&mut json);
        json.push('}');
        match serde_json::from_str(&json).expect("appended fields must be valid JSON") {
            serde_json::Value::Object(map) => map,
            other => panic!("expected an object, got {other}"),
        }
    }

    #[test]
    fn builders_set_each_field_and_default_is_empty() {
        assert_eq!(TilesetMetadata::new(), TilesetMetadata::default());
        let meta = TilesetMetadata::new()
            .with_name("N")
            .with_description("")
            .with_attribution("A");
        assert_eq!(meta.name.as_deref(), Some("N"));
        assert_eq!(meta.description.as_deref(), Some(""));
        assert_eq!(meta.attribution.as_deref(), Some("A"));
        assert_eq!(meta.clone(), meta);
    }

    #[test]
    fn default_appends_nothing() {
        let mut json = String::from("{\"a\":1");
        TilesetMetadata::new().append_json_fields(&mut json);
        assert_eq!(json, "{\"a\":1");
        assert_eq!(TilesetMetadata::new().serialized_len(), 0);
    }

    #[test]
    fn fields_append_in_fixed_order_with_exact_values() {
        let meta = TilesetMetadata::new()
            .with_attribution(HOSTILE)
            .with_name("Park Tiles 4")
            .with_description("National park boundaries");
        let mut json = String::new();
        meta.append_json_fields(&mut json);
        let name_at = json.find("\"name\"").unwrap();
        let desc_at = json.find("\"description\"").unwrap();
        let attr_at = json.find("\"attribution\"").unwrap();
        assert!(name_at < desc_at && desc_at < attr_at, "order: {json}");

        let map = object_of(&meta);
        assert_eq!(map["name"], "Park Tiles 4");
        assert_eq!(map["description"], "National park boundaries");
        assert_eq!(map["attribution"], HOSTILE);
    }

    #[test]
    fn each_field_appears_alone_and_empty_strings_are_kept() {
        for (meta, key) in [
            (TilesetMetadata::new().with_name(""), "name"),
            (TilesetMetadata::new().with_description(""), "description"),
            (TilesetMetadata::new().with_attribution(""), "attribution"),
        ] {
            let map = object_of(&meta);
            assert_eq!(map.len(), 2, "only {key} added: {map:?}");
            assert_eq!(map[key], "", "{key} keeps its empty string");
        }
    }

    #[test]
    fn whitespace_and_unicode_are_not_normalized() {
        let value = "  \u{e9}\u{301} \t\u{10000}  ";
        let map = object_of(&TilesetMetadata::new().with_name(value));
        assert_eq!(map["name"], value);
    }

    #[test]
    fn serialized_len_matches_appended_bytes() {
        let meta = TilesetMetadata::new()
            .with_name("")
            .with_description("\"\\\n\u{1}")
            .with_attribution(HOSTILE);
        let mut json = String::new();
        meta.append_json_fields(&mut json);
        assert_eq!(meta.serialized_len(), json.len() as u64);
    }

    #[test]
    fn check_size_measures_escaped_bytes() {
        let cap = MAX_INTERNAL_BYTES as usize;
        // `{"name":"..."}` is 11 bytes of framing around the value.
        let fits = TilesetMetadata::new().with_name("x".repeat(cap - 11));
        assert!(fits.check_size().is_ok());
        let over = TilesetMetadata::new().with_name("x".repeat(cap - 10));
        assert!(over.check_size().is_err());
        // Fits before escaping, not after: each quote becomes `\"`.
        let escaped = TilesetMetadata::new().with_attribution("\"".repeat(cap / 2));
        let err = escaped.check_size().unwrap_err();
        assert!(err.contains("attribution"), "names the fields: {err}");
    }

    // -------------------------------------------------------------------------
    // MetadataCollector
    // -------------------------------------------------------------------------

    fn collect(overrides: &TilesetMetadata, inputs: &[Value]) -> Result<TilesetMetadata, String> {
        let mut c = MetadataCollector::new(overrides);
        for (i, root) in inputs.iter().enumerate() {
            c.add_json(&format!("input{i}"), root)?;
        }
        c.resolve()
    }

    fn inherit(inputs: &[Value]) -> Result<TilesetMetadata, String> {
        collect(&TilesetMetadata::new(), inputs)
    }

    fn credits(values: &[&str]) -> Vec<Value> {
        values.iter().map(|v| json!({ "attribution": v })).collect()
    }

    use serde_json::json;

    #[test]
    fn absent_and_null_inherit_nothing() {
        assert_eq!(inherit(&[json!({}), json!({})]), Ok(TilesetMetadata::new()));
        let nulls = json!({"name": null, "description": null, "attribution": null});
        assert_eq!(inherit(&[json!({}), nulls]), Ok(TilesetMetadata::new()));
    }

    #[test]
    fn one_or_repeated_values_are_kept_exactly() {
        let a = json!({"name": "A", "description": "D", "attribution": HOSTILE});
        let want = TilesetMetadata::new()
            .with_name("A")
            .with_description("D")
            .with_attribution(HOSTILE);
        assert_eq!(inherit(&[a.clone(), json!({})]), Ok(want.clone()));
        assert_eq!(inherit(&[a.clone(), a]), Ok(want));
    }

    #[test]
    fn overrides_win_per_field_including_empty() {
        let inputs = [
            json!({"name": "A", "description": "A", "attribution": "A"}),
            json!({"name": "B", "description": "B", "attribution": "B"}),
        ];
        for value in ["C", ""] {
            let all = TilesetMetadata::new()
                .with_name(value)
                .with_description(value)
                .with_attribution(value);
            assert_eq!(
                collect(&all, &inputs),
                Ok(all.clone()),
                "override {value:?}"
            );
        }
    }

    #[test]
    fn differing_names_and_descriptions_need_an_override() {
        for key in ["name", "description"] {
            let err = inherit(&[json!({ key: "East region" }), json!({ key: "West region" })])
                .unwrap_err();
            assert!(err.contains(&format!("--{key}")), "{err}");
            assert!(err.contains("input0") && err.contains("input1"), "{err}");
            assert!(!err.contains("region"), "values are not printed: {err}");
        }
        // An override for one field does not settle another's conflict.
        let inputs = [
            json!({"name": "East", "attribution": "A"}),
            json!({"name": "West", "attribution": "B"}),
        ];
        let err = collect(&TilesetMetadata::new().with_attribution("C"), &inputs).unwrap_err();
        assert!(err.contains("--name"), "{err}");
        let ok = collect(&TilesetMetadata::new().with_name("Both"), &inputs).unwrap();
        assert_eq!(ok.name.as_deref(), Some("Both"));
        assert_eq!(ok.attribution.as_deref(), Some("A | B"));
    }

    #[test]
    fn attribution_credits_are_deduplicated_sorted_and_joined() {
        for (inputs, want) in [
            (&["B", "A", "B"][..], "A | B"),
            (&["", "B"], "B"),
            (&["", ""], ""),
            (&[" ", "B"], "  | B"),
            (&["A | B", "A"], "A | A | B"),
            (
                &["a", "\u{10000}", "A", "\u{e000}"],
                "A | a | \u{e000} | \u{10000}",
            ),
        ] {
            let got = inherit(&credits(inputs)).unwrap();
            assert_eq!(got.attribution.as_deref(), Some(want), "{inputs:?}");
        }
    }

    #[test]
    fn resolution_ignores_input_order() {
        let inputs = [
            json!({"name": "N", "attribution": "<b>x</b>"}),
            json!({"attribution": "\u{e9}"}),
            json!({"name": "N", "attribution": ""}),
            json!({"description": "D", "attribution": "<b>x</b>"}),
        ];
        let want = inherit(&inputs).unwrap();
        assert_eq!(
            want,
            TilesetMetadata::new()
                .with_name("N")
                .with_description("D")
                .with_attribution("<b>x</b> | \u{e9}")
        );
        for perm in [[3, 2, 1, 0], [1, 3, 0, 2], [2, 0, 3, 1]] {
            let shuffled: Vec<Value> = perm.iter().map(|&i| inputs[i].clone()).collect();
            assert_eq!(inherit(&shuffled), Ok(want.clone()), "{perm:?}");
        }
    }

    #[test]
    fn non_string_fields_are_errors_unless_overridden() {
        for bad in [json!({}), json!([]), json!(1), json!(true)] {
            for key in ["name", "description", "attribution"] {
                let err = inherit(&[json!({}), json!({ key: bad })]).unwrap_err();
                assert!(err.starts_with("input1: "), "{err}");
                assert!(err.contains(&format!("--{key}")), "{err}");
            }
        }
        let overridden = TilesetMetadata::new().with_attribution("credit");
        let got = collect(&overridden, &[json!({"attribution": [1, 2]})]).unwrap();
        assert_eq!(got.attribution.as_deref(), Some("credit"));
    }

    #[test]
    fn unknown_keys_and_non_object_roots_carry_nothing() {
        let unknown = json!({"title": [1], "credits": {"a": 1}, "vector_layers": "x"});
        let roots = [
            unknown,
            json!(null),
            json!("name"),
            json!(3),
            json!(["name"]),
        ];
        assert_eq!(inherit(&roots), Ok(TilesetMetadata::new()));
        // Publication fields count without any vector_layers.
        let bare = inherit(&[json!({"attribution": "A"})]).unwrap();
        assert_eq!(bare.attribution.as_deref(), Some("A"));
    }

    #[test]
    fn known_metadata_participates_like_json() {
        let mut c = MetadataCollector::new(&TilesetMetadata::new());
        c.add_json("archive", &json!({"name": "N", "attribution": "B"}))
            .unwrap();
        c.add_metadata(
            "band",
            &TilesetMetadata::new().with_name("N").with_attribution("A"),
        )
        .unwrap();
        let got = c.resolve().unwrap();
        assert_eq!(got.name.as_deref(), Some("N"));
        assert_eq!(got.attribution.as_deref(), Some("A | B"));
        let err = c
            .add_metadata("other", &TilesetMetadata::new().with_name("M"))
            .unwrap_err();
        assert!(err.contains("archive") && err.contains("other"), "{err}");
    }

    #[test]
    fn credits_that_cannot_fit_fail_as_they_arrive() {
        let half = MAX_INTERNAL_BYTES as usize / 2;
        let inputs = credits(&[&"a".repeat(half), &"b".repeat(half)]);
        let err = inherit(&inputs).unwrap_err();
        assert!(err.contains("--attribution"), "{err}");
        // Duplicates cost nothing, and an override discards them all.
        assert!(inherit(&credits(&[&"a".repeat(half), &"a".repeat(half)])).is_ok());
        let small = TilesetMetadata::new().with_attribution("credit");
        assert_eq!(
            collect(&small, &inputs).unwrap().attribution.as_deref(),
            Some("credit")
        );
    }
}
