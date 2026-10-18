//! Reduce a shell command line to the commands it runs, for matching a live
//! agent's recipe against the recorded tool calls (RAL-682).
//!
//! One combined, quote-aware pass serves Bash, PowerShell and cmd alike -- it
//! does not try to tell the shells apart and does not need the optional
//! display-only shell parser. A command line becomes a list of units:
//! * `&&`, `||`, `;`, `&` and line breaks separate units;
//! * directory navigation (`cd`, `cd /d`, `Set-Location`, `Push-Location`,
//!   `Pop-Location`, ...) is dropped;
//! * redirections (`> file`, `>> file`, `2>&1`, ...) are dropped;
//! * pipeline stages after the first that only filter or display output
//!   (`head`, `grep`, `Select-Object`, `findstr`, `more`, ...) are dropped;
//! * environment settings (`MODE=x`, `$env:MODE='x'`, `export`, `set`) stay
//!   part of the unit they precede, so they are never silently lost.
//!
//! Quoted text and text inside `(...)`/`{...}` is never an operator.

/// Output-only stages a pipeline may add after a command.
const FILTER_COMMANDS: &[&str] = &[
    "head",
    "tail",
    "grep",
    "rg",
    "tee",
    "select-object",
    "where-object",
    "select-string",
    "findstr",
    "more",
];

/// Directory navigation, which a recipe may leave out.
const NAVIGATION_COMMANDS: &[&str] = &[
    "cd",
    "chdir",
    "pushd",
    "popd",
    "set-location",
    "push-location",
    "pop-location",
];

#[derive(Debug, PartialEq, Eq)]
enum ShellTok {
    Word(String),
    /// `&&`, `||`, `;`, `&` or a line break.
    Separator,
    Pipe,
}

fn shell_tokens(command: &str) -> Vec<ShellTok> {
    let chars: Vec<char> = command.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has_word = false;
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    let mut i = 0;
    let flush = |out: &mut Vec<ShellTok>, cur: &mut String, has_word: &mut bool| {
        if *has_word {
            out.push(ShellTok::Word(std::mem::take(cur)));
            *has_word = false;
        }
    };
    let separate = |out: &mut Vec<ShellTok>| {
        if out.last() != Some(&ShellTok::Separator) {
            out.push(ShellTok::Separator);
        }
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if let Some(q) = quote {
            cur.push(c);
            if c == '\\' && q == '"' {
                if let Some(n) = next {
                    cur.push(n);
                    i += 1;
                }
            } else if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                cur.push(c);
                has_word = true;
            }
            '(' | '{' => {
                depth += 1;
                cur.push(c);
                has_word = true;
            }
            ')' | '}' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
                has_word = true;
            }
            _ if depth > 0 => {
                cur.push(c);
                has_word = true;
            }
            '\n' | '\r' | ';' => {
                flush(&mut out, &mut cur, &mut has_word);
                separate(&mut out);
            }
            c if c.is_whitespace() => flush(&mut out, &mut cur, &mut has_word),
            '&' => {
                if next == Some('&') {
                    flush(&mut out, &mut cur, &mut has_word);
                    separate(&mut out);
                    i += 1;
                } else if cur.ends_with(['>', '<']) || next == Some('>') {
                    // `2>&1`, `>&2`, `&>file`: part of a redirection.
                    cur.push(c);
                    has_word = true;
                } else {
                    flush(&mut out, &mut cur, &mut has_word);
                    separate(&mut out);
                }
            }
            '|' => {
                flush(&mut out, &mut cur, &mut has_word);
                if next == Some('|') {
                    separate(&mut out);
                    i += 1;
                } else {
                    out.push(ShellTok::Pipe);
                    if next == Some('&') {
                        i += 1;
                    }
                }
            }
            _ => {
                cur.push(c);
                has_word = true;
            }
        }
        i += 1;
    }
    flush(&mut out, &mut cur, &mut has_word);
    out
}

/// `Some(true)` for a redirection that takes the next word as its target,
/// `Some(false)` for a self-contained one (`2>&1`, `>out.txt`), `None` for a
/// word that is not a redirection.
fn redirection(word: &str) -> Option<bool> {
    let rest = word.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&' || c == '*');
    let rest = rest
        .strip_prefix(">>")
        .or_else(|| rest.strip_prefix(['>', '<']))?;
    Some(rest.is_empty())
}

fn first_word_lower(words: &[String]) -> String {
    words
        .first()
        .map(|w| w.to_ascii_lowercase())
        .unwrap_or_default()
}

fn is_environment_setting(stage: &[String]) -> bool {
    let first = first_word_lower(stage);
    if first.starts_with("$env:") || ["export", "set", "setx"].contains(&first.as_str()) {
        return true;
    }
    stage.iter().all(|w| {
        w.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    })
}

/// The commands a command line is made of; see the module docs.
#[must_use]
pub fn command_units(command: &str) -> Vec<String> {
    let mut segments: Vec<Vec<Vec<String>>> = vec![vec![Vec::new()]];
    let mut skip_target = false;
    for tok in shell_tokens(command) {
        match tok {
            ShellTok::Separator => {
                skip_target = false;
                segments.push(vec![Vec::new()]);
            }
            ShellTok::Pipe => {
                skip_target = false;
                if let Some(last) = segments.last_mut() {
                    last.push(Vec::new());
                }
            }
            ShellTok::Word(word) => {
                if skip_target {
                    skip_target = false;
                } else if let Some(takes_target) = redirection(&word) {
                    skip_target = takes_target;
                } else if let Some(stage) = segments.last_mut().and_then(|s| s.last_mut()) {
                    stage.push(word);
                }
            }
        }
    }
    let mut units = Vec::new();
    let mut environment: Vec<String> = Vec::new();
    for stages in segments {
        let mut stages = stages.into_iter().filter(|s| !s.is_empty());
        let Some(head) = stages.next() else { continue };
        if NAVIGATION_COMMANDS.contains(&first_word_lower(&head).as_str()) {
            continue;
        }
        let mut text = head.join(" ");
        if !is_environment_setting(&head) {
            for stage in stages {
                if !FILTER_COMMANDS.contains(&first_word_lower(&stage).as_str()) {
                    text.push_str(" | ");
                    text.push_str(&stage.join(" "));
                }
            }
        }
        environment.push(text);
        if !is_environment_setting(&head) {
            units.push(environment.join("; "));
            environment.clear();
        }
    }
    units
}

#[cfg(test)]
mod tests {
    use super::command_units;

    fn units(command: &str) -> Vec<String> {
        command_units(command)
    }

    #[test]
    fn filters_navigation_and_redirection_are_dropped() {
        assert_eq!(
            units("fizz buzz --count 300 | head -n 10"),
            ["fizz buzz --count 300"]
        );
        assert_eq!(
            units("cargo test -p app 2>&1 | grep -E 'FAILED|error'"),
            ["cargo test -p app"]
        );
        assert_eq!(
            units("Get-Content a.log | Where-Object { $_ -match 'e' } | Select-Object -Last 5"),
            ["Get-Content a.log"]
        );
        assert_eq!(units("dir /b | findstr /i \".rs\""), ["dir /b"]);
        assert_eq!(units("x --check 2>&1 | more"), ["x --check"]);
        assert_eq!(units("cd /d C:\\app & dir"), ["dir"]);
        assert_eq!(
            units("Push-Location app; dotnet test > r.txt; Pop-Location"),
            ["dotnet test"]
        );
        assert_eq!(units("a >> out.log 2>&1"), ["a"]);
    }

    #[test]
    fn every_separator_splits() {
        for sep in ["&&", ";", "||", "&", "\n"] {
            assert_eq!(units(&format!("a {sep} b")), ["a", "b"], "{sep:?}");
        }
    }

    #[test]
    fn quoted_text_is_never_an_operator() {
        assert_eq!(units("echo 'a && b' | head"), ["echo 'a && b'"]);
        assert_eq!(units("echo \"a ; b\" > \"o u t\""), ["echo \"a ; b\""]);
        assert_eq!(units("echo 'x > y'"), ["echo 'x > y'"]);
    }

    #[test]
    fn environment_settings_stay_with_their_command() {
        assert_eq!(
            units("MODE=production npm test | head"),
            ["MODE=production npm test"]
        );
        assert_eq!(
            units("cd app; $env:MODE='x'; npm test"),
            ["$env:MODE='x'; npm test"]
        );
    }

    #[test]
    fn non_filter_pipelines_are_kept() {
        assert_eq!(units("cat a | sort"), ["cat a | sort"]);
    }
}
