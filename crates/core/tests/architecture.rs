//! Structural ratchets (#779): counts of code that bypasses a concept's
//! owner, pinned in `ci/ratchets.toml` so they can only go down.
//!
//! Each count is computed from source here, with a small lexer (comments,
//! strings and char literals masked) and plain string matching: no parser,
//! no new dependencies. The test fails when a count is **above** its pin (a
//! change added a bypass: route it through the owner instead) and also when
//! it is **below** its pin (a change removed one: lower the pin to the new
//! count in the same PR, so the gain is locked in).
//!
//! What counts as production code is spelled out in `ci/ratchets.toml`'s
//! header and implemented by [`SourceFile::load`]: `crates/*/src/**/*.rs`
//! minus every item under `#[cfg(test)]` / `#[cfg(all(test, ..))]` and every
//! file such an item declares (`#[cfg(test)] mod testutil;`).
//!
//! The similarity ratchets in the same file are checked by
//! `scripts/ci/similarity_ratchet.py` (they need the `similarity-rs`
//! binary); this test only skips them.
//!
//! Run with: cargo test --package tylertoo-core --test architecture -- --nocapture

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

// ---------------------------------------------------------------------------
// Lexing: mask comments, string contents and char literals so brace matching
// and token search only see code.
// ---------------------------------------------------------------------------

/// A string literal: byte range of its contents in the source, and the text.
#[derive(Debug, Clone)]
struct StrLit {
    start: usize,
    content: String,
}

/// `src` with comments and literal contents replaced by spaces (newlines
/// kept, so byte offsets and line numbers match the original).
struct Lexed {
    masked: Vec<u8>,
    strings: Vec<StrLit>,
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// When a raw string (`r"`, `r#"`, `br"`) opens at `i`: the index just past
/// its opening quote, and its number of `#`s.
fn raw_string_open(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let k = match (b.get(i), b.get(i + 1)) {
        (Some(b'r'), _) => i + 1,
        (Some(b'b'), Some(b'r')) => i + 2,
        _ => return None,
    };
    let hashes = b[k..].iter().take_while(|&&x| x == b'#').count();
    (b.get(k + hashes) == Some(&b'"')).then_some((k + hashes + 1, hashes))
}

fn lex(src: &str) -> Lexed {
    let b = src.as_bytes();
    let n = b.len();
    let mut masked = b.to_vec();
    let mut strings = Vec::new();
    let blank = |m: &mut Vec<u8>, from: usize, to: usize| {
        for c in &mut m[from..to.min(n)] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    let mut i = 0;
    while i < n {
        let c = b[i];
        let prev_ident = i > 0 && is_ident(b[i - 1]);
        if c == b'/' && i + 1 < n && b[i + 1] == b'/' {
            let end = b[i..].iter().position(|&x| x == b'\n').map_or(n, |p| i + p);
            blank(&mut masked, i, end);
            i = end;
        } else if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            let mut depth = 0usize;
            let mut j = i;
            while j < n {
                if b[j] == b'/' && j + 1 < n && b[j + 1] == b'*' {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && j + 1 < n && b[j + 1] == b'/' {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            blank(&mut masked, i, j);
            i = j;
        } else if let Some((body_start, hashes)) = raw_string_open(b, i).filter(|_| !prev_ident) {
            // Raw string r"..." / r#"..."# / br"...".
            let mut close = vec![b'"'];
            close.extend(std::iter::repeat_n(b'#', hashes));
            let body_end = b[body_start..]
                .windows(close.len())
                .position(|w| w == close.as_slice())
                .map_or(n, |p| body_start + p);
            strings.push(StrLit {
                start: body_start,
                content: src[body_start..body_end].to_string(),
            });
            blank(&mut masked, body_start, body_end);
            i = (body_end + close.len()).min(n);
        } else if c == b'"' {
            let body_start = i + 1;
            let mut j = body_start;
            while j < n && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let body_end = j.min(n);
            strings.push(StrLit {
                start: body_start,
                content: src[body_start..body_end].to_string(),
            });
            blank(&mut masked, body_start, body_end);
            i = body_end + 1;
        } else if c == b'\'' {
            // Char literal or lifetime/label.
            if i + 1 < n && b[i + 1] == b'\\' {
                let mut j = i + 2;
                while j < n && b[j] != b'\'' {
                    j += 1;
                }
                blank(&mut masked, i + 1, j);
                i = j + 1;
            } else if i + 1 < n {
                let len = utf8_len(b[i + 1]);
                if i + 1 + len < n && b[i + 1 + len] == b'\'' {
                    blank(&mut masked, i + 1, i + 1 + len);
                    i += len + 2;
                } else {
                    i += 1; // lifetime: leave as code
                }
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    Lexed { masked, strings }
}

// ---------------------------------------------------------------------------
// Test-region detection.
// ---------------------------------------------------------------------------

/// Index just past the bracket that closes the one at `open`.
fn matching(m: &[u8], open: usize, o: u8, c: u8) -> usize {
    let mut depth = 0usize;
    for (j, &x) in m.iter().enumerate().skip(open) {
        if x == o {
            depth += 1;
        } else if x == c {
            depth -= 1;
            if depth == 0 {
                return j + 1;
            }
        }
    }
    m.len()
}

fn skip_ws(m: &[u8], mut i: usize) -> usize {
    while i < m.len() && m[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Split `s` at commas outside any bracket.
fn split_top_level(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    out.push(cur);
    out
}

/// Whether an attribute body (between `#[` and `]`, whitespace removed)
/// compiles its item only under `cfg(test)`.
fn is_test_cfg(attr: &str) -> bool {
    if attr == "cfg(test)" {
        return true;
    }
    if let Some(inner) = attr
        .strip_prefix("cfg(all(")
        .and_then(|s| s.strip_suffix("))"))
    {
        return split_top_level(inner).iter().any(|a| a == "test");
    }
    false
}

/// Byte ranges of `#[cfg(test)]` items, and the names of `mod x;` items
/// among them (their files are test code too).
fn test_regions(m: &[u8]) -> (Vec<(usize, usize)>, Vec<String>) {
    let mut regions = Vec::new();
    let mut mods = Vec::new();
    let mut i = 0;
    while let Some(p) = find(m, b"#[", i) {
        let attr_end = matching(m, p + 1, b'[', b']');
        let body: String = String::from_utf8_lossy(&m[p + 2..attr_end - 1])
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if !is_test_cfg(&body) {
            i = p + 2;
            continue;
        }
        // Skip any further attributes on the same item.
        let mut j = skip_ws(m, attr_end);
        while m.get(j) == Some(&b'#') && m.get(j + 1) == Some(&b'[') {
            j = skip_ws(m, matching(m, j + 1, b'[', b']'));
        }
        let item_start = j;
        // The item ends at its first top-level `;` or its `{ .. }` block.
        let mut depth = 0i32;
        let mut end = m.len();
        let mut k = item_start;
        while k < m.len() {
            match m[k] {
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                b'{' if depth == 0 => {
                    end = matching(m, k, b'{', b'}');
                    break;
                }
                b';' if depth == 0 => {
                    end = k + 1;
                    let item = String::from_utf8_lossy(&m[item_start..k]);
                    let words: Vec<&str> = item.split_whitespace().collect();
                    if let Some(pos) = words.iter().position(|w| *w == "mod") {
                        if let Some(name) = words.get(pos + 1) {
                            mods.push((*name).to_string());
                        }
                    }
                    break;
                }
                _ => {}
            }
            k += 1;
        }
        regions.push((p, end));
        i = end;
    }
    (regions, mods)
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Positions of `word` with identifier boundaries on both sides.
fn find_words(m: &[u8], word: &str) -> Vec<usize> {
    let w = word.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = find(m, w, i) {
        let before = p == 0 || !is_ident(m[p - 1]);
        let after = p + w.len() >= m.len() || !is_ident(m[p + w.len()]);
        if before && after {
            out.push(p);
        }
        i = p + 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Source inventory.
// ---------------------------------------------------------------------------

struct SourceFile {
    /// Path relative to the repo root, with `/` separators.
    rel: String,
    raw: String,
    lexed: Lexed,
    /// Per byte: true when the byte is production code.
    prod: Vec<bool>,
    line_starts: Vec<usize>,
}

impl SourceFile {
    fn load(root: &Path, path: &Path, whole_file_is_test: bool) -> (Self, Vec<String>) {
        let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let lexed = lex(&raw);
        let mut prod = vec![!whole_file_is_test; raw.len()];
        let (regions, mods) = test_regions(&lexed.masked);
        if find(&lexed.masked, b"#![cfg(test)]", 0).is_some() {
            prod.iter_mut().for_each(|p| *p = false);
        }
        for (s, e) in regions {
            prod[s..e.min(raw.len())]
                .iter_mut()
                .for_each(|p| *p = false);
        }
        let line_starts = std::iter::once(0)
            .chain(raw.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        (
            SourceFile {
                rel,
                raw,
                lexed,
                prod,
                line_starts,
            },
            mods,
        )
    }

    fn line_of(&self, pos: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= pos)
    }

    fn line_text(&self, line: usize) -> &str {
        self.raw.lines().nth(line - 1).unwrap_or("").trim()
    }

    fn is_prod(&self, pos: usize) -> bool {
        self.prod.get(pos).copied().unwrap_or(false)
    }

    fn site(&self, pos: usize) -> String {
        let line = self.line_of(pos);
        format!("{}:{line}: {}", self.rel, self.line_text(line))
    }
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// The directory a module file's child modules live in.
fn child_mod_dir(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap();
    match path.file_stem().and_then(|s| s.to_str()) {
        Some("mod" | "lib" | "main") => parent.to_path_buf(),
        Some(stem) => parent.join(stem),
        None => parent.to_path_buf(),
    }
}

/// Every `.rs` file under `crates/*/src`, with test code marked.
fn src_files(root: &Path) -> Vec<SourceFile> {
    let mut paths = Vec::new();
    for krate in ["core", "cli", "python"] {
        rs_files(&root.join("crates").join(krate).join("src"), &mut paths);
    }
    // First pass: find files declared by `#[cfg(test)] mod x;`.
    let mut test_files: Vec<PathBuf> = Vec::new();
    let mut test_dirs: Vec<PathBuf> = Vec::new();
    for p in &paths {
        let src = fs::read_to_string(p).unwrap();
        let (_, mods) = test_regions(&lex(&src).masked);
        let dir = child_mod_dir(p);
        for m in mods {
            test_files.push(dir.join(format!("{m}.rs")));
            test_dirs.push(dir.join(&m));
        }
    }
    paths
        .iter()
        .map(|p| {
            let is_test =
                test_files.iter().any(|t| t == p) || test_dirs.iter().any(|d| p.starts_with(d));
            SourceFile::load(root, p, is_test).0
        })
        .collect()
}

/// Every `.rs` file under `crates/` (src, tests, benches), for checks that
/// are about test code itself.
fn all_crate_rs(root: &Path) -> Vec<SourceFile> {
    let mut paths = Vec::new();
    rs_files(&root.join("crates"), &mut paths);
    paths
        .iter()
        .filter(|p| !p.components().any(|c| c.as_os_str() == "target"))
        .map(|p| SourceFile::load(root, p, false).0)
        .collect()
}

// ---------------------------------------------------------------------------
// Metrics. Each returns the offending sites; the count is their number.
// ---------------------------------------------------------------------------

type Metric = fn(&Inventory) -> Vec<String>;

struct Inventory {
    root: PathBuf,
    src: Vec<SourceFile>,
    all: Vec<SourceFile>,
}

fn prod_files<'a>(inv: &'a Inventory, prefix: &'a str) -> impl Iterator<Item = &'a SourceFile> {
    inv.src.iter().filter(move |f| f.rel.starts_with(prefix))
}

/// Production occurrences of `needle` (plain substring of masked code).
fn prod_occurrences(inv: &Inventory, prefix: &str, needles: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for f in prod_files(inv, prefix) {
        for n in needles {
            let mut i = 0;
            while let Some(p) = find(&f.lexed.masked, n.as_bytes(), i) {
                let boundary = p == 0 || !is_ident(f.lexed.masked[p - 1]);
                if boundary && f.is_prod(p) {
                    out.push(f.site(p));
                }
                i = p + 1;
            }
        }
    }
    out.sort();
    out
}

/// Every way to open a parquet file for reading.
fn parquet_open_sites(inv: &Inventory) -> Vec<String> {
    prod_occurrences(
        inv,
        "crates/",
        &[
            "ParquetRecordBatchReaderBuilder::",
            "ParquetRecordBatchStreamBuilder::",
            "ArrowReaderBuilder::",
            "SerializedFileReader::new",
            "SerializedFileReader::try_from",
            "ParquetMetaDataReader::new",
            "ArrowReaderMetadata::load",
            "ArrowReaderMetadata::try_new",
        ],
    )
}

/// Files whose production code holds the literal `"geometry"`.
fn geometry_literal_files(inv: &Inventory) -> Vec<String> {
    let mut out = Vec::new();
    for f in prod_files(inv, "crates/core/src/") {
        if let Some(s) = f
            .lexed
            .strings
            .iter()
            .find(|s| s.content == "geometry" && f.is_prod(s.start))
        {
            out.push(f.site(s.start));
        }
    }
    out
}

/// `static NAME: T` items in core production code, with the last path
/// segment of `T`.
fn core_statics(inv: &Inventory) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for f in prod_files(inv, "crates/core/src/") {
        let m = &f.lexed.masked;
        for p in find_words(m, "static") {
            if !f.is_prod(p) || (p > 0 && m[p - 1] == b'\'') {
                continue;
            }
            let rest = String::from_utf8_lossy(&m[p + 6..(p + 200).min(m.len())]).to_string();
            let rest = rest.trim_start().trim_start_matches("mut ").trim_start();
            let Some(colon) = rest.find(':') else {
                continue;
            };
            let ty: String = rest[colon + 1..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
                .collect();
            let last = ty.rsplit("::").next().unwrap_or("").to_string();
            out.push((f.site(p), last));
        }
    }
    out
}

/// Process-global "warn once" flags: `static X: Once` / `AtomicBool`.
fn static_once_warnings(inv: &Inventory) -> Vec<String> {
    core_statics(inv)
        .into_iter()
        .filter(|(_, t)| t == "Once" || t == "AtomicBool")
        .map(|(s, _)| s)
        .collect()
}

/// Process-global counters: `static X: AtomicU64` and friends.
fn global_atomic_counters(inv: &Inventory) -> Vec<String> {
    core_statics(inv)
        .into_iter()
        .filter(|(_, t)| {
            matches!(
                t.as_str(),
                "AtomicU64" | "AtomicUsize" | "AtomicU32" | "AtomicI64" | "AtomicIsize"
            )
        })
        .map(|(s, _)| s)
        .collect()
}

fn cli_bail_calls(inv: &Inventory) -> Vec<String> {
    prod_occurrences(inv, "crates/cli/src/", &["bail!"])
}

fn too_many_arguments_allows(inv: &Inventory) -> Vec<String> {
    prod_occurrences(inv, "crates/", &["clippy::too_many_arguments"])
}

/// Skip `<...>` starting at `i` (which holds `<`); `->` is not a bracket.
fn skip_angle(m: &[u8], mut i: usize) -> usize {
    let mut depth = 0i32;
    while i < m.len() {
        match m[i] {
            b'-' if m.get(i + 1) == Some(&b'>') => i += 1,
            b'<' => depth += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    i
}

/// Number of parameters of the `fn` keyword at `p`, or `None` when it is a
/// function-pointer type (`fn(u32) -> u32`) rather than an item.
fn fn_param_count(m: &[u8], p: usize) -> Option<(String, usize)> {
    let mut i = skip_ws(m, p + 2);
    let name_start = i;
    while i < m.len() && is_ident(m[i]) {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name = String::from_utf8_lossy(&m[name_start..i]).to_string();
    i = skip_ws(m, i);
    if m.get(i) == Some(&b'<') {
        i = skip_ws(m, skip_angle(m, i));
    }
    if m.get(i) != Some(&b'(') {
        return None;
    }
    let mut depth = 0i32;
    let mut args = 0usize;
    let mut seg_has_text = false;
    let mut j = i;
    while j < m.len() {
        let c = m[j];
        match c {
            b'-' if m.get(j + 1) == Some(&b'>') => {
                j += 1;
            }
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b')' | b']' | b'}' | b'>' => {
                depth -= 1;
                if depth == 0 {
                    if seg_has_text {
                        args += 1;
                    }
                    break;
                }
            }
            b',' if depth == 1 => {
                if seg_has_text {
                    args += 1;
                }
                seg_has_text = false;
                j += 1;
                continue;
            }
            _ => {}
        }
        if depth >= 1 && !c.is_ascii_whitespace() && !(depth == 1 && c == b'(') {
            seg_has_text = true;
        }
        j += 1;
    }
    Some((name, args))
}

fn fns_over_7_params(inv: &Inventory) -> Vec<String> {
    let mut out = Vec::new();
    for f in prod_files(inv, "crates/") {
        let m = &f.lexed.masked;
        for p in find_words(m, "fn") {
            if !f.is_prod(p) {
                continue;
            }
            if let Some((name, n)) = fn_param_count(m, p) {
                if n > 7 {
                    out.push(format!("{} [{name}: {n} params]", f.site(p)));
                }
            }
        }
    }
    out
}

/// Whether an `#[ignore = ".."]` reason starts with an issue number.
fn reason_has_issue(reason: &str) -> bool {
    let r = reason.trim_start();
    r.starts_with('#') && r[1..].starts_with(|c: char| c.is_ascii_digit())
}

/// `#[ignore]` (or `cfg_attr(.., ignore)`) without `= "#NNN …"`, anywhere
/// under `crates/`: these only ever appear in test code.
fn ignore_without_issue(inv: &Inventory) -> Vec<String> {
    let mut out = Vec::new();
    for f in &inv.all {
        let m = &f.lexed.masked;
        for p in find_words(m, "ignore") {
            let mut b = p;
            while b > 0 && m[b - 1].is_ascii_whitespace() {
                b -= 1;
            }
            let prev = if b > 0 { m[b - 1] } else { b' ' };
            let in_attr =
                prev == b'[' || (prev == b',' && f.line_text(f.line_of(p)).contains("cfg_attr"));
            if !in_attr {
                continue;
            }
            let q = skip_ws(m, p + "ignore".len());
            let ok = m.get(q) == Some(&b'=')
                && f.lexed
                    .strings
                    .iter()
                    .find(|s| s.start > q)
                    .is_some_and(|s| reason_has_issue(&s.content));
            if !ok {
                out.push(f.site(p));
            }
        }
    }
    out
}

/// pytest skip markers in the binding's test suite.
fn pytest_skips(inv: &Inventory) -> Vec<String> {
    let mut out = Vec::new();
    let mut paths = Vec::new();
    py_files(&inv.root.join("crates/python/tests"), &mut paths);
    for p in paths {
        let text = fs::read_to_string(&p).unwrap();
        let rel = p
            .strip_prefix(&inv.root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        for (n, line) in text.lines().enumerate() {
            let code = line.split('#').next().unwrap_or("");
            for marker in [
                "pytest.mark.skipif",
                "pytest.mark.skip(",
                "pytest.skip(",
                "pytest.importorskip(",
            ] {
                if code.contains(marker) {
                    out.push(format!("{rel}:{}: {}", n + 1, line.trim()));
                }
            }
            if code.trim_end().ends_with("pytest.mark.skip") {
                out.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    out
}

fn py_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            py_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "py") {
            out.push(p);
        }
    }
}

/// The modules that own a disallowed method; their allows are the point.
const DISALLOWED_METHOD_OWNERS: &[&str] = &[
    "crates/core/src/atomic_output.rs",
    "crates/core/src/untrusted.rs",
];

fn disallowed_method_allows(inv: &Inventory) -> Vec<String> {
    prod_occurrences(inv, "crates/", &["clippy::disallowed_methods"])
        .into_iter()
        // `cfg_attr(test, allow(..))` exempts only the test build.
        .filter(|s| !s.contains("cfg_attr(test"))
        .filter(|s| !DISALLOWED_METHOD_OWNERS.iter().any(|o| s.starts_with(o)))
        .collect()
}

/// Entries of `deny.toml`'s `[bans] skip` list.
fn deny_skip_entries(root: &Path) -> Vec<String> {
    let text = fs::read_to_string(root.join("deny.toml")).unwrap();
    let mut out = Vec::new();
    let mut in_skip = false;
    for (n, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.starts_with("skip = [") {
            in_skip = true;
        } else if in_skip && t.starts_with(']') {
            in_skip = false;
        } else if in_skip && t.starts_with('{') {
            out.push(format!("deny.toml:{}: {t}", n + 1));
        }
    }
    out
}

fn cargo_deny_skips(inv: &Inventory) -> Vec<String> {
    deny_skip_entries(&inv.root)
}

const METRICS: &[(&str, Metric)] = &[
    ("parquet_open_sites", parquet_open_sites),
    ("geometry_literal_files", geometry_literal_files),
    ("static_once_warnings", static_once_warnings),
    ("global_atomic_counters", global_atomic_counters),
    ("cli_bail_calls", cli_bail_calls),
    ("too_many_arguments_allows", too_many_arguments_allows),
    ("fns_over_7_params", fns_over_7_params),
    ("ignore_without_issue", ignore_without_issue),
    ("pytest_skips", pytest_skips),
    ("disallowed_method_allows", disallowed_method_allows),
    ("cargo_deny_skips", cargo_deny_skips),
];

// ---------------------------------------------------------------------------
// ci/ratchets.toml: a flat subset of TOML (tables of `key = int | "str"`).
// ---------------------------------------------------------------------------

type Table = BTreeMap<String, String>;

fn parse_ratchets(text: &str) -> BTreeMap<String, Table> {
    let mut out: BTreeMap<String, Table> = BTreeMap::new();
    let mut cur: Option<String> = None;
    for (n, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(name) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let name = name.to_string();
            assert!(
                !out.contains_key(&name),
                "ci/ratchets.toml:{}: duplicate [{name}]",
                n + 1
            );
            out.insert(name.clone(), Table::new());
            cur = Some(name);
            continue;
        }
        let (k, v) = t
            .split_once('=')
            .unwrap_or_else(|| panic!("ci/ratchets.toml:{}: expected key = value", n + 1));
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(v);
        let table = cur
            .as_ref()
            .unwrap_or_else(|| panic!("ci/ratchets.toml:{}: key outside a table", n + 1));
        out.get_mut(table)
            .unwrap()
            .insert(k.trim().to_string(), v.to_string());
    }
    out
}

fn load_ratchets(root: &Path) -> BTreeMap<String, Table> {
    let path = root.join("ci/ratchets.toml");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (the structural ratchets, #779)", path.display()));
    parse_ratchets(&text)
}

fn inventory() -> Inventory {
    let root = repo_root();
    let src = src_files(&root);
    let all = all_crate_rs(&root);
    Inventory { root, src, all }
}

// ---------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------

/// Every ratchet's count equals its pin: above fails (a new bypass), below
/// fails too (lower the pin so the gain cannot be given back).
#[test]
fn ratchets_match_their_pins() {
    let root = repo_root();
    let ratchets = load_ratchets(&root);
    let inv = inventory();
    let mut failures = String::new();
    let mut table = String::new();
    for (name, metric) in METRICS {
        let Some(entry) = ratchets.get(*name) else {
            let _ = writeln!(failures, "[{name}] is missing from ci/ratchets.toml");
            continue;
        };
        let pin: usize = entry
            .get("pin")
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("[{name}] needs an integer `pin`"));
        let sites = metric(&inv);
        let count = sites.len();
        let _ = writeln!(
            table,
            "  {name:<28} {count:>4} (pin {pin}, target {})",
            entry.get("target").map_or("?", String::as_str)
        );
        if count > pin {
            let _ = writeln!(
                failures,
                "[{name}] is {count}, above its pin {pin}. Route the new code through \
                 the owner ({}) instead of adding a bypass. Guards: {}. Sites:\n    {}",
                entry.get("owner").map_or("?", String::as_str),
                entry.get("catches").map_or("", String::as_str),
                sites.join("\n    ")
            );
        } else if count < pin {
            let _ = writeln!(
                failures,
                "[{name}] is {count}, below its pin {pin}: lower the pin to {count} in \
                 ci/ratchets.toml so the gain is kept."
            );
        }
    }
    println!("structural ratchets (#779):\n{table}");
    assert!(failures.is_empty(), "\n{failures}");
}

/// Every table in ci/ratchets.toml is either computed here or names the
/// script that checks it, and records what drives it to its target.
#[test]
fn ratchet_entries_are_complete() {
    let ratchets = load_ratchets(&repo_root());
    let known: Vec<&str> = METRICS.iter().map(|(n, _)| *n).collect();
    for (name, entry) in &ratchets {
        if name == "scope" {
            continue;
        }
        for key in ["pin", "target", "owner", "catches"] {
            assert!(entry.contains_key(key), "[{name}] needs `{key}`");
        }
        let pin: usize = entry["pin"].parse().expect("integer pin");
        let target: usize = entry["target"].parse().expect("integer target");
        assert!(
            target <= pin,
            "[{name}] target {target} is above its pin {pin}"
        );
        match entry.get("checked_by") {
            Some(script) => assert!(
                repo_root().join(script).exists(),
                "[{name}] checked_by {script} does not exist"
            ),
            None => assert!(
                known.contains(&name.as_str()),
                "[{name}] is not a metric this test computes; add it to METRICS \
                 or name its `checked_by` script"
            ),
        }
    }
}

/// A production `allow(clippy::disallowed_methods)` says why, with an issue
/// number on the same line, so each exemption is tied to the work that
/// removes it.
#[test]
fn disallowed_method_allows_cite_an_issue() {
    let inv = inventory();
    let mut bad = Vec::new();
    for f in &inv.src {
        for (n, line) in f.raw.lines().enumerate() {
            if line.contains("allow(clippy::disallowed_methods)") {
                let start = f.line_starts[n];
                let comment = line.split_once("//").map_or("", |(_, c)| c);
                let cites = comment
                    .match_indices('#')
                    .any(|(i, _)| reason_has_issue(&comment[i..]));
                if f.is_prod(start + line.find('#').unwrap_or(0)) && !cites {
                    bad.push(format!("{}:{}: {}", f.rel, n + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "add `// #NNN <why>` on the same line:\n  {}",
        bad.join("\n  ")
    );
}

/// Each cargo-deny duplicate skip links an issue in its reason.
#[test]
fn deny_skips_cite_an_issue() {
    let bad: Vec<String> = deny_skip_entries(&repo_root())
        .into_iter()
        .filter(|e| {
            let reason = e.split("reason").nth(1).unwrap_or("");
            !reason
                .match_indices('#')
                .any(|(i, _)| reason_has_issue(&reason[i..]))
        })
        .collect();
    assert!(
        bad.is_empty(),
        "each skip needs reason = \"#NNN ...\":\n  {}",
        bad.join("\n  ")
    );
}

// --- The scanner's own tests: the rules above are only as good as these. ---

fn lexed_prod(src: &str) -> (Lexed, Vec<bool>) {
    let lexed = lex(src);
    let mut prod = vec![true; src.len()];
    for (s, e) in test_regions(&lexed.masked).0 {
        prod[s..e].iter_mut().for_each(|p| *p = false);
    }
    (lexed, prod)
}

#[test]
fn lexer_masks_comments_strings_and_chars() {
    let src = "let a = \"{ x }\"; // { comment\nlet b = '{'; let c: &'static str = r#\"}\"#; /* { /* } */ */";
    let l = lex(src);
    let m = String::from_utf8(l.masked).unwrap();
    assert!(!m.contains('{') && !m.contains('}'), "{m}");
    assert!(m.contains("'static"), "lifetimes stay code: {m}");
    let contents: Vec<&str> = l.strings.iter().map(|s| s.content.as_str()).collect();
    assert_eq!(contents, ["{ x }", "}"]);
}

#[test]
fn cfg_test_items_are_not_production() {
    let src = "fn a() { x(\"geometry\") }\n#[cfg(test)]\nmod tests {\n fn b() { \"}\" }\n}\n\
               #[cfg(all(test, feature = \"remote\"))]\n#[allow(dead_code)]\nfn c() {}\n\
               #[cfg(feature = \"testing\")]\nfn d() {}\n#[cfg(test)]\nmod testutil;\n";
    let (l, prod) = lexed_prod(src);
    let at = |needle: &str| prod[src.find(needle).unwrap()];
    assert!(at("fn a"));
    assert!(!at("fn b"));
    assert!(!at("fn c"));
    assert!(at("fn d"), "a feature gate is not a test gate");
    assert_eq!(test_regions(&l.masked).1, ["testutil"]);
}

#[test]
fn param_counter_handles_generics_closures_and_fn_pointers() {
    let count = |src: &str| {
        let l = lex(src);
        fn_param_count(&l.masked, find_words(&l.masked, "fn")[0]).map(|(_, n)| n)
    };
    assert_eq!(count("fn f() {}"), Some(0));
    assert_eq!(count("fn f(&self, a: u8) {}"), Some(2));
    assert_eq!(
        count("fn f<T: Fn(u8, u8) -> u8>(a: HashMap<K, V>, b: impl Fn(u8) -> u8, c: [u8; 2],) {}"),
        Some(3)
    );
    assert_eq!(count("type F = fn(u8, u8) -> u8;"), None);
}

#[test]
fn ignore_rule_requires_an_issue_number() {
    assert!(reason_has_issue("#563 needs 2 GiB"));
    assert!(!reason_has_issue("slow"));
    assert!(!reason_has_issue("# not a number"));
}
