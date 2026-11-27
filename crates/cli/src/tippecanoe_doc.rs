//! Generated flag mappings and preset differences for tippecanoe.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use clap::CommandFactory;
use serde_json::Value;
use tylertoo_core::internal::overview::{
    config::{keywords, Preset, TIPPECANOE_SOURCES, TIPPECANOE_VERSION},
    convert::ConvertOptions,
    export::ExportOptions,
};

use super::Cli;

const MAX_NOTE_LEN: usize = 90;

// Long spellings are in main.cpp's long_options_orig and in --help.
// main.cpp:3382-3396 accepts "g" as the maximum-zoom argument.
// Omit --no-tiny-polygon-reduction: --no-collapse drops tiny polygons,
// whereas tippecanoe skips their reduction (tile.cpp:2159-2170).
const TIPPECANOE_FLAGS: &[FlagRow] = &[
    FlagRow {
        tippecanoe: &["--maximum-zoom"],
        command: "tiles",
        tylertoo: &["--max-zoom"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--minimum-zoom"],
        command: "tiles",
        tylertoo: &["--min-zoom"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--maximum-zoom=g"],
        command: "tiles",
        tylertoo: &["--max-zoom auto"],
        note: "Each tool estimates the maximum zoom from the input.",
    },
    FlagRow {
        tippecanoe: &["--layer"],
        command: "tiles",
        tylertoo: &["--layer-name"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--named-layer"],
        command: "pyramid",
        tylertoo: &["--band"],
        note: "Use GeoParquet bands with explicit zoom ranges.",
    },
    FlagRow {
        tippecanoe: &["--name"],
        command: "tiles",
        tylertoo: &["--name"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--description"],
        command: "tiles",
        tylertoo: &["--description"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--attribution"],
        command: "tiles",
        tylertoo: &["--attribution"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--buffer"],
        command: "tiles",
        tylertoo: &["--tile-buffer"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--include"],
        command: "tiles",
        tylertoo: &["--include-property"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--exclude"],
        command: "tiles",
        tylertoo: &["--exclude-property"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--exclude-all"],
        command: "tiles",
        tylertoo: &["--exclude-all-properties"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--drop-rate"],
        command: "tiles",
        tylertoo: &["--drop-rate"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--gamma"],
        command: "tiles",
        tylertoo: &["--drop-gamma"],
        note: "Both protect sparse areas, but the scales differ.",
    },
    FlagRow {
        tippecanoe: &["--drop-lines"],
        command: "tiles",
        tylertoo: &["--drop-lines"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--drop-polygons"],
        command: "tiles",
        tylertoo: &["--drop-polygons"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--no-simplification-of-shared-nodes"],
        command: "tiles",
        tylertoo: &["--no-simplification-of-shared-nodes"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--simplification"],
        command: "tiles",
        tylertoo: &["--simplify-factor"],
        note: "Tile units in tippecanoe; ground sample distance in tylertoo.",
    },
    FlagRow {
        tippecanoe: &["--maximum-tile-bytes"],
        command: "tiles",
        tylertoo: &["--max-compressed-tile-size"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["--coalesce-densest-as-needed"],
        command: "tiles",
        tylertoo: &["--size-valve merge"],
        note: "Similar tile size control; the merge algorithms differ.",
    },
    FlagRow {
        tippecanoe: &["--drop-densest-as-needed", "--drop-fraction-as-needed"],
        command: "tiles",
        tylertoo: &["--size-valve drop"],
        note: "Similar tile size control; the feature selection algorithms differ.",
    },
    FlagRow {
        tippecanoe: &["--cluster-distance"],
        command: "tiles",
        tylertoo: &["--cluster"],
        note: "A pixel radius in tippecanoe; thinning cells in tylertoo.",
    },
    FlagRow {
        tippecanoe: &["--coalesce"],
        command: "tiles",
        tylertoo: &["--no-coalesce-lines"],
        note: "Line coalescing is on by default; this tylertoo switch turns it off.",
    },
    FlagRow {
        tippecanoe: &["--use-attribute-for-id"],
        command: "tiles",
        tylertoo: &["--feature-id"],
        note: "",
    },
    FlagRow {
        tippecanoe: &["tile-join"],
        command: "merge",
        tylertoo: &[],
        note: "Use merge for archives with disjoint tiles.",
    },
    FlagRow {
        tippecanoe: &["tile-join"],
        command: "pyramid",
        tylertoo: &[],
        note: "Use pyramid for zoom bands.",
    },
    FlagRow {
        tippecanoe: &["tippecanoe-decode"],
        command: "decode",
        tylertoo: &[],
        note: "Writes GeoParquet.",
    },
];

struct FlagRow {
    tippecanoe: &'static [&'static str],
    command: &'static str,
    tylertoo: &'static [&'static str],
    note: &'static str,
}

fn resolve_flag(command: &clap::Command, flag: &str) {
    let name = flag
        .split_whitespace()
        .next()
        .unwrap()
        .strip_prefix("--")
        .unwrap();
    assert!(
        command.get_arguments().any(|arg| {
            arg.get_long() == Some(name)
                || arg
                    .get_all_aliases()
                    .is_some_and(|aliases| aliases.contains(&name))
        }),
        "unresolved flag {flag} on {}",
        command.get_name()
    );
}

fn validate_row(command: &clap::Command, row: &FlagRow) {
    let command = command
        .find_subcommand(row.command)
        .expect("documented command exists");
    for flag in row.tylertoo {
        resolve_flag(command, flag);
    }
    assert!(
        row.note.chars().count() <= MAX_NOTE_LEN,
        "flag note is too long"
    );
    assert!(
        !row.note.chars().any(|c| c.is_numeric()),
        "flag note contains a number"
    );
    assert!(
        row.tippecanoe.iter().any(|token| {
            token.starts_with('-') && token.chars().any(char::is_alphabetic)
                || matches!(*token, "tippecanoe" | "tile-join" | "tippecanoe-decode")
        }),
        "tippecanoe row needs an option or program"
    );
}

fn check_sources(changed: &[&str], sources: &[(&str, &str, &str)]) {
    let keys: BTreeSet<_> = sources.iter().map(|(key, _, _)| *key).collect();
    assert_eq!(keys.len(), sources.len(), "duplicate preset source");
    assert_eq!(
        changed.iter().copied().collect::<BTreeSet<_>>(),
        keys,
        "preset sources must match changed defaults"
    );
}

fn display_value(key: &str, value: &Value) -> String {
    match value {
        Value::Null => if key.contains("size") {
            "no cap"
        } else {
            "unset"
        }
        .to_owned(),
        Value::Number(number) if number.is_u64() || number.is_i64() => {
            let digits = number.to_string();
            let mut grouped = String::new();
            for (index, ch) in digits.chars().enumerate() {
                if index > 0
                    && (digits.len() - index).is_multiple_of(3)
                    && ch != '-'
                    && !grouped.ends_with('-')
                {
                    grouped.push(',');
                }
                grouped.push(ch);
            }
            format!("`{grouped}`")
        }
        Value::String(text) => format!("`{text}`"),
        value => format!("`{value}`"),
    }
}

pub(super) fn render() -> String {
    let command = Cli::command();
    let mut output = String::from(
        "<!-- Generated file. Do not edit by hand.\n\
         Regenerate: cargo run -p tylertoo --features gen-docs -- gen-reference-docs tippecanoe > crates/cli/docs/tippecanoe-tables.md\n\
         -->\n\n<!-- --8<-- [start:flags] -->\n",
    );
    writeln!(
        output,
        "| tippecanoe {TIPPECANOE_VERSION} | tylertoo | Notes |\n| --- | --- | --- |"
    )
    .unwrap();
    for row in TIPPECANOE_FLAGS {
        validate_row(&command, row);
        let foreign = row
            .tippecanoe
            .iter()
            .map(|token| format!("`{token}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let local = if row.tylertoo.is_empty() {
            format!("`{}`", row.command)
        } else {
            row.tylertoo
                .iter()
                .map(|flag| format!("`{} {flag}`", row.command))
                .collect::<Vec<_>>()
                .join(", ")
        };
        writeln!(output, "| {foreign} | {local} | {} |", row.note).unwrap();
    }
    output.push_str("<!-- --8<-- [end:flags] -->\n\n<!-- --8<-- [start:preset] -->\n");
    writeln!(output, "Preset defaults compared with tippecanoe {TIPPECANOE_VERSION}.\n\n| Flag | tylertoo default | With `--preset tippecanoe` | tippecanoe source |\n| --- | --- | --- | --- |").unwrap();
    let options = [
        (
            keywords::defaults(ConvertOptions::default()),
            keywords::defaults(Preset::convert(
                Some(Preset::Tippecanoe),
                ConvertOptions::default(),
            )),
        ),
        (
            keywords::defaults(ExportOptions::default()),
            keywords::defaults(Preset::export(
                Some(Preset::Tippecanoe),
                ExportOptions::default(),
            )),
        ),
    ];
    let changed: BTreeMap<_, _> = options
        .iter()
        .flat_map(|(base, preset)| {
            preset
                .iter()
                .filter(|(key, value)| base[*key] != **value)
                .map(|(key, value)| (key.as_str(), (&base[key], value)))
        })
        .collect();
    check_sources(
        &changed.keys().copied().collect::<Vec<_>>(),
        TIPPECANOE_SOURCES,
    );
    let tiles = command.find_subcommand("tiles").unwrap();
    resolve_flag(tiles, "--preset");
    for (key, flag, source) in TIPPECANOE_SOURCES {
        resolve_flag(tiles, flag);
        let (default, preset) = changed[key];
        writeln!(
            output,
            "| `{flag}` | {} | {} | `{source}` |",
            display_value(key, default),
            display_value(key, preset)
        )
        .unwrap();
    }
    output.push_str("<!-- --8<-- [end:preset] -->\n");
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tippecanoe_rejects_invalid_flag_rows() {
        let command = crate::Cli::command();
        for row in [
            FlagRow { tippecanoe: &["--buffer"], command: "tiles", tylertoo: &["--missing"], note: "" },
            FlagRow { tippecanoe: &["--buffer"], command: "decode", tylertoo: &["--tile-buffer"], note: "" },
            FlagRow { tippecanoe: &["--buffer"], command: "missing", tylertoo: &[], note: "" },
            FlagRow { tippecanoe: &["concept"], command: "tiles", tylertoo: &[], note: "" },
            FlagRow { tippecanoe: &["--buffer"], command: "tiles", tylertoo: &[], note: "default 5" },
            FlagRow { tippecanoe: &["--buffer"], command: "tiles", tylertoo: &[], note: "This note is intentionally too long to fit the allowed length for a flag mapping table entry." },
        ] {
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| validate_row(&command, &row))).is_err());
        }
    }

    #[test]
    fn tippecanoe_rejects_missing_stale_or_duplicate_sources() {
        for (keys, sources) in [
            (vec!["a"], vec![]),
            (vec![], vec![("a", "--a", "main.cpp")]),
            (
                vec!["a"],
                vec![("a", "--a", "main.cpp"), ("a", "--a", "main.cpp")],
            ),
        ] {
            assert!(std::panic::catch_unwind(|| check_sources(&keys, &sources)).is_err());
        }
    }

    #[test]
    fn tippecanoe_tables_render() {
        use clap::Parser;
        assert!(matches!(
            Cli::try_parse_from(["tylertoo", "gen-reference-docs", "tippecanoe"])
                .unwrap()
                .command,
            super::super::Command::GenReferenceDocs {
                mode: Some(super::super::ReferenceDocsMode::Tippecanoe)
            }
        ));
        let output = render();
        for section in ["flags", "preset"] {
            assert!(output.contains(&format!("<!-- --8<-- [start:{section}] -->")));
            assert!(output.contains(&format!("<!-- --8<-- [end:{section}] -->")));
        }
        assert!(output.contains("`--maximum-zoom=g`"));
        assert!(output.contains("`tile-join`"));
        assert!(output.contains("`tippecanoe-decode`"));
        assert!(output.contains(&format!("| tippecanoe {TIPPECANOE_VERSION} |")));
        assert!(output.contains(&format!(
            "Preset defaults compared with tippecanoe {TIPPECANOE_VERSION}."
        )));
        let preset = output
            .split("<!-- --8<-- [start:preset] -->")
            .nth(1)
            .unwrap();
        assert!(!preset.contains("| `--drop-rate` |"));
        assert!(!preset.contains("| `--tile-buffer` |"));
        let tag = include_str!("../../../benchmarks/e2e/setup_tippecanoe.sh")
            .lines()
            .find_map(|line| line.strip_prefix("DEFAULT_TAG=\""))
            .unwrap()
            .trim_end_matches('"');
        assert_eq!(TIPPECANOE_VERSION, format!("v{tag}"));
    }

    #[test]
    fn tippecanoe_preset_numbers_round_trip() {
        let output = render();
        let defaults = [
            keywords::defaults(ConvertOptions::default()),
            keywords::defaults(ExportOptions::default()),
        ];
        let presets = [
            keywords::defaults(Preset::convert(
                Some(Preset::Tippecanoe),
                ConvertOptions::default(),
            )),
            keywords::defaults(Preset::export(
                Some(Preset::Tippecanoe),
                ExportOptions::default(),
            )),
        ];
        for (base, preset) in defaults.iter().zip(&presets) {
            for (key, value) in preset.iter().filter(|(key, value)| base[*key] != **value) {
                let (_, flag, _) = TIPPECANOE_SOURCES
                    .iter()
                    .find(|(source_key, _, _)| *source_key == key)
                    .unwrap();
                let row = output
                    .lines()
                    .find(|line| line.starts_with(&format!("| `{flag}` |")))
                    .unwrap_or_else(|| panic!("missing preset row for {key}"));
                let cells: Vec<_> = row.split('|').map(str::trim).collect();
                for (cell, expected) in [(cells[2], &base[key]), (cells[3], value)] {
                    if let Some(expected) = expected.as_f64() {
                        let parsed: f64 = cell.trim_matches('`').replace(',', "").parse().unwrap();
                        assert_eq!(parsed, expected, "{key}");
                        if expected >= 1_000.0 {
                            assert!(cell.contains(','));
                        }
                    } else if expected.is_null() {
                        assert_eq!(
                            cell,
                            if key == "tile_size_limit" {
                                "no cap"
                            } else {
                                "unset"
                            }
                        );
                    } else {
                        assert_eq!(cell.trim_matches('`'), expected.as_str().unwrap());
                    }
                }
            }
        }
    }

    #[test]
    fn tippecanoe_generated_reference_matches_committed_documentation() {
        assert_eq!(render(), include_str!("../docs/tippecanoe-tables.md"));
        assert!(!render().is_empty());
    }
}
