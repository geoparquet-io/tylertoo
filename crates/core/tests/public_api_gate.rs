//! #437 gate on the committed public-API baseline (`api/tylertoo-core.txt`,
//! which CI's public-api job keeps equal to the real surface).
//!
//! 1. No stable signature names a pre-1.0 or fast-moving dependency, so a
//!    major release of one cannot force a tylertoo-core major release.
//! 2. Every `tylertoo_core::` path a stable signature names is itself in
//!    the baseline: a stable item does not lean on an `internal` one.

use std::collections::BTreeSet;

fn baseline() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/api/tylertoo-core.txt");
    std::fs::read_to_string(path).expect("read the public-API baseline")
}

/// Crate path prefixes no stable signature may name.
const FORBIDDEN: &[&str] = &[
    "arrow::",
    "arrow_",
    "parquet::",
    "geo_types::",
    "geo::",
    "geoarrow::",
    "geoarrow_",
    "object_store::",
];

/// Whether `line` names one of the [`FORBIDDEN`] crates at the start of a
/// path, not inside a longer identifier (`my_geo::`) or a path
/// (`tylertoo_core::X::arrow_bytes`).
fn names_forbidden_crate(line: &str) -> Option<&'static str> {
    FORBIDDEN.iter().copied().find(|prefix| {
        line.match_indices(prefix).any(|(at, _)| {
            !line[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':')
        })
    })
}

#[test]
fn the_stable_surface_names_no_third_party_data_types() {
    let leaks: Vec<_> = baseline()
        .lines()
        .filter_map(|line| names_forbidden_crate(line).map(|krate| format!("{krate}  {line}")))
        .collect();
    assert!(
        leaks.is_empty(),
        "stable signatures name third-party types; move them behind \
         `tylertoo_core::internal` or `#[doc(hidden)]`:\n{}",
        leaks.join("\n")
    );
}

/// The `tylertoo_core::…` paths in `text`, each cut at its end.
fn core_paths(text: &str) -> impl Iterator<Item = &str> {
    text.match_indices("tylertoo_core::").map(move |(at, _)| {
        let rest = &text[at..];
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
            .unwrap_or(rest.len());
        rest[..end].trim_end_matches(':')
    })
}

#[test]
fn every_type_a_stable_signature_names_is_stable() {
    let text = baseline();
    // The item each `pub struct|enum|trait|type|fn|const|static|mod` line declares.
    let declared: BTreeSet<&str> = text
        .lines()
        .filter_map(|line| {
            // Skip leading attributes such as `#[non_exhaustive] #[repr(u8)]`.
            let mut line = line;
            while let Some(rest) = line.strip_prefix("#[") {
                line = rest.split_once("] ")?.1;
            }
            let rest = line.strip_prefix("pub ")?;
            let (keyword, item) = rest.split_once(' ')?;
            matches!(
                keyword,
                "struct" | "enum" | "trait" | "type" | "fn" | "const" | "static" | "mod"
            )
            .then(|| core_paths(item).next())
            .flatten()
        })
        .collect();
    let missing: BTreeSet<&str> = core_paths(&text)
        .filter(|path| {
            // A path is covered when it, or an item it is a member of, is declared.
            let mut prefix = *path;
            loop {
                if declared.contains(prefix) {
                    return false;
                }
                match prefix.rfind("::") {
                    Some(cut) => prefix = &prefix[..cut],
                    None => return true,
                }
            }
        })
        .collect();
    assert!(
        missing.is_empty(),
        "stable signatures name items that are not stable; re-export them at \
         the crate root or hide the signature:\n{}",
        missing.into_iter().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn the_crate_matcher_ignores_longer_identifiers() {
    assert_eq!(names_forbidden_crate("pub fn f(geo::Point)"), Some("geo::"));
    assert_eq!(
        names_forbidden_crate("(&arrow_schema::Schema)"),
        Some("arrow_")
    );
    assert_eq!(names_forbidden_crate("pub fn f(my_geo::Point)"), None);
    assert_eq!(names_forbidden_crate("pub fn sparrow_count()"), None);
    assert_eq!(
        names_forbidden_crate("pub fn tylertoo_core::X::arrow_bytes()"),
        None
    );
}
