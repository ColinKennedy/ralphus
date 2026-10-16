//! Offline check that a `command` is written for the shell it runs under
//! (RAL-651).
//!
//! Which shell a text belongs to is decided entirely by the tree-sitter
//! grammars in `ralphus-shell-syntax`: the command is parsed with every
//! grammar and the one reporting the fewest syntax errors fits best. Nothing
//! here inspects the text itself. Without the crate's `parser` feature every
//! check answers `None`, and a shell with no grammar is never checked.
//! Findings are advisory: the caller reports them as warnings.

use ralphus_shell_syntax::{Parsed, Shell, SpanKind, Status, parse, shell_for_tool};

use crate::shellcmd::shell_kind_from_exe;

const GRAMMARS: [Shell; 3] = [Shell::Bash, Shell::PowerShell, Shell::Cmd];

fn grammar_for(shell: &str) -> Option<Shell> {
    match shell_kind_from_exe(shell)? {
        "bash" | "sh" | "zsh" => Some(Shell::Bash),
        "powershell" | "pwsh" => Some(Shell::PowerShell),
        "cmd" => Some(Shell::Cmd),
        _ => None,
    }
}

fn name(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => "bash",
        Shell::PowerShell => "powershell",
        Shell::Cmd => "cmd",
    }
}

/// Syntax errors in a parse, or `None` when the grammar gave no verdict
/// (timeout, too long, parser not built in).
fn errors(parsed: &Parsed) -> Option<u32> {
    match parsed.status {
        Status::Ok => Some(0),
        Status::Unparsed => Some(parsed.error_count),
        _ => None,
    }
}

/// A warning message when `command` fits another shell's grammar better than
/// `shell` (the explicit `shell` of the cell or proof step, `None` when
/// unset), or `None` when it fits, ties, or cannot be checked. With no
/// explicit shell the host's default shell stands in for it.
#[must_use]
pub fn check_command(command: &str, shell: Option<&str>) -> Option<String> {
    let (declared, explicit) = match shell {
        Some(shell) => (grammar_for(shell)?, true),
        None => (shell_for_tool("exec", cfg!(windows))?, false),
    };
    let declared_parse = parse(declared, command);
    let declared_errors = errors(&declared_parse)?;
    if declared_errors == 0 {
        return None;
    }
    let best = GRAMMARS
        .into_iter()
        .filter(|g| *g != declared)
        .filter_map(|g| Some((errors(&parse(g, command))?, g)))
        .min_by_key(|(count, _)| *count);
    if let Some((count, other)) = best.filter(|(count, _)| *count < declared_errors) {
        let other_name = name(other);
        let verdict = if count == 0 {
            "is valid"
        } else {
            "fits better as"
        };
        return Some(if explicit {
            format!(
                "this command {verdict} {other_name} but its shell is {}; fix the command or set `shell = \"{other_name}\"`",
                name(declared),
            )
        } else {
            format!(
                "this command {verdict} {other_name}, not the default {}; want to set `shell = \"{other_name}\"` explicitly?",
                name(declared),
            )
        });
    }
    if !explicit {
        return None;
    }
    let at = declared_parse
        .error_line
        .map_or(String::new(), |n| format!(" at line {n} of the command"));
    Some(format!(
        "syntax error{at}: not valid {} syntax",
        name(declared)
    ))
}

const BASH_BUILTINS: &[&str] = &[
    ".",
    ":",
    "[",
    "[[",
    "alias",
    "bg",
    "bind",
    "break",
    "builtin",
    "caller",
    "cd",
    "command",
    "compgen",
    "complete",
    "continue",
    "declare",
    "dirs",
    "disown",
    "echo",
    "enable",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "getopts",
    "hash",
    "help",
    "history",
    "jobs",
    "kill",
    "let",
    "local",
    "logout",
    "mapfile",
    "popd",
    "printf",
    "pushd",
    "pwd",
    "read",
    "readarray",
    "readonly",
    "return",
    "set",
    "shift",
    "shopt",
    "source",
    "suspend",
    "test",
    "times",
    "trap",
    "true",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "wait",
    "if",
    "then",
    "else",
    "elif",
    "fi",
    "for",
    "while",
    "until",
    "do",
    "done",
    "case",
    "esac",
    "function",
    "select",
    "time",
    "!",
    "{",
    "}",
];

const CMD_BUILTINS: &[&str] = &[
    "assoc", "break", "call", "cd", "chdir", "cls", "color", "copy", "date", "del", "dir", "echo",
    "endlocal", "erase", "exit", "for", "ftype", "goto", "if", "md", "mkdir", "mklink", "move",
    "path", "pause", "popd", "prompt", "pushd", "rd", "rem", "ren", "rename", "rmdir", "set",
    "setlocal", "shift", "start", "time", "title", "type", "ver", "verify", "vol",
];

const POWERSHELL_ALIASES: &[&str] = &[
    "%", "?", "ac", "cat", "cd", "chdir", "clc", "clear", "cli", "clp", "cls", "cnsn", "compare",
    "copy", "cp", "cpi", "cvpa", "dbp", "del", "diff", "dir", "dnsn", "ebp", "echo", "epal",
    "epcsv", "erase", "etsn", "exsn", "fc", "fl", "foreach", "ft", "fw", "gal", "gbp", "gc", "gcb",
    "gci", "gcm", "gcs", "gdr", "ghy", "gi", "gin", "gjb", "gl", "gm", "gmo", "gp", "gps", "gpv",
    "group", "gsn", "gsv", "gu", "gv", "gwmi", "h", "history", "icm", "iex", "ihy", "ii", "ipal",
    "ipcsv", "ipmo", "iwr", "irm", "kill", "lp", "ls", "man", "md", "measure", "mi", "mount",
    "move", "mp", "mv", "nal", "ndr", "ni", "nmo", "npssc", "nsn", "nv", "ogv", "oh", "popd", "ps",
    "pushd", "pwd", "r", "rbp", "rcjb", "rcsn", "rd", "rdr", "ren", "ri", "rjb", "rm", "rmdir",
    "rmo", "rni", "rnp", "rp", "rsn", "rv", "rvpa", "rwmi", "sajb", "sal", "saps", "sasv", "sbp",
    "sc", "select", "set", "shcm", "si", "sl", "sleep", "sls", "sort", "sp", "spjb", "spps",
    "spsv", "start", "sv", "swmi", "tee", "type", "where", "wget", "curl", "write", "if",
    "foreach", "while", "switch", "function", "return", "try", "catch", "finally", "throw", "exit",
    "break", "continue", "param", "begin", "process", "end", "do", "until", "for", "else",
    "elseif", "trap", "filter", "workflow", "class", "using", "&", ".",
];

/// The distinct executable names `command` invokes, in first-seen order, per
/// the tree-sitter grammar of `shell` (the host default when `None`). Empty
/// when the grammar gives no clean parse or the parser is not built in. Names
/// whose value is only known at run time (variables, substitutions, `{name}` /
/// `<<...>>` placeholders), paths, and shell builtins/keywords are left out.
#[must_use]
pub fn executable_names(command: &str, shell: Option<&str>) -> Vec<String> {
    let grammar = match shell {
        Some(shell) => grammar_for(shell),
        None => shell_for_tool("exec", cfg!(windows)),
    };
    let Some(grammar) = grammar else {
        return Vec::new();
    };
    let parsed = parse(grammar, command);
    if parsed.status != Status::Ok {
        return Vec::new();
    }
    let mut names: Vec<String> = Vec::new();
    for span in parsed
        .spans
        .iter()
        .filter(|s| s.kind == SpanKind::CommandName)
    {
        let Some(raw) = command.get(span.start..span.end) else {
            continue;
        };
        let name = raw.trim();
        let name = ["\"", "'"]
            .iter()
            .find_map(|q| name.strip_prefix(q)?.strip_suffix(q))
            .unwrap_or(name);
        let dynamic = name.is_empty()
            || name.contains([
                '$', '`', '%', '(', ')', '{', '}', '<', '>', '"', '\'', '*', '?', '~', '=', '/',
                '\\',
            ]);
        if dynamic || is_builtin(grammar, name) || names.iter().any(|n| n == name) {
            continue;
        }
        names.push(name.to_string());
    }
    names
}

fn is_builtin(shell: Shell, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    match shell {
        Shell::Bash => BASH_BUILTINS.contains(&name),
        Shell::Cmd => CMD_BUILTINS.contains(&lower.as_str()),
        // Cmdlets and functions (`Verb-Noun`) cannot be enumerated without
        // starting PowerShell, so a hyphenated name is assumed to exist.
        Shell::PowerShell => name.contains('-') || POWERSHELL_ALIASES.contains(&lower.as_str()),
    }
}

/// Whether `name` resolves to an executable on this host's `PATH` the way
/// `shell` would look it up (`PATHEXT` for cmd/PowerShell on Windows).
#[must_use]
pub fn executable_on_path(name: &str, shell: Shell) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let mut suffixes = vec![String::new()];
    if cfg!(windows) && shell != Shell::Bash {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        suffixes.extend(
            pathext
                .split(';')
                .filter(|e| !e.is_empty())
                .map(str::to_string),
        );
    }
    std::env::split_paths(&path).any(|dir| {
        suffixes.iter().any(|ext| {
            let ext_matches = !cfg!(windows) || !ext.is_empty() || name.contains('.');
            ext_matches && dir.join(format!("{name}{ext}")).is_file()
        })
    })
}

/// Memoizes executable lookups so one validation checks each distinct
/// `(shell, name)` at most once.
#[derive(Default)]
pub struct CommandAvailability {
    checked: std::collections::HashMap<(&'static str, String), bool>,
    /// How many lookups hit the host (not the cache).
    lookups: usize,
}

impl CommandAvailability {
    /// Warning messages, one per executable in `command` that cannot be found
    /// on this host for `shell`. Names already looked up are not re-checked,
    /// and a name already reported missing is not reported again.
    pub fn missing(&mut self, command: &str, shell: Option<&str>) -> Vec<String> {
        let grammar = match shell {
            Some(shell) => grammar_for(shell),
            None => shell_for_tool("exec", cfg!(windows)),
        };
        let Some(grammar) = grammar else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for exe in executable_names(command, shell) {
            let key = (name(grammar), exe.clone());
            if self.checked.contains_key(&key) {
                continue;
            }
            self.lookups += 1;
            let found = executable_on_path(&exe, grammar);
            self.checked.insert(key, found);
            if !found {
                out.push(format!(
                    "command `{exe}` was not found on the daemon host (checked for {}); check the selected shell or correct the command name. It may still run if it is an alias, a shell function, or only exists where the command executes",
                    name(grammar),
                ));
            }
        }
        out
    }

    /// Number of host lookups performed (cache hits excluded).
    #[must_use]
    pub fn lookups(&self) -> usize {
        self.lookups
    }
}

#[cfg(all(test, feature = "shell-syntax"))]
mod tests {
    use super::*;

    #[test]
    fn ambiguous_command_is_silent() {
        for shell in [None, Some("bash"), Some("powershell")] {
            assert_eq!(check_command("cargo test", shell), None);
        }
    }

    #[test]
    fn unset_shell_compares_against_the_host_default() {
        let msg = check_command("if [ -f a ]; then echo hi; fi", None);
        if cfg!(windows) {
            assert!(msg.unwrap().contains("shell = \"bash\""));
        } else {
            assert_eq!(msg, None);
        }
        let msg = check_command("foreach ($i in 1..3) { Write-Host $i }", None);
        if cfg!(windows) {
            assert_eq!(msg, None);
        } else {
            assert!(msg.unwrap().contains("shell = \"powershell\""));
        }
    }

    #[test]
    fn bash_command_under_powershell_names_both_shells() {
        let msg = check_command("if [ -f a ]; then echo hi; fi", Some("powershell")).unwrap();
        assert!(msg.contains("bash") && msg.contains("powershell"), "{msg}");
    }

    #[test]
    fn powershell_command_under_bash_names_both_shells() {
        let msg = check_command("foreach ($i in 1..3) { Write-Host $i }", Some("bash")).unwrap();
        assert!(msg.contains("shell = \"powershell\""), "{msg}");
    }

    #[test]
    fn syntax_error_reports_line() {
        let msg = check_command("echo ok\nif then ((", Some("bash")).unwrap();
        assert!(msg.contains("line 2"), "{msg}");
    }

    #[test]
    fn extracts_distinct_executables_across_chains() {
        let names = executable_names(
            "cd x && cargo fmt; cargo test | rg foo || echo $X; $CMD run; zzz-tool",
            Some("bash"),
        );
        assert_eq!(names, vec!["cargo", "rg", "zzz-tool"]);
    }

    #[test]
    fn powershell_skips_cmdlets_and_aliases() {
        let names = executable_names("Get-ChildItem; ls; git status; git log", Some("powershell"));
        assert_eq!(names, vec!["git"]);
    }

    #[test]
    fn cmd_skips_builtins() {
        let names = executable_names("echo hi && dir && cargo build", Some("cmd"));
        assert_eq!(names, vec!["cargo"]);
    }

    #[test]
    fn placeholders_and_paths_are_not_checked() {
        assert!(executable_names("./stop-demo --port 1", Some("bash")).is_empty());
        assert!(executable_names("{tool} run", Some("bash")).is_empty());
    }

    #[test]
    fn missing_command_warns_once_with_guidance() {
        let mut avail = CommandAvailability::default();
        let first = avail.missing(
            "ralphus-no-such-tool-xyz a; ralphus-no-such-tool-xyz b",
            Some("bash"),
        );
        assert_eq!(first.len(), 1);
        assert!(first[0].contains("daemon host") && first[0].contains("selected shell"));
        assert!(first[0].contains("ralphus-no-such-tool-xyz"));
        assert!(
            avail
                .missing("ralphus-no-such-tool-xyz c", Some("bash"))
                .is_empty()
        );
        assert_eq!(avail.lookups(), 1);
    }

    #[test]
    fn lookups_are_per_shell() {
        let mut avail = CommandAvailability::default();
        avail.missing("ralphus-no-such-tool-xyz", Some("bash"));
        avail.missing("ralphus-no-such-tool-xyz", Some("cmd"));
        assert_eq!(avail.lookups(), 2);
    }

    #[test]
    fn present_command_is_silent() {
        let mut avail = CommandAvailability::default();
        // The test binary was launched by cargo, so cargo is on PATH.
        assert!(avail.missing("cargo test", None).is_empty());
    }

    #[test]
    fn unknown_shell_is_not_checked() {
        assert_eq!(check_command("if then ((", Some("fish")), None);
    }
}

#[cfg(test)]
mod tests_off {
    use super::*;

    #[test]
    fn skipped_without_the_feature() {
        // Cargo feature unification can enable the parser through the
        // dependency alone, without this crate's `shell-syntax` feature.
        if ralphus_shell_syntax::available() {
            return;
        }
        assert_eq!(check_command("Get-ChildItem", None), None);
        assert_eq!(check_command("if then ((", Some("bash")), None);
    }
}
