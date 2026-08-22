//! `ralphus show <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `show` group. Bare `ralphus show` prints group help
//! (`p_show.set_defaults(func=_help_printer(p_show))` in the Python source).
//!
//! Note: the Python `show` group has exactly one real subcommand,
//! `help-map` -- there is no `show tutor`. The Task TOML tutorial lives
//! under `task show-tutor` (`_cmd_show_tutor` in the Python source), which
//! this crate already exposes as the top-level `ralphus tutor` command
//! (`Command::TutorShow` in `commands/mod.rs`, backed by
//! `crate::tutor::task_tutor()`) -- no separate `show`-scoped tutor command
//! exists to port.
//!
//! `show help-map` is backed by [`crate::help_map`], this crate's port of
//! `ralphus.helpmap` (RAL-110): the six guidance notes followed by the full
//! command-tree text, matching `_cmd_show_help_map`'s exact print sequence.

use crate::args::GlobalOpts;

#[derive(Debug, Clone)]
pub enum ShowCommand {
    Help,
    HelpMap,
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> ShowCommand {
    match args.first().map(String::as_str) {
        None => ShowCommand::Help,
        Some("help-map") => ShowCommand::HelpMap,
        Some(other) => ShowCommand::UsageError(format!("unknown show subcommand: {other}")),
    }
}

#[must_use]
pub fn dispatch(cmd: ShowCommand, _opts: &GlobalOpts) -> i32 {
    match cmd {
        ShowCommand::Help => {
            println!("ralphus show <help-map>");
            0
        }
        ShowCommand::HelpMap => {
            println!("{}", crate::help_map::full_output());
            0
        }
        ShowCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bare_show_is_help() {
        matches!(parse(&[]), ShowCommand::Help);
    }

    #[test]
    fn parses_help_map() {
        matches!(parse(&v(&["help-map"])), ShowCommand::HelpMap);
    }

    #[test]
    fn help_map_dispatch_exits_0() {
        assert_eq!(dispatch(ShowCommand::HelpMap, &test_opts()), 0);
    }

    #[test]
    fn unknown_subcommand_is_usage_error() {
        matches!(parse(&v(&["bogus"])), ShowCommand::UsageError(_));
    }

    fn test_opts() -> GlobalOpts {
        GlobalOpts {
            daemon_url: "http://127.0.0.1:1".to_string(),
            json: false,
        }
    }
}
