//! Surface-parity gates for the CLI (#779, track G2).
//!
//! Compiled into the `tylertoo` binary's unit tests through a `#[path]`
//! module at the end of `src/main.rs`: the clap `*Args` structs and the
//! option builders are private to the binary crate, so an integration test
//! under `tests/` cannot reach them.
//!
//! The gates:
//!
//! - **Clap dump.** `parity/clap_surface.json` is the machine-readable list
//!   of every argument of `tiles`, `overview`, `export-pmtiles`, `pyramid`
//!   and `decode`. The Python parity test (`crates/python/tests/
//!   test_parity.py`) checks it against the Python signatures. A test here
//!   fails when the committed copy is stale. Regenerate it with
//!   `UPDATE_PARITY_SNAPSHOT=1 cargo test -p tylertoo --bin tylertoo surface_parity`.
//! - **(b) Defaults.** clap's defaults, run through the real builders, equal
//!   the core `Default` impls.
//! - **(c) Parsed but unused.** Changing any one flag changes the core
//!   options the builder produces (#374, #600).
//! - **(d) pyramid vs tiles.** Every flag on one and not the other is
//!   listed with a reason (#374, #443, #458).
//!
//! Every difference that is allowed, or known and not yet fixed, is an entry
//! in `tests/parity_exemptions.toml` with a reason and an issue number. An
//! entry with `xfail = true` is drift that F3 (#779) removes: the test fails
//! when the drift is gone and the entry is still there, the way a strict
//! pytest xfail does. The number of entries in each table is pinned below.

use super::*;
use std::collections::{BTreeMap, BTreeSet};

/// The subcommands with a product surface the Python API mirrors.
const DUMPED: [&str; 5] = ["tiles", "overview", "export-pmtiles", "pyramid", "decode"];

/// The subcommands gated for parsed-but-unused flags.
const BUILT: [&str; 4] = ["tiles", "overview", "export-pmtiles", "pyramid"];

/// Entries per exemption table and kind. A new entry, or one removed, needs
/// this pin changed in the same PR, so a reviewer sees every exemption.
const PINNED: &[(&str, &str, usize)] = &[
    ("parsed_unused", "exempt", 15),
    ("parsed_unused", "xfail", 0),
    ("defaults", "exempt", 0),
    ("defaults", "xfail", 0),
    ("pyramid_vs_tiles", "exempt", 59),
    ("pyramid_vs_tiles", "xfail", 0),
];

const SNAPSHOT: &str = "tests/parity/clap_surface.json";
const EXEMPTIONS: &str = "tests/parity_exemptions.toml";

// --- the clap surface --------------------------------------------------------

/// A built subcommand, so actions carry their implicit defaults.
fn subcommand(name: &str) -> clap::Command {
    let mut cli = Cli::command();
    cli.build();
    cli.find_subcommand(name)
        .unwrap_or_else(|| panic!("no subcommand {name}"))
        .clone()
}

/// The arguments of a subcommand, without clap's own and the global ones.
fn own_args(cmd: &clap::Command) -> Vec<clap::Arg> {
    cmd.get_arguments()
        .filter(|a| !matches!(a.get_id().as_str(), "help" | "version" | "quiet"))
        .cloned()
        .collect()
}

fn kind(arg: &clap::Arg) -> &'static str {
    match arg.get_action() {
        clap::ArgAction::SetTrue => "flag",
        clap::ArgAction::SetFalse => "negated-flag",
        clap::ArgAction::Append => "append",
        clap::ArgAction::Set => "value",
        clap::ArgAction::Count => "count",
        other => panic!("unexpected clap action {other:?} on {}", arg.get_id()),
    }
}

/// The long spelling, or the id of a positional.
fn arg_name(arg: &clap::Arg) -> String {
    arg.get_long()
        .map_or_else(|| arg.get_id().to_string(), str::to_string)
}

fn arg_json(arg: &clap::Arg) -> serde_json::Value {
    let flag = matches!(kind(arg), "flag" | "negated-flag");
    serde_json::json!({
        "id": arg.get_id().as_str(),
        "long": arg.get_long(),
        "aliases": arg.get_visible_aliases().unwrap_or_default(),
        "positional": arg.is_positional(),
        "hidden": arg.is_hide_set(),
        "kind": kind(arg),
        "default": arg
            .get_default_values()
            .first()
            .map(|v| v.to_string_lossy().into_owned()),
        "possible_values": if flag {
            Vec::new()
        } else {
            arg.get_possible_values()
                .iter()
                .map(|p| p.get_name().to_string())
                .collect()
        },
    })
}

/// The machine-readable clap dump the Python parity test reads.
fn clap_surface() -> String {
    let mut root = serde_json::Map::new();
    for name in DUMPED {
        let args: Vec<_> = own_args(&subcommand(name)).iter().map(arg_json).collect();
        root.insert(name.to_string(), serde_json::Value::Array(args));
    }
    let mut text = serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .expect("the dump serializes");
    text.push('\n');
    text
}

/// The committed dump is current. #779: the Python parity test reads this
/// file, so a flag added without regenerating it would slip past that test.
#[test]
fn clap_surface_snapshot_is_current() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(SNAPSHOT);
    let live = clap_surface();
    if std::env::var_os("UPDATE_PARITY_SNAPSHOT").is_some() {
        std::fs::write(&path, &live).expect("write the clap dump");
        return;
    }
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert!(
        committed == live,
        "{SNAPSHOT} is stale: the CLI's arguments changed. Regenerate it with \
         `UPDATE_PARITY_SNAPSHOT=1 cargo test -p tylertoo --bin tylertoo surface_parity`, \
         then run `uv run pytest tests/test_parity.py` in crates/python"
    );
}

// --- the exemption ledger ----------------------------------------------------

#[derive(Debug, Clone)]
struct Entry {
    table: String,
    command: String,
    name: String,
    reason: String,
    issue: i64,
    xfail: bool,
}

impl Entry {
    fn kind(&self) -> &'static str {
        if self.xfail {
            "xfail"
        } else {
            "exempt"
        }
    }

    fn cite(&self) -> String {
        format!(
            "[[{}]] {} {} (#{}, {})",
            self.table,
            self.command,
            self.name,
            self.issue,
            self.kind()
        )
    }
}

/// Every entry of `tests/parity_exemptions.toml`.
fn ledger() -> Vec<Entry> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(EXEMPTIONS);
    let text = std::fs::read_to_string(&path).expect("read the exemption ledger");
    let doc: toml_edit::DocumentMut = text.parse().expect("the exemption ledger is TOML");
    let mut entries = Vec::new();
    for (table, item) in doc.as_table() {
        let rows = item
            .as_array_of_tables()
            .unwrap_or_else(|| panic!("[{table}] must be an array of tables ([[{table}]])"));
        for row in rows {
            let text = |key: &str| row.get(key).and_then(|v| v.as_str()).map(str::to_string);
            entries.push(Entry {
                table: table.to_string(),
                command: text("command").unwrap_or_else(|| "pyramid".to_string()),
                name: text("flag")
                    .or_else(|| text("field"))
                    .unwrap_or_else(|| panic!("[[{table}]] entry without flag or field")),
                reason: text("reason").unwrap_or_default(),
                issue: row
                    .get("issue")
                    .and_then(toml_edit::Item::as_integer)
                    .unwrap_or(0),
                xfail: row
                    .get("xfail")
                    .and_then(toml_edit::Item::as_bool)
                    .unwrap_or(false),
            });
        }
    }
    entries
}

fn entries_in(table: &str) -> Vec<Entry> {
    ledger().into_iter().filter(|e| e.table == table).collect()
}

fn lookup<'a>(entries: &'a [Entry], command: &str, name: &str) -> Option<&'a Entry> {
    entries
        .iter()
        .find(|e| e.command == command && e.name == name)
}

/// Each entry has a reason and an issue, none repeats, and the counts match
/// the pins: an exemption cannot be added, or quietly left behind, without
/// a reviewer seeing the pin change.
#[test]
fn exemption_ledger_is_well_formed_and_pinned() {
    let entries = ledger();
    let mut seen = BTreeSet::new();
    for e in &entries {
        assert!(!e.reason.trim().is_empty(), "{} has no reason", e.cite());
        assert!(e.issue > 0, "{} has no issue number", e.cite());
        assert!(
            seen.insert((e.table.clone(), e.command.clone(), e.name.clone())),
            "{} is listed twice",
            e.cite()
        );
    }
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for e in &entries {
        *counts
            .entry((e.table.clone(), e.kind().to_string()))
            .or_default() += 1;
    }
    for &(table, kind, pinned) in PINNED {
        let n = counts
            .remove(&(table.to_string(), kind.to_string()))
            .unwrap_or(0);
        assert_eq!(
            n, pinned,
            "[[{table}]] has {n} {kind} entries but the pin says {pinned}: update PINNED \
             in tests/parity/in_crate.rs with the ledger"
        );
    }
    assert!(
        counts.is_empty(),
        "tables or kinds missing from PINNED: {counts:?}"
    );
}

// --- the builders ------------------------------------------------------------

/// Positional arguments that make a bare invocation of each subcommand parse.
fn positionals(command: &str) -> &'static [&'static str] {
    match command {
        "tiles" => &["in.parquet", "out.pmtiles"],
        "overview" => &["in.parquet", "out.parquet"],
        "export-pmtiles" | "decode" => &["in.parquet", "out.pmtiles"],
        "pyramid" => &["out.pmtiles", "--band", "0-5:coarse.parquet"],
        other => panic!("no positionals for {other}"),
    }
}

fn parse(command: &str, flags: &[String]) -> Result<Command> {
    let mut argv: Vec<String> = vec!["tylertoo".into(), command.into()];
    argv.extend(positionals(command).iter().map(|s| (*s).to_string()));
    argv.extend(flags.iter().cloned());
    Ok(Cli::try_parse_from(argv)?.command)
}

/// The core options a subcommand's builder makes from `flags`, as `{:#?}`
/// text, or `None` where no builder can be called from a test.
///
/// Each command has one builder since #779 F3, and each builder is pure
/// translation from the clap structs to the core options:
///
/// - `tiles`: [`tiles_options`] (the whole [`TilesOptions`]; the shard job
///   and `--max-zoom auto` read the input, so `resolve_tiles_job` adds them
///   at run time).
/// - `overview`: [`overview_options`].
/// - `export-pmtiles`: [`export_options`].
/// - `pyramid`: [`pyramid_options`].
fn build(command: &str, flags: &[String]) -> Option<Result<String>> {
    let parsed = match parse(command, flags) {
        Ok(c) => c,
        Err(e) => return Some(Err(e)),
    };
    let built = match parsed {
        Command::Tiles(args) => tiles_options(&args).map(|o| format!("{o:#?}")),
        Command::Overview(args) => overview_options(&args).map(|o| format!("{o:#?}")),
        Command::ExportPmtiles(args) => export_options(&args).map(|o| format!("{o:#?}")),
        Command::Pyramid(args) => Ok(format!("{:#?}", pyramid_options(&args))),
        _ => return None,
    };
    Some(built)
}

// --- (b) defaults ------------------------------------------------------------

/// `{:#?}` text flattened to `path = value` lines, so two option values
/// compare field by field and a difference names its field path.
fn debug_leaves(text: &str) -> BTreeMap<String, usize> {
    let mut stack: Vec<String> = Vec::new();
    let mut leaves = BTreeMap::new();
    for line in text.lines() {
        let body = line.trim_start();
        let depth = (line.len() - body.len()) / 4;
        stack.truncate(depth);
        let field = body.split_once(": ").filter(|(name, _)| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        });
        let value = match field {
            Some((name, value)) => {
                stack.resize(depth, String::new());
                stack.push(name.to_string());
                value
            }
            None => body,
        };
        let path: Vec<&str> = stack
            .iter()
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .collect();
        *leaves
            .entry(format!("{} = {value}", path.join(".")))
            .or_default() += 1;
    }
    leaves
}

/// The field paths whose values differ between two `{:#?}` texts.
fn differing_fields(built: &str, core: &str) -> BTreeSet<String> {
    let (a, b) = (debug_leaves(built), debug_leaves(core));
    a.keys()
        .chain(b.keys())
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| k.split(" = ").next().unwrap_or_default().to_string())
        .collect()
}

/// One clap default checked by hand against its core default, for a flag
/// no builder reaches yet: (flag, clap's value, core's value), both as
/// `{:?}` text.
type HandCheck = (&'static str, String, String);

/// Since #779 F3 every command's builder reaches every flag, so nothing is
/// checked by hand. A flag mapped outside its builder again needs a check
/// here.
fn hand_checks(_command: &str) -> Vec<HandCheck> {
    Vec::new()
}

/// The core value each builder's bare-invocation output is compared to.
fn core_default(command: &str) -> Option<String> {
    use tylertoo_core::overview::convert::ConvertOptions;
    match command {
        "tiles" => Some(format!("{:#?}", TilesOptions::default())),
        "overview" => Some(format!("{:#?}", ConvertOptions::default())),
        "export-pmtiles" => Some(format!("{:#?}", ExportOptions::default())),
        "pyramid" => Some(format!(
            "{:#?}",
            tylertoo_core::pyramid::PyramidOptions::default()
        )),
        _ => None,
    }
}

/// (b) Each command's clap defaults, run through its builder, equal the
/// core `Default` impls; flags no builder reaches are compared by hand.
///
/// Would have caught the hand-copied defaults #699 fixed (`tiles` and
/// Python each carried their own copy of the size cap), and the `pyramid`
/// size-cap default that #761 had to change on three surfaces.
#[test]
fn clap_defaults_match_the_core_defaults() {
    let exempt = entries_in("defaults");
    let unbuilt = entries_in("parsed_unused");
    let mut failures = Vec::new();
    let mut used = BTreeSet::new();
    for command in BUILT {
        if let (Some(built), Some(core)) = (build(command, &[]), core_default(command)) {
            let built = built.unwrap_or_else(|e| panic!("bare `{command}` fails to build: {e}"));
            for field in differing_fields(&built, &core) {
                let hit = exempt.iter().find(|e| {
                    e.command == command
                        && (field == e.name || field.starts_with(&format!("{}.", e.name)))
                });
                match hit {
                    Some(e) => {
                        used.insert(e.cite());
                    }
                    None => failures.push(format!(
                        "`{command}` default differs from core at `{field}`"
                    )),
                }
            }
        }
        let checks = hand_checks(command);
        for (flag, clap, core) in &checks {
            if clap != core {
                match lookup(&exempt, command, flag) {
                    Some(e) => {
                        used.insert(e.cite());
                    }
                    None => failures.push(format!(
                        "`{command} --{flag}` defaults to {clap}, core to {core}"
                    )),
                }
            }
        }
        // Every flag no builder reaches, with a clap default, is checked by
        // hand: otherwise its default is compared to nothing.
        let checked: BTreeSet<&str> = checks.iter().map(|(f, ..)| *f).collect();
        for arg in own_args(&subcommand(command)) {
            let name = arg_name(&arg);
            let unreached = lookup(&unbuilt, command, &name).is_some_and(|e| e.xfail);
            if unreached
                && kind(&arg) == "value"
                && !arg.get_default_values().is_empty()
                && !checked.contains(name.as_str())
            {
                failures.push(format!(
                    "`{command} --{name}` has a default no builder reaches and no hand \
                     check: add it to hand_checks"
                ));
            }
        }
    }
    for e in &exempt {
        if !used.contains(&e.cite()) {
            failures.push(format!("stale: {} no longer differs; remove it", e.cite()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// --- (c) parsed but unused ---------------------------------------------------

/// A valid value for a flag that has no default, or whose default is not a
/// number one can step from.
fn sample_value(flag: &str) -> Option<&'static str> {
    Some(match flag {
        "sort-key" | "magnitude-ladder" | "include-property" | "exclude-property" => "pop",
        "entry-zoom" => "density:5000=4",
        "class-rank" => "class:a=2",
        "filter" => "pop > 1",
        "simplify-factor" => "0.5",
        "point-thinning" | "line-thinning" | "polygon-thinning" | "line-visibility"
        | "polygon-visibility" => "3.5",
        "representation" => "0-2:point",
        "spill-dir" | "work-dir" => "scratch",
        "save-plan" | "plan" => "p.plan",
        "gsd" => "1000,500",
        "bbox" => "0,0,1,1",
        "layer-name" | "name" | "description" | "attribution" => "x",
        "max-tile-size" | "tile-size-limit" => "1M",
        "max-compressed-tile-size" => "100K",
        "feature-id" => "id",
        "keep-overview" => "o.parquet",
        "extent" => "8192",
        "tile-range" => "21..40",
        "min-zoom" => "2",
        "zoom-ceiling" => "3",
        "in-flight-batches" | "partition-wave" => "4",
        "read-workers" => "2",
        "size-valve" => "drop",
        "feature-order" => "pop:desc",
        _ => return None,
    })
}

/// Arguments appended to a bare invocation for the baseline and for the run
/// with one flag changed, or `None` when no valid change is known.
fn variant(arg: &clap::Arg) -> Option<(Vec<String>, Vec<String>)> {
    let flag = arg.get_long()?;
    let long = format!("--{flag}");
    let owned = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    // Flags that only parse alongside another one.
    match flag {
        "ladder-step" => {
            let base = ["--magnitude-ladder", "pop"];
            return Some((owned(&base), owned(&[&base[..], &[&long, "2"]].concat())));
        }
        "accumulate-attribute" => {
            return Some((
                owned(&["--cluster"]),
                owned(&["--cluster", &long, "pop:sum"]),
            ))
        }
        _ => {}
    }
    if kind(arg) == "flag" {
        return Some((Vec::new(), vec![long]));
    }
    let default = arg
        .get_default_values()
        .first()
        .map(|v| v.to_string_lossy().into_owned());
    let value = if let Some(v) = sample_value(flag) {
        v.to_string()
    } else if let Some(p) = arg
        .get_possible_values()
        .iter()
        .map(|p| p.get_name().to_string())
        .find(|p| Some(p) != default.as_ref())
    {
        p
    } else {
        // A numeric default: step it by one.
        let n: f64 = default.as_deref()?.parse().ok()?;
        let stepped = n + 1.0;
        if default.as_deref()?.contains('.') {
            format!("{stepped:.1}")
        } else {
            format!("{stepped}")
        }
    };
    Some((Vec::new(), vec![long, value]))
}

/// (c) Every flag of `tiles`, `overview`, `export-pmtiles` and `pyramid`
/// changes the core options its builder makes, unless the ledger lists it:
/// as presentation only (`--force`, `--report`, logging, input selection),
/// or as `xfail` where the flag is mapped outside any callable builder.
///
/// Would have caught #374 (`pyramid --feature-order` parsed nowhere: the
/// pyramid built `ExportOptions::default()`) and the #600 class of a flag
/// accepted and then ignored.
#[test]
fn every_flag_reaches_the_core_options() {
    let ledger = entries_in("parsed_unused");
    let mut failures = Vec::new();
    let mut seen = BTreeSet::new();
    for command in BUILT {
        for arg in own_args(&subcommand(command)) {
            if arg.is_positional() || arg.is_hide_set() {
                continue;
            }
            let name = arg_name(&arg);
            let entry = lookup(&ledger, command, &name);
            if let Some(e) = entry {
                seen.insert(e.cite());
                if !e.xfail {
                    continue;
                }
            }
            let Some((base, changed)) = variant(&arg) else {
                failures.push(format!(
                    "`{command} --{name}`: no sample value; add one to sample_value"
                ));
                continue;
            };
            let (Some(a), Some(b)) = (build(command, &base), build(command, &changed)) else {
                if entry.is_none() {
                    failures.push(format!(
                        "`{command} --{name}` reaches no callable builder: thread it through \
                         one, or list it in [[parsed_unused]]"
                    ));
                }
                continue;
            };
            let (a, b) = match (a, b) {
                (Ok(a), Ok(b)) => (a, b),
                (a, b) => {
                    failures.push(format!(
                        "`{command}` {changed:?} does not build (fix the sample): {:?} / {:?}",
                        a.err(),
                        b.err()
                    ));
                    continue;
                }
            };
            match (a == b, entry) {
                (true, None) => failures.push(format!(
                    "`{command} --{name}` is parsed but changes no core option (#374, #600)"
                )),
                (false, Some(e)) => failures.push(format!(
                    "stale: `{command} --{name}` now reaches the builder; remove {}",
                    e.cite()
                )),
                _ => {}
            }
        }
    }
    for e in &ledger {
        if !seen.contains(&e.cite()) {
            failures.push(format!("stale: {} names no such flag", e.cite()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// --- (d) pyramid vs tiles ----------------------------------------------------

fn visible_longs(command: &str) -> BTreeSet<String> {
    own_args(&subcommand(command))
        .iter()
        .filter(|a| !a.is_hide_set())
        .filter_map(|a| a.get_long().map(str::to_string))
        .collect()
}

/// (d) A flag on `tiles` and not on `pyramid`, or the reverse, is listed
/// with its reason. The `xfail` entries are export knobs `pyramid` should
/// take and does not yet.
///
/// Would have caught #374 (`pyramid` had no `--feature-order`) and #443's
/// pyramid half (`pyramid` has no `--feature-id`); #458 asks for exactly
/// this test.
#[test]
fn pyramid_has_parity_with_tiles_flags() {
    let ledger = entries_in("pyramid_vs_tiles");
    let (tiles, pyramid) = (visible_longs("tiles"), visible_longs("pyramid"));
    let mut failures = Vec::new();
    let one_sided: BTreeSet<&String> = tiles.symmetric_difference(&pyramid).collect();
    for flag in &one_sided {
        if lookup(&ledger, "pyramid", flag).is_none() {
            let side = if tiles.contains(*flag) {
                "on tiles, not pyramid"
            } else {
                "on pyramid, not tiles"
            };
            failures.push(format!(
                "--{flag} is {side}: add it, or list it in [[pyramid_vs_tiles]]"
            ));
        }
    }
    for e in &ledger {
        if !one_sided.contains(&e.name) {
            failures.push(format!(
                "stale: --{} is on both commands, or on neither; remove {}",
                e.name,
                e.cite()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The flattener the default check relies on names nested fields.
#[test]
fn debug_leaves_names_nested_fields() {
    let core = "Opts {\n    a: 1,\n    inner: Inner {\n        b: 2,\n    },\n}";
    let built = "Opts {\n    a: 1,\n    inner: Inner {\n        b: 3,\n    },\n}";
    assert_eq!(
        differing_fields(built, core),
        BTreeSet::from(["inner.b".to_string()])
    );
}
