//! Generates the per-subcommand reference tables in `docs/cli.md` from the
//! `clap` command tree exported by `zaz::cli::Cli`.

use anyhow::{Context, Result};
use clap::{Arg, ArgAction, Command, CommandFactory};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use zaz::cli::Cli;

use crate::splicer::splice;

const DEFAULT_DOCS_PATH: &str = "docs/cli.md";

pub fn run(write: bool, path: Option<PathBuf>) -> Result<ExitCode> {
    let path = path.unwrap_or_else(|| PathBuf::from(DEFAULT_DOCS_PATH));
    let original =
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let regenerated = regenerate(&original)?;

    if regenerated == original {
        return Ok(ExitCode::SUCCESS);
    }

    if write {
        fs::write(&path, &regenerated).with_context(|| format!("writing {}", path.display()))?;
        println!("xtask: regenerated {}", path.display());
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!(
            "xtask: {} is out of date; rerun `make docs-cli` to regenerate.",
            path.display()
        );
        eprintln!("{}", unified_diff(&original, &regenerated, &path));
        Ok(ExitCode::from(1))
    }
}

fn regenerate(original: &str) -> Result<String> {
    let root = Cli::command();
    let mut current = original.to_string();

    let root_marker = root.get_name().to_string();
    current = splice(&root_marker, &render_command(&root, true), &current)
        .with_context(|| format!("splicing {root_marker}"))?;

    // Hidden subcommands (e.g. the internal `supervisor` launcher) are excluded
    // from the user-facing reference.
    for sub in root.get_subcommands().filter(|sub| !sub.is_hide_set()) {
        let marker = format!("{} {}", root.get_name(), sub.get_name());
        let body = render_command(sub, false);
        current = splice(&marker, &body, &current).with_context(|| format!("splicing {marker}"))?;
    }

    Ok(current)
}

fn render_command(cmd: &Command, is_root: bool) -> String {
    let mut out = String::new();

    let positionals: Vec<&Arg> = cmd.get_positionals().collect();
    let options: Vec<&Arg> = cmd
        .get_arguments()
        .filter(|a| !a.is_positional() && !is_implicit_help_or_version(a))
        .collect();

    if positionals.is_empty() && options.is_empty() {
        out.push_str("This subcommand takes no arguments or flags.\n");
        if is_root {
            out.push_str("\nGlobal flags above apply when invoking `zaz` with no subcommand.\n");
        }
        return out;
    }

    if !positionals.is_empty() {
        out.push_str("**Positional arguments**\n\n");
        out.push_str("| Argument | Required | Description |\n");
        out.push_str("|----------|----------|-------------|\n");
        for arg in &positionals {
            out.push_str(&format_positional_row(arg));
            out.push('\n');
        }
        out.push('\n');
    }

    if !options.is_empty() {
        out.push_str("**Flags**\n\n");
        out.push_str("| Flag | Default | Description |\n");
        out.push_str("|------|---------|-------------|\n");
        for arg in &options {
            out.push_str(&format_option_row(arg));
            out.push('\n');
        }
    }

    out
}

fn is_implicit_help_or_version(arg: &Arg) -> bool {
    matches!(
        arg.get_action(),
        ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
    )
}

fn format_positional_row(arg: &Arg) -> String {
    let name = arg
        .get_value_names()
        .and_then(|names| names.first().map(|n| format!("`{}`", n)))
        .unwrap_or_else(|| format!("`{}`", arg.get_id()));
    let required = if arg.is_required_set() { "yes" } else { "no" };
    let description = pick_help(arg);
    format!("| {name} | {required} | {description} |")
}

fn format_option_row(arg: &Arg) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(short) = arg.get_short() {
        parts.push(format!("`-{short}`"));
    }
    if let Some(long) = arg.get_long() {
        parts.push(format!("`--{long}`"));
    }
    if parts.is_empty() {
        parts.push(format!("`{}`", arg.get_id()));
    }

    let mut name = parts.join(", ");
    if takes_value(arg) {
        let value_name = arg
            .get_value_names()
            .and_then(|names| names.first().map(|n| n.to_string()))
            .unwrap_or_else(|| arg.get_id().to_string().to_uppercase());
        name.push_str(&format!(" `<{value_name}>`"));
    }

    let default = format_default(arg);
    let description = pick_help(arg);
    format!("| {name} | {default} | {description} |")
}

fn takes_value(arg: &Arg) -> bool {
    !matches!(
        arg.get_action(),
        ArgAction::SetTrue
            | ArgAction::SetFalse
            | ArgAction::Count
            | ArgAction::Help
            | ArgAction::HelpShort
            | ArgAction::HelpLong
            | ArgAction::Version
    )
}

fn format_default(arg: &Arg) -> String {
    let defaults = arg.get_default_values();
    if defaults.is_empty() {
        match arg.get_action() {
            ArgAction::SetTrue => "`false`".to_string(),
            ArgAction::SetFalse => "`true`".to_string(),
            ArgAction::Count => "`0`".to_string(),
            _ => "—".to_string(),
        }
    } else {
        defaults
            .iter()
            .map(|v| format!("`{}`", v.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn pick_help(arg: &Arg) -> String {
    let raw = arg
        .get_long_help()
        .map(|s| s.to_string())
        .or_else(|| arg.get_help().map(|s| s.to_string()))
        .unwrap_or_default();
    one_line(&raw)
}

fn one_line(text: &str) -> String {
    text.replace('\n', " ").trim().replace("  ", " ")
}

fn unified_diff(original: &str, updated: &str, path: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!("--- {} (committed)\n", path.display()));
    out.push_str(&format!("+++ {} (regenerated)\n", path.display()));

    let lhs: Vec<&str> = original.lines().collect();
    let rhs: Vec<&str> = updated.lines().collect();

    let limit = lhs.len().max(rhs.len());
    for i in 0..limit {
        match (lhs.get(i), rhs.get(i)) {
            (Some(a), Some(b)) if a == b => {}
            (Some(a), Some(b)) => {
                out.push_str(&format!("- {a}\n+ {b}\n"));
            }
            (Some(a), None) => out.push_str(&format!("- {a}\n")),
            (None, Some(b)) => out.push_str(&format!("+ {b}\n")),
            (None, None) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_command_empty_subcommand_produces_placeholder() {
        let cmd = Command::new("noop");
        let body = render_command(&cmd, false);
        assert!(body.contains("takes no arguments"));
    }

    #[test]
    fn render_command_emits_flag_table_for_options() {
        let cmd = Command::new("demo").arg(
            clap::Arg::new("path")
                .long("path")
                .value_name("FILE")
                .help("path to thing"),
        );
        let body = render_command(&cmd, false);
        assert!(body.contains("**Flags**"));
        assert!(body.contains("`--path` `<FILE>`"));
        assert!(body.contains("path to thing"));
    }

    #[test]
    fn render_command_emits_positional_table() {
        let cmd = Command::new("demo").arg(
            clap::Arg::new("group")
                .value_name("GROUP")
                .help("group name"),
        );
        let body = render_command(&cmd, false);
        assert!(body.contains("**Positional arguments**"));
        assert!(body.contains("`GROUP`"));
        assert!(body.contains("group name"));
    }

    #[test]
    fn regenerate_fills_every_marker_in_real_docs() {
        let cli_md = include_str!("../../../docs/cli.md");
        let regenerated = regenerate(cli_md).expect("regenerate");
        // None of the marker bodies should still contain the placeholder.
        assert!(
            !regenerated.contains("generated by tooling landed in milestone 23.5"),
            "marker placeholder text not replaced"
        );
    }
}
