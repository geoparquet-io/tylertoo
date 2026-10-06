//! Layout for the generated CLI reference. Syntax and help come from clap.

use clap::{Args, CommandFactory};
use std::fmt::Write;

use super::{Cli, ConvertTuningArgs};

/// Group options under their CLI help headings and print shared tuning once.
pub(super) fn format(markdown: String) -> String {
    let command = Cli::command();
    let tuning = ConvertTuningArgs::augment_args(clap::Command::new("tuning"));
    let shared_ids: Vec<_> = tuning.get_arguments().map(clap::Arg::get_id).collect();
    let mut output = String::from("# CLI reference\n\n");
    let mut shared = Vec::new();

    for section in markdown.split("\n## `").skip(1) {
        let (name, content) = section.split_once("`\n\n").unwrap();
        let subcommand = name.strip_prefix("tylertoo ");
        let definition = subcommand
            .and_then(|name| command.find_subcommand(name))
            .unwrap_or(&command);
        writeln!(output, "## `{name}`\n").unwrap();

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
            continue;
        }

        for part in remainder.split("###### **").filter(|part| !part.is_empty()) {
            let (heading, body) = part.split_once(":**\n").unwrap();
            let entries = parse_entries(body);
            if heading == "Arguments" {
                write_table(&mut output, "Arguments", &entries);
                continue;
            }
            let arguments: Vec<_> = definition
                .get_arguments()
                .filter(|arg| !arg.is_positional() && !arg.is_hide_set())
                .collect();
            assert_eq!(arguments.len(), entries.len(), "option count for {name}");
            let mut groups = Vec::new();
            for (arg, entry) in arguments.into_iter().zip(entries) {
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
                write_table(&mut output, &heading, &entries);
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
        write_table(&mut output, &heading, &entries);
    }
    output.truncate(output.trim_end().len());
    output.push('\n');
    output
}

type Entry = (String, String);
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
            entries.push((syntax.to_owned(), description.to_owned()));
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

fn write_table(output: &mut String, heading: &str, entries: &[Entry]) {
    writeln!(
        output,
        "### {heading}\n\n| {} | Description |\n| --- | --- |",
        if heading == "Arguments" {
            "Argument"
        } else {
            "Option"
        }
    )
    .unwrap();
    for (syntax, description) in entries {
        // Pipes inside code spans still delimit Markdown table cells.
        let punctuation = if description.ends_with('.') { "" } else { "." };
        writeln!(
            output,
            "| {} | {}{punctuation} |",
            syntax.replace('|', "\\|"),
            description.replace('|', "\\|")
        )
        .unwrap();
    }
    output.push('\n');
}
