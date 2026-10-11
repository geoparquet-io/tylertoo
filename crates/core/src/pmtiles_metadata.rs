//! Archive-level publication metadata: the top-level `name`, `description`
//! and `attribution` strings of a PMTiles archive's JSON metadata.
//!
//! These describe the published tileset as a whole, separately from the MVT
//! layer name (`vector_layers[].id`) and from feature properties. Readers such
//! as the PMTiles JavaScript adapter copy them into TileJSON, where MapLibre
//! shows `attribution` in its attribution control.

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
}
