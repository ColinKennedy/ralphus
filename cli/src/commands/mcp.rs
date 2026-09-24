//! `ralphus mcp initialize`: local MCP-host setup owned by each agent backend.

use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;

use ralphus_runner::claude_code_backend::ClaudeCodeBackend;
use ralphus_runner::codex_backend::CodexBackend;
use ralphus_runner::mcp_init::McpInitializer;
use ralphus_runner::pi_backend::PiBackend;

use crate::flags::Scanner;

#[derive(Debug)]
pub enum McpCommand {
    Initialize {
        host: String,
        profile_file: Option<String>,
        dry_run: bool,
        yes: bool,
    },
    Help,
    UsageError(String),
}

pub fn parse(args: &[String]) -> McpCommand {
    match args.first().map(String::as_str) {
        Some("initialize") => {
            let Some(host) = args.get(1).cloned() else {
                return McpCommand::UsageError(
                    "mcp initialize requires <claude|codex|pi>".to_string(),
                );
            };
            let mut inner = Scanner::new(&args[2..]);
            McpCommand::Initialize {
                host,
                profile_file: inner.take_value("--profile-file").ok().flatten(),
                dry_run: inner.take_bool("--dry-run"),
                yes: inner.take_bool("--yes"),
            }
        }
        None => McpCommand::Help,
        Some(other) => McpCommand::UsageError(format!("mcp: expected 'initialize', got {other:?}")),
    }
}

pub fn dispatch(command: McpCommand) -> i32 {
    match command {
        McpCommand::Help => {
            println!(
                "usage: ralphus mcp initialize <claude|codex|pi> [--profile-file <path>] [--dry-run] [--yes]"
            );
            0
        }
        McpCommand::UsageError(message) => {
            println!("usage error: {message}");
            2
        }
        McpCommand::Initialize {
            host,
            profile_file,
            dry_run,
            yes,
        } => initialize(&host, profile_file, dry_run, yes),
    }
}

pub(crate) fn initialize(
    host: &str,
    profile_file: Option<String>,
    dry_run: bool,
    yes: bool,
) -> i32 {
    if !matches!(host, "claude" | "codex" | "pi") {
        println!("error: unknown MCP host {host:?}; expected claude, codex, or pi");
        return 2;
    }
    let default_profile = default_profile_path();
    let profile_path = match profile_file {
        Some(path) => ralphus_core::expand_home(&path),
        None if dry_run || yes => default_profile,
        None => match choose_profile_path(&default_profile) {
            Ok(path) => path,
            Err(error) => {
                println!("error: {error}");
                return 2;
            }
        },
    };
    let backend: Box<dyn McpInitializer> = match host {
        "claude" => Box::new(ClaudeCodeBackend {
            keep_temporary_files: false,
            program_override: None,
        }),
        "codex" => Box::new(CodexBackend {
            keep_temporary_files: false,
            program_override: None,
        }),
        "pi" => Box::new(PiBackend {
            keep_temporary_files: false,
            program_override: None,
        }),
        _ => unreachable!("the host was checked above"),
    };
    let plan = match backend.mcp_initialization_plan(profile_path) {
        Ok(plan) => plan,
        Err(error) => {
            println!("error: {error}");
            return 2;
        }
    };
    println!("MCP initialization dry run for {}:", plan.host);
    println!("  use {}", plan.mcp_program.display());
    for install in &plan.third_party_installs {
        println!("  THIRD-PARTY install: {}", install.description);
        println!("    publisher: {}", install.publisher);
        println!("    source: {}", install.url);
    }
    for command in &plan.commands {
        println!(
            "  run {}: {} {}",
            command.description,
            command.program,
            command.args.join(" ")
        );
    }
    if plan.third_party_installs.is_empty() && plan.commands.is_empty() && plan.edits.is_empty() {
        println!("  no changes needed");
        return 0;
    }
    for edit in &plan.edits {
        println!("  edit {}: {}", edit.path.display(), edit.description);
    }
    if dry_run {
        return 0;
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            println!(
                "error: use --yes to apply this non-interactively, or --dry-run to inspect it"
            );
            return 2;
        }
        if !plan.third_party_installs.is_empty() {
            println!(
                "WARNING: this will install the third-party Pi MCP Adapter before writing the listed files."
            );
        }
        print!("Apply this plan? [Y/n] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no") {
            println!("aborted");
            return 1;
        }
    }
    match backend.apply_mcp_initialization(&plan) {
        Ok(()) => {
            println!(
                "{} will load ralphus MCP the next time it starts. Restart your shell to use the updated PATH.",
                plan.host
            );
            0
        }
        Err(error) => {
            println!("error: {error}");
            2
        }
    }
}

fn choose_profile_path(default: &std::path::Path) -> Result<PathBuf, String> {
    print!("PATH profile to edit [{}]: ", default.display());
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|error| format!("could not read profile path: {error}"))?;
    let answer = answer.trim();
    Ok(if answer.is_empty() {
        default.to_path_buf()
    } else {
        ralphus_core::expand_home(answer)
    })
}

fn default_profile_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let shell = std::env::var("SHELL")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if shell.contains("fish") {
        return home.join(".config").join("fish").join("config.fish");
    }
    if shell.contains("zsh") {
        return home.join(".zshrc");
    }
    if shell.contains("bash") {
        return home.join(".bashrc");
    }
    if std::env::var_os("PSModulePath").is_some() {
        let windows_powershell = std::env::var_os("PSModulePath")
            .is_some_and(|value| value.to_string_lossy().contains("WindowsPowerShell"));
        let directory = if windows_powershell {
            "WindowsPowerShell"
        } else {
            "PowerShell"
        };
        return home
            .join("Documents")
            .join(directory)
            .join("Microsoft.PowerShell_profile.ps1");
    }
    home.join(".profile")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    #[test]
    fn parses_codex_dry_run_with_an_explicit_profile() {
        let command = parse(&v(&[
            "initialize",
            "codex",
            "--profile-file",
            "~/.bashrc",
            "--dry-run",
        ]));
        assert!(
            matches!(command, McpCommand::Initialize { host, profile_file: Some(profile_file), dry_run: true, yes: false } if host == "codex" && profile_file == "~/.bashrc")
        );
    }

    #[test]
    fn initialize_requires_a_host() {
        assert!(
            matches!(parse(&v(&["initialize"])), McpCommand::UsageError(message) if message.contains("claude|codex|pi"))
        );
    }
}
