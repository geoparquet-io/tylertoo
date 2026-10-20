//! Archive-level publication metadata: the top-level `name`, `description`
//! and `attribution` strings of a PMTiles archive's JSON metadata.
//!
//! These describe the published tileset as a whole, separately from the MVT
//! layer name (`vector_layers[].id`) and from feature properties. Readers such
//! as the PMTiles JavaScript adapter copy them into TileJSON, where MapLibre
//! shows `attribution` in its attribution control.

use std::collections::BTreeMap;

use serde::Serialize;
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
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
    /// the appended string.
    pub(crate) fn serialized_len(&self) -> u64 {
        self.fields()
            .map(|(key, value)| field_len(key, json_string_len(value)))
            .sum()
    }

    /// Reject values that cannot fit an archive's metadata on their own.
    ///
    /// The structural metadata adds bytes and the final encoded section is
    /// checked again when the archive is assembled; this is the cheap lower
    /// bound a caller can apply before any conversion work starts.
    pub(crate) fn check_size(&self) -> Result<(), String> {
        let len = self.serialized_len() + STANDALONE_FRAMING;
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

/// What [`TilesetMetadata::append_json_fields`] appends for one field,
/// `,"key":` plus a value serialized to `value_len` bytes (quotes included).
fn field_len(key: &str, value_len: u64) -> u64 {
    key.len() as u64 + 4 + value_len
}

/// Bytes the appended fields gain as an object of their own: the leading
/// comma becomes `{`, plus the closing `}`.
const STANDALONE_FRAMING: u64 = 1;

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

/// Where an inherited value came from: the first input that carried it, and
/// how many inputs carried it.
#[derive(Debug)]
struct Source {
    first: String,
    count: usize,
}

impl Source {
    fn new(label: &str) -> Self {
        Self {
            first: label.to_string(),
            count: 1,
        }
    }

    /// `a.pmtiles`, or `a.pmtiles and 3 other inputs`.
    fn describe(&self) -> String {
        match self.count {
            1 => self.first.clone(),
            2 => format!("{} and 1 other input", self.first),
            n => format!("{} and {} other inputs", self.first, n - 1),
        }
    }
}

/// What the inputs carry for a name or a description.
#[derive(Debug, Default)]
enum Inherited {
    /// No input carried a value.
    #[default]
    None,
    /// Every input that carried a value carried this one.
    One(String, Source),
    /// Two inputs carried different values; these are their labels. The
    /// values themselves are dropped, so a fleet of differing names costs
    /// no more memory than two.
    Conflict(String, String),
}

/// Resolves the publication metadata of an archive composed from others
/// (`merge`, `pyramid`).
///
/// Each field resolves on its own. An explicit override (including `""`)
/// wins and that field's inherited values are not even read. Otherwise:
/// a name or description the inputs agree on is kept, and one they
/// disagree on is left out ([`Resolved::conflicts`]); attribution credits
/// are all kept, deduplicated exactly, sorted by Rust string order and
/// joined with `" | "` (an inherited `""` counts only when no input has a
/// credit). Absent and `null` fields are no value, and any other non-string
/// is ignored with a warning.
///
/// A disagreement is not an error because composing archives from other
/// tools has to keep working: tippecanoe names every archive after its
/// output file, so two of its archives always disagree. Leaving the field
/// out is order-independent, and the same thing an unset field does.
///
/// The resolved metadata does not depend on the order inputs are added in.
/// Which inputs a message names can.
///
/// DIVERGENCE FROM TIPPECANOE: tile-join keeps the last input's attribution
/// and description, joins differing names with " + ", and ignores an empty
/// override (tile-join.cpp, near L1220). We keep every input's credit so a
/// composed map still credits all of its sources, leave out a name or
/// description rather than build one from the input order, and let `""`
/// clear a field explicitly.
#[derive(Debug)]
pub(crate) struct MetadataCollector {
    overrides: TilesetMetadata,
    name: Inherited,
    description: Inherited,
    /// Distinct non-empty attribution credits, and where each came from.
    credits: BTreeMap<String, Source>,
    /// Escaped bytes of every credit in `credits`, without quotes or
    /// separators: what the joined value costs in the metadata JSON.
    credit_bytes: u64,
    /// Whether some input carried `"attribution": ""`.
    empty_credit: bool,
    /// Inputs carrying a field that is neither a string nor `null`, with
    /// the field's key. Warned about by [`Self::log_resolution`], so a
    /// caller that collects twice (`pyramid`'s preflight, then its merge)
    /// warns once.
    non_strings: Vec<(String, &'static str)>,
}

/// What [`MetadataCollector::resolve`] settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// The output archive's publication metadata.
    pub(crate) metadata: TilesetMetadata,
    /// The keys (`"name"`, `"description"`) the inputs disagreed on, which
    /// the output leaves out.
    pub(crate) conflicts: Vec<String>,
}

impl MetadataCollector {
    /// A collector whose result applies `overrides` field by field.
    pub(crate) fn new(overrides: &TilesetMetadata) -> Self {
        Self {
            overrides: overrides.clone(),
            name: Inherited::None,
            description: Inherited::None,
            credits: BTreeMap::new(),
            credit_bytes: 0,
            empty_credit: false,
            non_strings: Vec::new(),
        }
    }

    fn overridden(&self, field: Field) -> bool {
        field.of(&self.overrides).is_some()
    }

    /// Add one input archive's metadata JSON, by its whole root.
    ///
    /// A root that is not an object carries no publication fields. Unknown
    /// keys are ignored, and so is a field that is neither a string nor
    /// `null` ([`Self::log_resolution`] warns about it, naming `label`).
    ///
    /// # Errors
    ///
    /// Returns a message naming `label` if its attribution credit takes the
    /// inputs' joined credits past what an archive's metadata can hold.
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
                Some(_) => self.non_strings.push((label.to_string(), field.key())),
            }
        }
        Ok(())
    }

    fn add_value(&mut self, label: &str, field: Field, value: &str) -> Result<(), String> {
        let slot = match field {
            Field::Name => &mut self.name,
            Field::Description => &mut self.description,
            Field::Attribution => return self.add_credit(label, value),
        };
        match slot {
            Inherited::None => *slot = Inherited::One(value.to_string(), Source::new(label)),
            Inherited::One(seen, source) if seen == value => source.count += 1,
            Inherited::One(_, source) => {
                let first = std::mem::take(&mut source.first);
                *slot = Inherited::Conflict(first, label.to_string());
            }
            Inherited::Conflict(..) => {}
        }
        Ok(())
    }

    fn add_credit(&mut self, label: &str, credit: &str) -> Result<(), String> {
        if credit.is_empty() {
            self.empty_credit = true;
            return Ok(());
        }
        if let Some(source) = self.credits.get_mut(credit) {
            source.count += 1;
            return Ok(());
        }
        // Without its quotes: the joined value is quoted once, below.
        self.credit_bytes += json_string_len(credit) - 2;
        self.credits.insert(credit.to_string(), Source::new(label));
        // Checked as credits arrive, so a merge of many inputs fails at the
        // input whose credit cannot fit rather than after joining them all.
        // The bound is the one `check_size` applies to the joined value.
        let separators = (self.credits.len() as u64 - 1) * CREDIT_SEPARATOR.len() as u64;
        let joined = self.credit_bytes + separators + 2;
        let len = field_len(Field::Attribution.key(), joined) + STANDALONE_FRAMING;
        if len > MAX_INTERNAL_BYTES {
            return Err(format!(
                "{label}: its attribution credit takes the inputs' joined credits to {len} \
                 bytes, over the {MAX_INTERNAL_BYTES}-byte limit on an archive's metadata; \
                 override the output's attribution (--attribution)"
            ));
        }
        Ok(())
    }

    /// The output's publication metadata, and the fields left out because
    /// the inputs disagreed on them.
    ///
    /// # Errors
    ///
    /// Returns a message if the resolved values are too large for an
    /// archive's metadata.
    pub(crate) fn resolve(&self) -> Result<Resolved, String> {
        let mut conflicts = Vec::new();
        let mut inherit = |field: Field, slot: &Inherited| {
            if let Some(value) = field.of(&self.overrides) {
                return Some(value.to_string());
            }
            match slot {
                Inherited::None => None,
                Inherited::One(value, _) => Some(value.clone()),
                Inherited::Conflict(..) => {
                    conflicts.push(field.key().to_string());
                    None
                }
            }
        };
        let name = inherit(Field::Name, &self.name);
        let description = inherit(Field::Description, &self.description);
        let attribution = self.overrides.attribution.clone().or_else(|| {
            if !self.credits.is_empty() {
                let credits: Vec<&str> = self.credits.keys().map(String::as_str).collect();
                Some(credits.join(CREDIT_SEPARATOR))
            } else {
                self.empty_credit.then(String::new)
            }
        });
        let metadata = TilesetMetadata {
            name,
            description,
            attribution,
        };
        metadata.check_size()?;
        Ok(Resolved {
            metadata,
            conflicts,
        })
    }

    /// Log where the output's publication metadata came from.
    ///
    /// Every inherited value at `info`, naming the input that carried it: an
    /// input archive's attribution is HTML the output's map will render, so
    /// what was inherited, and from which input, belongs on record. At
    /// `warn`, each field the inputs disagreed on, each ignored non-string
    /// field, and credits that differ only in case or spacing (one mistyped
    /// job in a fleet joins as a second credit).
    pub(crate) fn log_resolution(&self) {
        // Named, never printed: the value could be anything, of any size.
        for (label, key) in &self.non_strings {
            log::warn!("{label}: its {key} is not a string, so the output does not inherit it");
        }
        for (field, slot) in [
            (Field::Name, &self.name),
            (Field::Description, &self.description),
        ] {
            match slot {
                Inherited::None => {}
                Inherited::One(value, source) => log::info!(
                    "inherited the PMTiles {} {} from {}",
                    field.key(),
                    preview(value),
                    source.describe()
                ),
                Inherited::Conflict(first, second) => log::warn!(
                    "{first} and {second} carry different PMTiles {key}s, so the output has \
                     none; set its {key} (--{key}) to give it one",
                    key = field.key()
                ),
            }
        }
        for (credit, source) in &self.credits {
            log::info!(
                "inherited the attribution credit {} from {}",
                preview(credit),
                source.describe()
            );
        }
        let mut by_shape: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for credit in self.credits.keys() {
            let shape = credit.split_whitespace().collect::<Vec<_>>().join(" ");
            by_shape
                .entry(shape.to_lowercase())
                .or_default()
                .push(credit);
        }
        for near in by_shape.values().filter(|credits| credits.len() > 1) {
            let listed: Vec<String> = near.iter().map(|c| preview(c)).collect();
            log::warn!(
                "attribution credits {} differ only in case or spacing, and all of them are \
                 kept; check the inputs, or set the output's attribution (--attribution)",
                listed.join(", ")
            );
        }
    }
}

/// `value` quoted and escaped for a message, cut after its first 200
/// characters: a credit can be long HTML.
pub(crate) fn preview(value: &str) -> String {
    const MAX_CHARS: usize = 200;
    match value.char_indices().nth(MAX_CHARS) {
        None => format!("{value:?}"),
        Some((cut, _)) => format!("{:?}… ({} bytes)", &value[..cut], value.len()),
    }
}

/// Length of `value` once serialized as a JSON string, quotes included.
fn json_string_len(value: &str) -> u64 {
    // Serializing a `str` cannot fail.
    serde_json::to_string(value)
        .expect("JSON string serialization")
        .len() as u64
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
        serde_json::from_str(&json).expect("appended fields must form a JSON object")
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

    #[test]
    fn credit_check_and_check_size_agree_at_the_limit() {
        // The largest single credit both accept, and one byte more.
        let fits = MAX_INTERNAL_BYTES as usize - 18;
        let at = |n: usize| credits(&["x".repeat(n).as_str()]);
        assert!(inherit(&at(fits)).is_ok());
        assert!(TilesetMetadata::new()
            .with_attribution("x".repeat(fits))
            .check_size()
            .is_ok());
        assert!(inherit(&at(fits + 1)).is_err());
        assert!(TilesetMetadata::new()
            .with_attribution("x".repeat(fits + 1))
            .check_size()
            .is_err());
    }

    #[test]
    fn previews_quote_and_cut_long_values() {
        assert_eq!(preview("<b>\"x\"</b>"), r#""<b>\"x\"</b>""#);
        let long = "\u{e9}".repeat(300);
        let cut = preview(&long);
        assert!(
            cut.starts_with(&format!("\"{}\"…", "\u{e9}".repeat(200))),
            "{cut}"
        );
        assert!(cut.ends_with("(600 bytes)"), "{cut}");
        assert_eq!(Source::new("a").describe(), "a");
        let mut s = Source::new("a");
        s.count = 2;
        assert_eq!(s.describe(), "a and 1 other input");
        s.count = 4;
        assert_eq!(s.describe(), "a and 3 other inputs");
    }

    // -------------------------------------------------------------------------
    // MetadataCollector
    // -------------------------------------------------------------------------

    fn resolve(overrides: &TilesetMetadata, inputs: &[Value]) -> Result<Resolved, String> {
        let mut c = MetadataCollector::new(overrides);
        for (i, root) in inputs.iter().enumerate() {
            c.add_json(&format!("input{i}"), root)?;
        }
        c.resolve()
    }

    fn collect(overrides: &TilesetMetadata, inputs: &[Value]) -> Result<TilesetMetadata, String> {
        resolve(overrides, inputs).map(|r| r.metadata)
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
    fn differing_names_and_descriptions_are_left_out() {
        for key in ["name", "description"] {
            let got = resolve(
                &TilesetMetadata::new(),
                &[json!({ key: "East region" }), json!({ key: "West region" })],
            )
            .unwrap();
            assert_eq!(got.metadata, TilesetMetadata::new(), "{key}");
            assert_eq!(got.conflicts, vec![key]);
        }
        // Every field is settled at once, and an override for one field does
        // not settle another's disagreement.
        let inputs = [
            json!({"name": "East", "description": "E", "attribution": "A"}),
            json!({"name": "West", "description": "W", "attribution": "B"}),
        ];
        let got = resolve(&TilesetMetadata::new().with_attribution("C"), &inputs).unwrap();
        assert_eq!(got.conflicts, vec!["name", "description"]);
        assert_eq!(got.metadata.attribution.as_deref(), Some("C"));
        let got = resolve(&TilesetMetadata::new().with_name("Both"), &inputs).unwrap();
        assert_eq!(got.conflicts, vec!["description"]);
        assert_eq!(got.metadata.name.as_deref(), Some("Both"));
        assert_eq!(got.metadata.attribution.as_deref(), Some("A | B"));
    }

    /// tippecanoe names every archive after its output file and copies the
    /// name into `description`, so composing two of them is the common case
    /// for a disagreement; it must not depend on how many inputs agree first.
    #[test]
    fn a_disagreement_stays_one_in_any_order() {
        let inputs = [
            json!({"name": "a.pmtiles", "description": "a.pmtiles"}),
            json!({"name": "a.pmtiles", "description": "a.pmtiles"}),
            json!({"name": "b.pmtiles", "description": "b.pmtiles"}),
        ];
        for perm in [[0, 1, 2], [2, 0, 1], [0, 2, 1]] {
            let shuffled: Vec<Value> = perm.iter().map(|&i| inputs[i].clone()).collect();
            let got = resolve(&TilesetMetadata::new(), &shuffled).unwrap();
            assert_eq!(got.metadata, TilesetMetadata::new(), "{perm:?}");
            assert_eq!(got.conflicts, vec!["name", "description"], "{perm:?}");
        }
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
    fn non_string_fields_are_ignored() {
        for bad in [json!({}), json!([]), json!(1), json!(true)] {
            for key in ["name", "description", "attribution"] {
                let got = inherit(&[json!({ key: "kept" }), json!({ key: bad })]).unwrap();
                assert_eq!(
                    got,
                    collect(&TilesetMetadata::new(), &[json!({ key: "kept" })]).unwrap(),
                    "{key}: {bad}"
                );
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
    fn credits_that_cannot_fit_fail_as_they_arrive() {
        let half = MAX_INTERNAL_BYTES as usize / 2;
        let inputs = credits(&[&"a".repeat(half), &"b".repeat(half)]);
        let err = inherit(&inputs).unwrap_err();
        assert!(err.contains("--attribution"), "{err}");
        assert!(err.starts_with("input1: "), "names the input: {err}");
        // Duplicates cost nothing, and an override discards them all.
        assert!(inherit(&credits(&[&"a".repeat(half), &"a".repeat(half)])).is_ok());
        let small = TilesetMetadata::new().with_attribution("credit");
        assert_eq!(
            collect(&small, &inputs).unwrap().attribution.as_deref(),
            Some("credit")
        );
    }
}
