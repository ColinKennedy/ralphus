//! Best-effort per-command labels for shell command lines, and the generic
//! tool-argument renderer, shared by the agent backends that surface tool
//! calls.

use serde_json::Value;

/// Renders a tool call's arguments compactly for the live tmux pane
/// (RAL-102), mirroring the old `claude_code_backend.py`'s
/// `_format_tool_input`. `truncate_chars` (RAL-303) is the per-value
/// character budget before a trailing `…` is appended -- configurable via
/// `[live_view] tool_arg_truncate_chars`.
pub fn format_tool_input(input: &Value, truncate_chars: usize) -> String {
    let Some(obj) = input.as_object() else {
        return String::new();
    };
    obj.iter()
        .map(|(key, value)| {
            let text = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let text = if text.chars().count() > truncate_chars {
                let truncated: String = text.chars().take(truncate_chars).collect();
                format!("{truncated}…")
            } else {
                text
            };
            format!("{key}={text:?}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One lexed piece of a shell command line.
enum ShellTok {
    Word(String),
    /// An unquoted statement separator: `;`, `|`, `&`, or a newline.
    Sep,
}

/// Best-effort shell lexer: whitespace-separated words, `'...'` literal,
/// `"..."` with `\"` unescaped, unquoted `\` literal (Windows paths).
/// Returns `None` on an unterminated quote.
fn lex_shell(src: &str) -> Option<Vec<ShellTok>> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        ch => cur.push(ch),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' if chars.peek() == Some(&'"') => {
                            chars.next();
                            cur.push('"');
                        }
                        ch => cur.push(ch),
                    }
                }
            }
            ';' | '|' | '&' | '\n' => {
                if in_word {
                    toks.push(ShellTok::Word(std::mem::take(&mut cur)));
                    in_word = false;
                }
                toks.push(ShellTok::Sep);
            }
            c if c.is_whitespace() => {
                if in_word {
                    toks.push(ShellTok::Word(std::mem::take(&mut cur)));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        toks.push(ShellTok::Word(cur));
    }
    Some(toks)
}

fn is_env_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// Normalizes an executable token into a label: basename, `.exe` stripped,
/// lowercased unless it is a `Verb-Noun` cmdlet written as-is. `None` when
/// the result is outside `[A-Za-z0-9_.-]` (labels must never contain `]`).
fn label_from_token(tok: &str) -> Option<String> {
    let base = tok.rsplit(['/', '\\']).next().unwrap_or(tok);
    let mut changed = base.len() != tok.len();
    let stem = if base.len() > 4 && base[base.len() - 4..].eq_ignore_ascii_case(".exe") {
        changed = true;
        &base[..base.len() - 4]
    } else {
        base
    };
    let is_cmdlet =
        !changed && stem.starts_with(|c: char| c.is_ascii_uppercase()) && stem.contains('-');
    let label = if is_cmdlet {
        stem.to_string()
    } else {
        stem.to_ascii_lowercase()
    };
    let ok = !label.is_empty()
        && !label.starts_with('.')
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    ok.then_some(label)
}

/// Shell control-flow words and grouping tokens that begin a compound
/// statement rather than naming a command.
fn is_shell_keyword(word: &str) -> bool {
    matches!(
        word,
        "for"
            | "if"
            | "while"
            | "until"
            | "case"
            | "select"
            | "function"
            | "do"
            | "done"
            | "then"
            | "else"
            | "elif"
            | "fi"
            | "esac"
            | "{"
            | "}"
            | "("
            | ")"
            | "!"
            | "[["
            | "time"
            | "foreach"
            | "switch"
            | "try"
    )
}

/// Label for the first real command in a script, skipping leading
/// `$var=...;` / `VAR=val` assignments and `cd <dir>` statements.
fn script_label(script: &str) -> Option<String> {
    let toks = lex_shell(script)?;
    let mut stmts: Vec<Vec<&str>> = vec![Vec::new()];
    for t in &toks {
        match t {
            ShellTok::Word(w) => stmts.last_mut()?.push(w),
            ShellTok::Sep => stmts.push(Vec::new()),
        }
    }
    for stmt in stmts {
        let mut words = stmt.as_slice();
        loop {
            match words {
                [] => break,
                [first, rest @ ..] if first.starts_with('$') => {
                    let assign =
                        first.contains('=') || rest.first().is_some_and(|n| n.starts_with('='));
                    if assign {
                        // PowerShell assignment: the whole statement is setup.
                        break;
                    }
                    return None;
                }
                [first, rest @ ..] if is_env_assignment(first) => words = rest,
                // A leading `cd <dir> &&` / `cd <dir>;` is setup, not the command.
                [first, ..] if first.eq_ignore_ascii_case("cd") => break,
                [first, ..] if is_shell_keyword(first) => return None,
                [first, ..] => return label_from_token(first),
            }
        }
    }
    None
}

/// Per-command label for a Codex `command_execution` command line, e.g.
/// `pwsh.exe -Command 'git status'` -> `git`. `None` when it cannot be
/// parsed reliably.
pub fn exec_command_label(command: &str) -> Option<String> {
    let toks = lex_shell(command.trim())?;
    let words: Vec<&str> = toks
        .iter()
        .map_while(|t| match t {
            ShellTok::Word(w) => Some(w.as_str()),
            ShellTok::Sep => None,
        })
        .collect();
    let first = *words.first()?;
    let base = first.rsplit(['/', '\\']).next().unwrap_or(first);
    let base = base.to_ascii_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    let args = &words[1..];
    let script: Option<String> = match base {
        "pwsh" | "powershell" => {
            let mut i = 0;
            let mut found = None;
            while i < args.len() {
                let a = args[i].to_ascii_lowercase();
                if matches!(a.as_str(), "-command" | "-c" | "-commandwithargs") {
                    found = Some(args[i + 1..].join(" "));
                    break;
                } else if matches!(
                    a.as_str(),
                    "-executionpolicy"
                        | "-ep"
                        | "-workingdirectory"
                        | "-wd"
                        | "-version"
                        | "-inputformat"
                        | "-outputformat"
                        | "-windowstyle"
                        | "-configurationname"
                ) {
                    i += 2;
                } else if a.starts_with('-') {
                    i += 1;
                } else {
                    found = Some(args[i..].join(" "));
                    break;
                }
            }
            Some(found?)
        }
        "bash" | "sh" | "zsh" | "dash" => {
            let mut i = 0;
            let mut found = None;
            while i < args.len() {
                let a = args[i];
                if a.starts_with('-') && !a.starts_with("--") && a[1..].contains('c') {
                    found = Some(args.get(i + 1)?.to_string());
                    break;
                } else if a.starts_with('-') {
                    i += 1;
                } else {
                    break;
                }
            }
            Some(found?)
        }
        "cmd" => {
            let i = args
                .iter()
                .position(|a| a.eq_ignore_ascii_case("/c") || a.eq_ignore_ascii_case("/k"))?;
            Some(args[i + 1..].join(" "))
        }
        _ => None,
    };
    match script {
        Some(s) => script_label(&s),
        None => script_label(command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(cmd: &str) -> Option<String> {
        exec_command_label(cmd)
    }

    #[test]
    fn cd_prefix_is_skipped() {
        assert_eq!(
            label("cd \"$PWD\" && git status --short").as_deref(),
            Some("git")
        );
        assert_eq!(label("cd x; cargo test").as_deref(), Some("cargo"));
        assert_eq!(
            label("bash -lc 'cd x && cargo test'").as_deref(),
            Some("cargo")
        );
        assert_eq!(
            label(r#"cd "C:\a\wt-ral-488" && git status"#).as_deref(),
            Some("git")
        );
        assert_eq!(label("cd x").as_deref(), None);
    }

    #[test]
    fn keywords_fall_back() {
        for cmd in [
            "for i in $(seq 1 20); do
  echo $i
done",
            "until ! pgrep -f x > /dev/null; do sleep 2; done",
            "if true; then ls; fi",
            "while true; do ls; done",
            "{ ls; }",
            "( ls )",
            "cd x && for i in 1; do ls; done",
        ] {
            assert_eq!(label(cmd), None, "{cmd}");
        }
    }

    #[test]
    fn bare_powershell_script() {
        assert_eq!(
            label("Get-Content a.txt | Select -First 1").as_deref(),
            Some("Get-Content")
        );
        assert_eq!(label("git status").as_deref(), Some("git"));
    }
}
