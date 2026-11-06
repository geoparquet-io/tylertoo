//! #437 gate on the committed public-API baseline (`api/tylertoo-core.txt`,
//! which CI's public-api job keeps equal to the real surface).
//!
//! 1. Every path a stable signature names starts at an allowed root
//!    ([`ALLOWED_ROOTS`]), so no dependency's major release can force a
//!    tylertoo-core major release.
//! 2. Every `tylertoo_core::` path a stable signature names is itself in
//!    the baseline: a stable item does not lean on an `internal` one.

use std::collections::BTreeSet;

fn baseline() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/api/tylertoo-core.txt");
    std::fs::read_to_string(path).expect("read the public-API baseline")
}

/// The path roots a stable signature may name: this crate, the standard
/// library, serde (the JSON wire shape), and `serde_json` (1.x; its error type
/// is opaque). Anything else is a dependency whose major release would force
/// one here: hide the signature or wrap the type.
const ALLOWED_ROOTS: &[&str] = &[
    "tylertoo_core",
    "core",
    "alloc",
    "std",
    "serde_core",
    "serde_json",
    "Self",
];

/// The root of every path in `line`: the identifier before a `::` that is
/// not itself preceded by an identifier character or `:`.
fn path_roots(line: &str) -> impl Iterator<Item = &str> {
    line.match_indices("::").filter_map(move |(at, _)| {
        let head = &line[..at];
        let start = head
            .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map_or(0, |i| i + 1);
        let root = &head[start..];
        let preceded = head[..start].ends_with(':');
        (!root.is_empty() && !preceded).then_some(root)
    })
}

/// The baseline, checked to be the real one: an empty or truncated file
/// would pass every check below.
fn checked_baseline() -> String {
    let text = baseline();
    assert!(
        text.lines().count() > 1000
            && text
                .lines()
                .any(|l| l.starts_with("pub fn tylertoo_core::convert_to_overviews")),
        "api/tylertoo-core.txt is not the public-API baseline (too short, or no \
         `pub fn tylertoo_core::convert_to_overviews`); regenerate it"
    );
    text
}

#[test]
fn the_stable_surface_names_only_allowed_path_roots() {
    let text = checked_baseline();
    let leaks: Vec<_> = text
        .lines()
        .flat_map(|line| {
            path_roots(line)
                .filter(|root| !ALLOWED_ROOTS.contains(root))
                .map(move |root| format!("{root}  {line}"))
        })
        .collect();
    assert!(
        leaks.is_empty(),
        "stable signatures name dependency types; move them behind \
         `tylertoo_core::internal` or `#[doc(hidden)]`, or wrap them:\n{}",
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
    let text = checked_baseline();
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
fn the_root_matcher_finds_only_path_starts() {
    let roots = |line| path_roots(line).collect::<Vec<_>>();
    assert_eq!(roots("pub fn f(prost::error::DecodeError)"), ["prost"]);
    assert_eq!(roots("(&arrow_schema::Schema)"), ["arrow_schema"]);
    assert_eq!(
        roots("<__S as serde_core::ser::Serializer>::Ok"),
        ["serde_core"]
    );
    assert_eq!(
        roots("pub fn tylertoo_core::X::arrow_bytes()"),
        ["tylertoo_core"]
    );
    assert_eq!(roots("pub type Self::Err = u8"), ["Self"]);
    assert!(roots("pub fn sparrow_count()").is_empty());
}
