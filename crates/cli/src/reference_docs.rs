//! Layout for the generated CLI reference. Syntax and help come from clap.

use clap::{Args, CommandFactory};
use std::fmt::Write;

use super::Cli;
use tylertoo_core::internal::overview::config::keywords;
type ConvertTuningArgs = tylertoo_core::internal::overview::config::cli::ConvertArgs<false>;

/// Honor clap's inverted switches in clap-markdown 0.1.5.
pub(super) fn command_for_markdown(command: clap::Command) -> clap::Command {
    command
        .mut_args(|arg| {
            if matches!(arg.get_action(), clap::ArgAction::SetFalse) {
                arg.action(clap::ArgAction::SetTrue)
                    .default_value(None::<&str>)
            } else {
                arg
            }
        })
        .mut_subcommands(command_for_markdown)
}

/// Group options under their CLI help headings and print shared tuning once.
pub(super) fn format(markdown: String) -> String {
    let command = Cli::command();
    let tuning = ConvertTuningArgs::augment_args(clap::Command::new("tuning"));
    let shared_ids: Vec<_> = tuning.get_arguments().map(clap::Arg::get_id).collect();
    let mut output = String::from(
        "# CLI reference\n\nIn option tables, the Python column names the keyword \
         that sets the same option on the matching Python function, such as \
         `tylertoo.convert()` for `tiles`.\n\n",
    );
    let mut shared = Vec::new();

    for section in markdown.split("\n## `").skip(1) {
        let (name, content) = section.split_once("`\n\n").unwrap();
        let subcommand = name.strip_prefix("tylertoo ");
        let definition = subcommand
            .and_then(|name| command.find_subcommand(name))
            .unwrap_or(&command);
        writeln!(output, "## `{name}`\n").unwrap();
        let function = subcommand.and_then(|name| {
            PYTHON_FUNCTIONS
                .iter()
                .find(|(command, _)| *command == name)
                .map(|(_, function)| *function)
        });

        let (intro, remainder) = content.split_once("###### **").unwrap_or((content, ""));
        let intro = intro.trim().replace("**Usage:** `", "```text\n");
        // Usage precedes any after-help text, which can itself contain backticks.
        let intro = if let Some((before, after)) = intro.split_once("```text\n") {
            let (usage, after) = after
                .split_once("`\n")
                .unwrap_or((after.trim_end_matches('`'), ""));
            format!("{before}```text\n{usage}\n```\n{after}")
        } else {
            intro
        };
        writeln!(output, "{}\n", intro.trim()).unwrap();

        if subcommand.is_none() {
            output.push_str("### Subcommands\n\n| Command | Purpose |\n| --- | --- |\n");
            for subcommand in command.get_subcommands().filter(|cmd| !cmd.is_hide_set()) {
                let name = subcommand.get_name();
                writeln!(
                    output,
                    "| [`{name}`](#tylertoo-{name}) | {} |",
                    subcommand.get_about().unwrap()
                )
                .unwrap();
            }
            output.push('\n');
            // Global options (`-q`) appear only here: clap-markdown reads the
            // unbuilt command, which has not copied them into subcommands.
            for part in remainder.split("###### **").filter(|part| !part.is_empty()) {
                let (heading, body) = part.split_once(":**\n").unwrap();
                if heading == "Options" {
                    write_table(&mut output, "Global options", &parse_entries(body), false);
                    output.push_str("These go before or after the subcommand.\n\n");
                }
            }
            continue;
        }

        for part in remainder.split("###### **").filter(|part| !part.is_empty()) {
            let (heading, body) = part.split_once(":**\n").unwrap();
            let entries = parse_entries(body);
            if heading == "Arguments" {
                write_table(&mut output, "Arguments", &entries, false);
                continue;
            }
            let arguments: Vec<_> = definition
                .get_arguments()
                .filter(|arg| !arg.is_positional() && !arg.is_hide_set())
                .collect();
            assert_eq!(arguments.len(), entries.len(), "option count for {name}");
            let mut groups = Vec::new();
            for (arg, mut entry) in arguments.into_iter().zip(entries) {
                entry.2 = python_cell(function, arg);
                if matches!(subcommand, Some("tiles" | "overview"))
                    && shared_ids.contains(&arg.get_id())
                {
                    if subcommand == Some("tiles") {
                        add_group(
                            &mut shared,
                            arg.get_help_heading().unwrap_or("Options"),
                            entry,
                        );
                    }
                } else {
                    add_group(
                        &mut groups,
                        arg.get_help_heading().unwrap_or("Options"),
                        entry,
                    );
                }
            }
            for (heading, entries) in groups {
                write_table(&mut output, &heading, &entries, function.is_some());
            }
            if matches!(subcommand, Some("tiles" | "overview")) {
                output.push_str(
                    "Also accepts all [shared conversion options](#shared-conversion-options).\n\n",
                );
            }
        }
    }

    output.push_str("## Shared conversion options\n\nThese options apply to both `tylertoo tiles` and `tylertoo overview`.\n\n");
    for (heading, entries) in shared {
        write_table(&mut output, &heading, &entries, true);
    }
    output.push_str("## Environment variables\n\n| Variable | Meaning |\n| --- | --- |\n");
    for (name, meaning) in tylertoo_core::internal::environment::ENVIRONMENT {
        writeln!(output, "| `{name}` | {meaning} |").unwrap();
    }
    output.truncate(output.trim_end().len());
    output.push('\n');
    output
}

/// Syntax, description, and the Python keyword cell.
type Entry = (String, String, String);
type Group = (String, Vec<Entry>);

fn add_group(groups: &mut Vec<Group>, heading: &str, entry: Entry) {
    if let Some((_, entries)) = groups.iter_mut().find(|(name, _)| name == heading) {
        entries.push(entry);
    } else {
        groups.push((heading.to_owned(), vec![entry]));
    }
}

fn parse_entries(markdown: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in markdown.lines().filter(|line| !line.trim().is_empty()) {
        if let Some(line) = line.strip_prefix("* ") {
            let (syntax, description) = line.split_once(" — ").unwrap_or((line, ""));
            entries.push((syntax.to_owned(), description.to_owned(), String::new()));
        } else {
            let description = &mut entries.last_mut().unwrap().1;
            let line = line.trim().replace("Default value:", "Default:");
            if !description.ends_with('.') {
                description.push('.');
            }
            description.push(' ');
            description.push_str(&line);
            if !description.ends_with('.') {
                description.push('.');
            }
        }
    }
    entries
}

fn write_table(output: &mut String, heading: &str, entries: &[Entry], python: bool) {
    let (python_heading, python_rule) = if python {
        (" Python |", " --- |")
    } else {
        ("", "")
    };
    writeln!(
        output,
        "### {heading}\n\n| {} |{python_heading} Description |\n| --- |{python_rule} --- |",
        if heading == "Arguments" {
            "Argument"
        } else {
            "Option"
        }
    )
    .unwrap();
    for (syntax, description, keyword) in entries {
        // Give the first row for each long option a stable cross-reference.
        // Repeated command surfaces share that spelling and its anchor.
        let anchor = syntax
            .split_once("--")
            .map(|(_, rest)| {
                let flag = rest
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                    .next()
                    .unwrap();
                format!("<a id=\"option-{flag}\"></a>")
            })
            .filter(|anchor| !output.contains(anchor));
        // Pipes inside code spans still delimit Markdown table cells.
        let punctuation = if description.ends_with('.') { "" } else { "." };
        writeln!(
            output,
            "| {}{} |{} {}{punctuation} |",
            anchor.unwrap_or_default(),
            syntax.replace('|', "\\|"),
            if python {
                format!(" {keyword} |")
            } else {
                String::new()
            },
            description.replace('|', "\\|")
        )
        .unwrap();
    }
    output.push('\n');
}

/// The Python function behind each subcommand that has one.
const PYTHON_FUNCTIONS: &[(&str, &str)] = &[
    ("tiles", "convert"),
    ("overview", "overview"),
    ("export-pmtiles", "export_pmtiles"),
    ("pyramid", "pyramid"),
    ("decode", "decode"),
    ("stats", "stats"),
    ("serve", "serve"),
];

/// Options of Python-backed subcommands that no Python keyword covers.
#[cfg(test)]
const CLI_ONLY: &[&str] = &[
    "files_from",
    "report",
    "shard",
    "shard_plan",
    "plan_only",
    "verbose",
    "force",
    "json",
    "zoom",
];

/// The Python keywords for one CLI option: its clap id when that is a keyword
/// of `function` (from `command_defaults`), else its renamed spellings.
fn python_keywords(function: &str, id: &str) -> Vec<String> {
    let keys = keywords::command_defaults(function);
    let names: &[&str] = match id {
        "gsd" => &["gsds"],
        "band" => &["bands"],
        "class_rank" => &["class_rank_column", "class_ranks", "class_rank_unknown"],
        "shared_nodes" => &["no_simplification_of_shared_nodes"],
        // Python's `pyramid(bands, output)` takes it positionally.
        "bands" if function == "pyramid" => return vec!["bands".to_owned()],
        _ => std::slice::from_ref(&id),
    };
    names
        .iter()
        .filter(|name| keys.contains_key(**name))
        .map(|name| (*name).to_owned())
        .collect()
}

fn python_cell(function: Option<&str>, arg: &clap::Arg) -> String {
    let Some(function) = function else {
        return String::new();
    };
    let names = python_keywords(function, arg.get_id().as_str());
    // A `--no-*` switch that clears a keyword is that keyword set to False.
    let value = if matches!(arg.get_action(), clap::ArgAction::SetFalse) {
        "=False"
    } else {
        ""
    };
    if names.is_empty() {
        "CLI only".to_owned()
    } else {
        names
            .iter()
            .map(|name| format!("`{name}{value}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_option_names_its_python_keyword_or_is_cli_only() {
        let command = Cli::command();
        let mut failures = Vec::new();
        for (name, function) in PYTHON_FUNCTIONS {
            let subcommand = command.find_subcommand(name).unwrap();
            for arg in subcommand
                .get_arguments()
                .filter(|arg| !arg.is_positional() && !arg.is_hide_set())
            {
                let id = arg.get_id().as_str();
                let mapped = !python_keywords(function, id).is_empty();
                if mapped == CLI_ONLY.contains(&id) {
                    failures.push(format!("{name} --{id}: mapped={mapped}"));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "fix PYTHON aliases or CLI_ONLY: {failures:#?}"
        );
    }

    #[test]
    fn cli_reference_shows_renamed_python_keywords() {
        let page = crate::gen_reference_markdown();
        for needle in [
            "| `--max-zoom <MAX_ZOOM>`",
            "| `gsds` |",
            "| `class_rank_column`, `class_ranks`, `class_rank_unknown` |",
            "| `no_simplification_of_shared_nodes` |",
            "| `density_drop=False` |",
            "| CLI only |",
        ] {
            assert!(page.contains(needle), "cli.md lacks {needle}");
        }
    }
}
