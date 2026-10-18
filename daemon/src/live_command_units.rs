//! Reduce a shell command line to the commands it runs, for matching a live
//! agent's recipe against the recorded tool calls (RAL-682, RAL-685).
//!
//! One combined, quote-aware pass serves Bash, PowerShell and cmd alike -- it
//! does not try to tell the shells apart and does not need the optional
//! display-only shell parser. A command line becomes a list of [`Unit`]s:
//! * `&&`, `||`, `;`, `&` and line breaks separate units (line continuations
//!   `\`, `` ` `` and `^` join lines first);
//! * directory navigation (`cd`, `Set-Location`, ...) is dropped;
//! * output redirections (`> file`, `>> file`, `2>&1`, `*>&1`, ...) and
//!   output-only filters (`| head`, `| sort`, `| Out-Null`, ...) are variants:
//!   they are shown as written but never needed to match;
//! * wrappers (`timeout`, `sudo`, `nice`, `time`, `env -u`, `bash -c`, `( ... )`, ...)
//!   are variants of the command they wrap;
//! * environment settings (`MODE=x`, `$env:MODE='x'`, `export`) stay part of
//!   the unit they precede, so they are never silently lost;
//! * input-changing forms (`< file`, `yes |`) and failure masking
//!   (`|| true`, `; true`) or backgrounding (`&`) are part of the command.
//!
//! Every recognised form is a row of [`ALLOWLIST`] with an explicit
//! [`Category`]. A [`Category::Variant`] only decorates a command and may be
//! left out of a recipe. A [`Category::Standalone`] row is a command in its own
//! right (`source .venv/bin/activate`, `set -e`): a recipe may leave it out,
//! but one it includes is kept as written and only matches an identical
//! recorded command.
//!
//! Quoted text and text inside `(...)`/`{...}` is never an operator, and
//! anything the pass cannot read with certainty (heredocs, here-strings,
//! unterminated quotes, too-deep nesting) yields no units at all.

/// How an allowlisted form relates to the command around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Wrappers, filters and redirections: decoration a recipe may omit.
    Variant,
    /// A real command (activation, shell options): a recipe may omit it, but
    /// when it includes one the recorded command must be identical.
    Standalone,
}

const MAX_DEPTH: usize = 4;

/// What a wrapper row removed from the front of a command.
struct Strip {
    /// Leading words the wrapper (and its options) occupy.
    consumed: usize,
    /// The wrapper bounds the command (`timeout`): omitting it from a recipe
    /// would publish a less safe command, so it is restored in the shown text.
    retained: bool,
    /// Environment assignments the wrapper carried (`env A=1 cmd`).
    kept: Vec<String>,
    /// A command line the wrapper runs (`bash -c "..."`), matched piece by piece.
    payload: Option<String>,
}

impl Strip {
    fn plain(consumed: usize) -> Self {
        Self {
            consumed,
            retained: false,
            kept: Vec::new(),
            payload: None,
        }
    }
}

enum Form {
    /// A pipeline stage that only filters or displays output.
    Filter(fn(&[String]) -> bool),
    /// A prefix in front of the command it runs.
    Wrapper(fn(&[String]) -> Option<Strip>),
    /// A command that is meaningful on its own.
    Standalone(fn(&[String]) -> bool),
}

struct Row {
    names: &'static [&'static str],
    category: Category,
    form: Form,
}

macro_rules! row {
    ($cat:ident, $form:ident($f:expr), $($name:literal),+) => {
        Row { names: &[$($name),+], category: Category::$cat, form: Form::$form($f) }
    };
}

/// Every form that may be omitted or must be kept, one row per form. Later
/// additions are rows, not new code paths.
static ALLOWLIST: &[Row] = &[
    // -- output filters and display stages -------------------------------
    row!(
        Variant,
        Filter(any_args),
        "head",
        "tail",
        "grep",
        "egrep",
        "fgrep",
        "rg",
        "tee",
        "uniq",
        "wc",
        "cut",
        "jq",
        "less",
        "more",
        "findstr",
        "select-object",
        "where-object",
        "select-string",
        "out-string",
        "format-table",
        "format-list",
        "out-host",
        "out-file",
        "out-null",
        "tee-object",
        "measure-object",
        "sort-object"
    ),
    row!(Variant, Filter(no_args), "cat"),
    row!(Variant, Filter(sort_filter), "sort"),
    row!(Variant, Filter(sed_filter), "sed"),
    row!(Variant, Filter(awk_filter), "awk"),
    // -- process wrappers ---------------------------------------------------
    row!(Variant, Wrapper(wrap_timeout), "timeout", "gtimeout"),
    row!(Variant, Wrapper(wrap_sudo), "sudo", "doas"),
    row!(Variant, Wrapper(wrap_nice), "nice"),
    row!(Variant, Wrapper(wrap_ionice), "ionice"),
    row!(Variant, Wrapper(wrap_chrt), "chrt"),
    row!(Variant, Wrapper(wrap_time), "time"),
    row!(Variant, Wrapper(wrap_stdbuf), "stdbuf"),
    row!(Variant, Wrapper(wrap_unbuffer), "unbuffer"),
    row!(Variant, Wrapper(wrap_script), "script"),
    row!(Variant, Wrapper(wrap_env), "env"),
    row!(Variant, Wrapper(wrap_measure_command), "measure-command"),
    // -- shell invocations ----------------------------------------------
    row!(
        Variant,
        Wrapper(wrap_posix_shell),
        "bash",
        "sh",
        "zsh",
        "dash",
        "ksh"
    ),
    row!(Variant, Wrapper(wrap_powershell), "pwsh", "powershell"),
    row!(Variant, Wrapper(wrap_cmd), "cmd"),
    // -- shell option noise -----------------------------------------------
    row!(Standalone, Standalone(shell_option), "set"),
    row!(
        Standalone,
        Standalone(always),
        "$erroractionpreference",
        "@echo",
        "chcp",
        "clear",
        "cls"
    ),
    row!(Standalone, Standalone(echo_separator), "echo"),
    // -- activation and setup ---------------------------------------------
    row!(Standalone, Standalone(has_argument), "source", "."),
    row!(Standalone, Standalone(use_subcommand), "nvm", "fnm"),
    row!(
        Standalone,
        Standalone(activate_subcommand),
        "conda",
        "mamba",
        "micromamba"
    ),
    row!(Standalone, Standalone(call_setup_script), "call"),
    row!(
        Standalone,
        Standalone(always),
        "activate",
        "activate.ps1",
        "activate.bat"
    ),
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

/// cmd built-ins and common PowerShell verbs: their names are
/// case-insensitive, unlike an ordinary program's.
const CMD_BUILTINS: &[&str] = &[
    "dir", "type", "echo", "copy", "del", "erase", "md", "mkdir", "rd", "rmdir", "ren", "rename",
    "move", "cls", "set", "call", "start", "mklink", "ver", "vol", "path", "title", "tree",
    "findstr", "where",
];

const CMDLET_VERBS: &[&str] = &[
    "add",
    "clear",
    "compare",
    "convertfrom",
    "convertto",
    "copy",
    "enter",
    "exit",
    "export",
    "find",
    "format",
    "get",
    "group",
    "import",
    "install",
    "invoke",
    "join",
    "measure",
    "move",
    "new",
    "out",
    "pop",
    "push",
    "read",
    "register",
    "remove",
    "rename",
    "resolve",
    "restart",
    "select",
    "set",
    "show",
    "sort",
    "split",
    "start",
    "stop",
    "tee",
    "test",
    "unregister",
    "update",
    "use",
    "wait",
    "where",
    "write",
];

/// Commands whose arguments are text, where `\` is not a path separator.
const TEXT_COMMANDS: &[&str] = &["echo", "write-host", "write-output"];

fn find_row(name: &str, category: Category) -> Option<&'static Row> {
    ALLOWLIST
        .iter()
        .find(|r| r.category == category && r.names.contains(&name))
}

// ---- filters ---------------------------------------------------------------

fn any_args(_: &[String]) -> bool {
    true
}

fn no_args(words: &[String]) -> bool {
    words.len() == 1
}

fn sort_filter(words: &[String]) -> bool {
    !words
        .iter()
        .skip(1)
        .any(|a| a.starts_with("-o") || a.starts_with("--output"))
}

fn unquote(word: &str) -> String {
    let b = word.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        let inner = &word[1..word.len() - 1];
        if b[0] == b'"' {
            return inner.replace("\\\"", "\"");
        }
        return inner.to_string();
    }
    word.to_string()
}

/// `sed` that only reads: `-n`/`-E` options and an `-e` or positional script of
/// `p`/`d`/`q`/`=` and `s///flags` commands, no file arguments.
fn sed_filter(words: &[String]) -> bool {
    let mut script: Option<String> = None;
    let mut i = 1;
    while i < words.len() {
        let a = words[i].as_str();
        match a {
            "-n" | "-E" | "-r" | "-u" | "--quiet" | "--silent" | "--regexp-extended"
            | "--unbuffered" => {}
            "-e" | "--expression" => {
                i += 1;
                let Some(s) = words.get(i) else { return false };
                script = Some(match script {
                    Some(prev) => format!("{prev};{}", unquote(s)),
                    None => unquote(s),
                });
            }
            _ if a.starts_with('-') && a.len() > 1 => return false,
            _ => {
                if script.is_some() {
                    return false;
                }
                script = Some(unquote(a));
            }
        }
        i += 1;
    }
    script.is_some_and(|s| sed_script_reads_only(&s))
}

fn sed_script_reads_only(script: &str) -> bool {
    script.split(';').all(|part| {
        let part = part.trim();
        if part.is_empty() {
            return true;
        }
        let chars: Vec<char> = part.chars().collect();
        let mut i = 0;
        // Address: line numbers, `$`, `/re/`, ranges.
        loop {
            match chars.get(i) {
                Some(c) if c.is_ascii_digit() || *c == '$' => i += 1,
                Some('/') => {
                    i += 1;
                    while i < chars.len() && chars[i] != '/' {
                        if chars[i] == '\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    if i >= chars.len() {
                        return false;
                    }
                    i += 1;
                }
                _ => break,
            }
            if chars.get(i) == Some(&',') {
                i += 1;
            } else {
                break;
            }
        }
        while chars.get(i).is_some_and(|c| c.is_whitespace() || *c == '!') {
            i += 1;
        }
        match chars.get(i) {
            Some('p' | 'd' | 'q' | 'Q' | '=') => chars[i + 1..].iter().all(|c| c.is_whitespace()),
            Some('s') => {
                let Some(&delim) = chars.get(i + 1) else {
                    return false;
                };
                if delim.is_alphanumeric() || delim == '\\' || delim.is_whitespace() {
                    return false;
                }
                let mut j = i + 2;
                for _ in 0..2 {
                    while j < chars.len() && chars[j] != delim {
                        if chars[j] == '\\' {
                            j += 1;
                        }
                        j += 1;
                    }
                    if j >= chars.len() {
                        return false;
                    }
                    j += 1;
                }
                chars[j..]
                    .iter()
                    .all(|c| c.is_ascii_digit() || "gpiImM".contains(*c))
            }
            _ => false,
        }
    })
}

/// `awk` with a plain program: no output redirection, pipes, `system()` or
/// `getline`, no `-f` file, no file arguments.
fn awk_filter(words: &[String]) -> bool {
    let mut program: Option<String> = None;
    let mut i = 1;
    while i < words.len() {
        let a = words[i].as_str();
        if a == "-F" || a == "-v" {
            i += 2;
            continue;
        }
        if a.starts_with("-F") || (a.starts_with("-v") && a.len() > 2) {
            i += 1;
            continue;
        }
        if a.starts_with('-') && a.len() > 1 {
            return false;
        }
        if program.is_some() {
            return false;
        }
        program = Some(unquote(a));
        i += 1;
    }
    program.is_some_and(|p| {
        !["system", ">", "|", "getline", "close"]
            .iter()
            .any(|s| p.contains(s))
    })
}

// ---- wrappers ----------------------------------------------------------------

fn is_duration(s: &str) -> bool {
    let body = s.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(s);
    let mut parts = body.splitn(2, '.');
    let whole = parts.next().unwrap_or_default();
    let frac = parts.next();
    !whole.is_empty()
        && whole.chars().all(|c| c.is_ascii_digit())
        && frac.is_none_or(|f| !f.is_empty() && f.chars().all(|c| c.is_ascii_digit()))
}

fn is_command_word(word: Option<&String>) -> bool {
    word.is_some_and(|w| !w.starts_with('-') && !w.starts_with('/'))
}

fn wrap_timeout(w: &[String]) -> Option<Strip> {
    let mut i = 1;
    while let Some(a) = w.get(i) {
        match a.as_str() {
            "--preserve-status" | "--foreground" | "-v" | "--verbose" => i += 1,
            "-s" | "--signal" => i += 2,
            "-k" | "--kill-after" => {
                if !w.get(i + 1).is_some_and(|d| is_duration(d)) {
                    return None;
                }
                i += 2;
            }
            _ if a.starts_with("--signal=") || (a.starts_with("-s") && a.len() > 2) => i += 1,
            _ if a.starts_with("--kill-after=") => {
                if !is_duration(&a["--kill-after=".len()..]) {
                    return None;
                }
                i += 1;
            }
            _ if a.starts_with("-k") && a.len() > 2 => {
                if !is_duration(&a[2..]) {
                    return None;
                }
                i += 1;
            }
            _ => break,
        }
    }
    if !is_duration(w.get(i)?) || !is_command_word(w.get(i + 1)) {
        return None;
    }
    Some(Strip {
        retained: true,
        ..Strip::plain(i + 1)
    })
}

/// `sudo [-n] [-E] [-H] [-u user] [-g group] cmd`. Like `timeout`, the wrapper
/// is retained: a recipe may omit it, but the shown command restores it, and a
/// recipe that names one must name the identical wrapper (same user, same flags).
/// `-S`/`-i`/`-s`/`-l`/`-e` read input, start a shell or are not a run: not wrappers.
fn wrap_sudo(w: &[String]) -> Option<Strip> {
    let mut i = 1;
    while let Some(a) = w.get(i) {
        match a.as_str() {
            "-n" | "--non-interactive" | "-E" | "--preserve-env" | "-H" | "--set-home" | "--" => {
                i += 1;
            }
            "-u" | "--user" | "-g" | "--group" => {
                if !is_command_word(w.get(i + 1)) {
                    return None;
                }
                i += 2;
            }
            _ if a.starts_with("--user=") || a.starts_with("--group=") => i += 1,
            _ => break,
        }
    }
    is_command_word(w.get(i)).then(|| Strip {
        retained: true,
        ..Strip::plain(i)
    })
}

fn is_int(s: &str) -> bool {
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

fn wrap_nice(w: &[String]) -> Option<Strip> {
    let mut i = 1;
    while let Some(a) = w.get(i) {
        if a == "-n" || a == "--adjustment" {
            if !w.get(i + 1).is_some_and(|v| is_int(v)) {
                return None;
            }
            i += 2;
        } else if a.starts_with("--adjustment=")
            || (a.starts_with("-n") && is_int(&a[2..]))
            || (a.len() > 1 && a.starts_with('-') && is_int(&a[1..]))
        {
            i += 1;
        } else {
            break;
        }
    }
    is_command_word(w.get(i)).then(|| Strip::plain(i))
}

fn wrap_ionice(w: &[String]) -> Option<Strip> {
    let mut i = 1;
    while let Some(a) = w.get(i) {
        match a.as_str() {
            "-c" | "-n" | "--class" | "--classdata" => i += 2,
            "-t" | "--ignore" => i += 1,
            _ if (a.starts_with("-c") || a.starts_with("-n")) && is_int(&a[2..]) => i += 1,
            _ => break,
        }
    }
    is_command_word(w.get(i)).then(|| Strip::plain(i))
}

fn wrap_chrt(w: &[String]) -> Option<Strip> {
    let mut i = 1;
    while let Some(a) = w.get(i) {
        match a.as_str() {
            "-f" | "-r" | "-o" | "-b" | "-i" | "-d" | "-R" | "-v" | "--fifo" | "--rr"
            | "--other" | "--batch" | "--idle" | "--reset-on-fork" => i += 1,
            _ => break,
        }
    }
    if i == 1
        || !w
            .get(i)
            .is_some_and(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())
    {
        return None;
    }
    is_command_word(w.get(i + 1)).then(|| Strip::plain(i + 1))
}

fn wrap_time(w: &[String]) -> Option<Strip> {
    let i = if w.get(1).is_some_and(|a| a == "-p") {
        2
    } else {
        1
    };
    is_command_word(w.get(i)).then(|| Strip::plain(i))
}

fn wrap_stdbuf(w: &[String]) -> Option<Strip> {
    let mut i = 1;
    while let Some(a) = w.get(i) {
        match a.as_str() {
            "-i" | "-o" | "-e" => i += 2,
            _ if a.starts_with("--input=")
                || a.starts_with("--output=")
                || a.starts_with("--error=") =>
            {
                i += 1;
            }
            _ if ["-i", "-o", "-e"].iter().any(|p| a.starts_with(p)) && a.len() > 2 => i += 1,
            _ => break,
        }
    }
    (i > 1 && is_command_word(w.get(i))).then(|| Strip::plain(i))
}

fn wrap_unbuffer(w: &[String]) -> Option<Strip> {
    let i = if w.get(1).is_some_and(|a| a == "-p") {
        2
    } else {
        1
    };
    is_command_word(w.get(i)).then(|| Strip::plain(i))
}

/// `script -q /dev/null cmd args` and `script -q -c "cmd" /dev/null`.
fn wrap_script(w: &[String]) -> Option<Strip> {
    let mut i = 1;
    let mut quiet = false;
    let mut payload = None;
    while let Some(a) = w.get(i) {
        match a.as_str() {
            "-q" | "--quiet" => {
                quiet = true;
                i += 1;
            }
            "-e" | "-f" | "-a" | "--return" | "--flush" | "--append" => i += 1,
            "-c" | "--command" => {
                payload = Some(unquote(w.get(i + 1)?));
                i += 2;
            }
            _ => break,
        }
    }
    if !quiet {
        return None;
    }
    match payload {
        Some(payload) => {
            // Optional typescript file, then nothing else.
            if w.len() > i + 1 {
                return None;
            }
            Some(Strip {
                payload: Some(payload),
                ..Strip::plain(w.len())
            })
        }
        None => {
            w.get(i)?;
            is_command_word(w.get(i + 1)).then(|| Strip::plain(i + 1))
        }
    }
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn wrap_env(w: &[String]) -> Option<Strip> {
    let mut i = 1;
    let mut kept = Vec::new();
    while let Some(a) = w.get(i) {
        match a.as_str() {
            "-i" | "--ignore-environment" | "-" | "--" => i += 1,
            "-u" | "--unset" => i += 2,
            _ if a.starts_with("--unset=") => i += 1,
            _ if is_assignment(a) => {
                kept.push(a.clone());
                i += 1;
            }
            _ => break,
        }
    }
    is_command_word(w.get(i)).then(|| Strip {
        kept,
        ..Strip::plain(i)
    })
}

fn is_group(word: &str) -> bool {
    word.len() >= 2
        && ((word.starts_with('(') && word.ends_with(')'))
            || (word.starts_with('{') && word.ends_with('}')))
}

fn group_payload(word: &str) -> String {
    word[1..word.len() - 1]
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_string()
}

fn wrap_measure_command(w: &[String]) -> Option<Strip> {
    (w.len() == 2 && w[1].starts_with('{') && is_group(&w[1])).then(|| Strip {
        payload: Some(group_payload(&w[1])),
        ..Strip::plain(2)
    })
}

fn wrap_posix_shell(w: &[String]) -> Option<Strip> {
    (w.len() == 3 && matches!(w[1].as_str(), "-c" | "-lc" | "-cl")).then(|| Strip {
        payload: Some(unquote(&w[2])),
        ..Strip::plain(3)
    })
}

fn joined_payload(words: &[String]) -> Option<String> {
    match words {
        [] => None,
        [one] => Some(unquote(one)),
        many => Some(many.join(" ")),
    }
}

fn wrap_powershell(w: &[String]) -> Option<Strip> {
    let mut i = 1;
    while w.get(i).is_some_and(|a| {
        a.eq_ignore_ascii_case("-NoProfile") || a.eq_ignore_ascii_case("-NonInteractive")
    }) {
        i += 1;
    }
    if !w.get(i).is_some_and(|a| a.eq_ignore_ascii_case("-Command")) {
        return None;
    }
    Some(Strip {
        payload: Some(joined_payload(&w[i + 1..])?),
        ..Strip::plain(w.len())
    })
}

fn wrap_cmd(w: &[String]) -> Option<Strip> {
    let mut i = 1;
    if w.get(i).is_some_and(|a| a.eq_ignore_ascii_case("/d")) {
        i += 1;
    }
    if !w.get(i).is_some_and(|a| a.eq_ignore_ascii_case("/c")) {
        return None;
    }
    Some(Strip {
        payload: Some(joined_payload(&w[i + 1..])?),
        ..Strip::plain(w.len())
    })
}

// ---- standalone commands ---------------------------------------------------------

fn always(_: &[String]) -> bool {
    true
}

fn has_argument(w: &[String]) -> bool {
    w.len() >= 2
}

fn shell_option(w: &[String]) -> bool {
    w.get(1).is_some_and(|a| a.starts_with(['-', '+']))
}

fn separator_like(arg: &str) -> bool {
    let arg = unquote(arg);
    arg.len() >= 2 && arg.chars().all(|c| "-=_*#~.".contains(c))
}

fn echo_separator(w: &[String]) -> bool {
    w.len() > 1 && w[1..].iter().all(|a| separator_like(a))
}

fn use_subcommand(w: &[String]) -> bool {
    w.get(1).is_some_and(|a| a == "use")
}

fn activate_subcommand(w: &[String]) -> bool {
    w.get(1).is_some_and(|a| a == "activate")
}

fn base_name_lower(word: &str) -> String {
    word.rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn call_setup_script(w: &[String]) -> bool {
    w.get(1).is_some_and(|t| {
        let base = base_name_lower(&unquote(t));
        ["vcvars", "vsdevcmd", "activate"]
            .iter()
            .any(|p| base.starts_with(p))
    })
}

/// A script named `activate`/`Activate.ps1` run by path.
fn activation_path(word: &str) -> bool {
    matches!(
        base_name_lower(&unquote(word)).as_str(),
        "activate" | "activate.ps1" | "activate.bat"
    )
}

// ---- tokens ----------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sep {
    And,
    Or,
    Semi,
    Amp,
}

impl Sep {
    fn text(self) -> &'static str {
        match self {
            Sep::And => "&&",
            Sep::Or => "||",
            Sep::Semi => ";",
            Sep::Amp => "&",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ShellTok {
    Word(String),
    Separator(Sep),
    Pipe,
}

/// `None` for an unterminated quote or bracket.
fn shell_tokens(command: &str) -> Option<Vec<ShellTok>> {
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
    let separate = |out: &mut Vec<ShellTok>, sep: Sep| {
        if !matches!(out.last(), Some(ShellTok::Separator(_))) {
            out.push(ShellTok::Separator(sep));
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
        if matches!(c, '\\' | '`' | '^') && matches!(next, Some('\n' | '\r')) {
            i += 1;
            if chars[i] == '\r' && chars.get(i + 1) == Some(&'\n') {
                i += 1;
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
                separate(&mut out, Sep::Semi);
            }
            c if c.is_whitespace() => flush(&mut out, &mut cur, &mut has_word),
            '&' => {
                if next == Some('&') {
                    flush(&mut out, &mut cur, &mut has_word);
                    separate(&mut out, Sep::And);
                    i += 1;
                } else if cur.ends_with(['>', '<']) || next == Some('>') {
                    // `2>&1`, `>&2`, `&>file`: part of a redirection.
                    cur.push(c);
                    has_word = true;
                } else {
                    flush(&mut out, &mut cur, &mut has_word);
                    separate(&mut out, Sep::Amp);
                }
            }
            '|' => {
                flush(&mut out, &mut cur, &mut has_word);
                if next == Some('|') {
                    separate(&mut out, Sep::Or);
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
    if quote.is_some() || depth > 0 {
        return None;
    }
    flush(&mut out, &mut cur, &mut has_word);
    Some(out)
}

enum Redir {
    None,
    /// `<<`, `<<<`, `<(`: carries content, so the command is not read.
    Content,
    /// Dropped output redirection; `true` when the next word is its target.
    Output(bool),
    /// Input redirection, which changes what the command reads and is kept.
    Input(bool),
}

fn redirection(word: &str) -> Redir {
    let rest = word.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&' || c == '*');
    if rest.starts_with("<<") || rest.starts_with("<(") {
        return Redir::Content;
    }
    if let Some(r) = rest.strip_prefix('<') {
        return Redir::Input(r.is_empty());
    }
    match rest.strip_prefix(">>").or_else(|| rest.strip_prefix('>')) {
        Some(r) => Redir::Output(r.is_empty()),
        None => Redir::None,
    }
}

// ---- words -----------------------------------------------------------------------

/// The name a command is looked up under: lower-case, without `.exe`.
fn lookup_name(word: &str) -> String {
    let lower = word.to_ascii_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

fn is_cmdlet(lower: &str) -> bool {
    lower.split_once('-').is_some_and(|(verb, noun)| {
        CMDLET_VERBS.contains(&verb)
            && !noun.is_empty()
            && noun.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

/// Comparison form of a command name, and whether it is Windows-style.
fn command_name(word: &str) -> (String, bool) {
    let mut name = word;
    let mut windows = false;
    if name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
        name = &name[..name.len() - 4];
        windows = true;
    }
    let lower = name.to_ascii_lowercase();
    if is_cmdlet(&lower) || CMD_BUILTINS.contains(&lower.as_str()) {
        return (lower, true);
    }
    if [".ps1", ".bat", ".cmd"].iter().any(|e| lower.ends_with(e)) || name.contains('\\') {
        windows = true;
    }
    (name.to_string(), windows)
}

fn plain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_./:=,+@%^-".contains(c)
}

/// One quoted word reduced to its canonical quoting. `'$HOME'` and `"$HOME"`
/// stay different; `"a b"` and `'a b'` are the same.
fn canonical_quotes(word: &str) -> String {
    let b = word.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        let inner = &word[1..word.len() - 1];
        let safe = !inner.contains(['\'', '"'])
            && (b[0] == b'\'' || !inner.contains(['$', '`', '\\', '!']));
        if safe {
            if !inner.is_empty() && inner.chars().all(plain_char) {
                return inner.to_string();
            }
            return format!("'{inner}'");
        }
    }
    word.to_string()
}

/// Comparison text for a command's words: command name case/`.exe`-normalised
/// where the shell ignores them, path separators unified for Windows-style
/// commands, quoting canonicalised.
fn normalize_words(words: &[String]) -> String {
    let Some(first) = words.first() else {
        return String::new();
    };
    let (name, windows) = command_name(first);
    let windows = windows && !TEXT_COMMANDS.contains(&name.as_str());
    let fix = |w: &str| {
        if windows && !w.contains(['$', '`']) {
            w.replace('\\', "/")
        } else {
            w.to_string()
        }
    };
    let mut out = vec![canonical_quotes(&fix(&name))];
    out.extend(words[1..].iter().map(|w| canonical_quotes(&fix(w))));
    out.join(" ")
}

fn leading_assignments(words: &[String]) -> usize {
    words.iter().take_while(|w| is_assignment(w)).count()
}

// ---- units -----------------------------------------------------------------------

/// One command of a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// Environment settings that precede the command, as written (`A=1; `).
    prefix: String,
    /// The command's first pipeline stage, as written.
    first: Vec<String>,
    /// Leading environment-assignment words in `first`.
    env_len: usize,
    /// The rest of the pipeline and any failure-masking/background suffix, as written.
    tail: String,
    /// What it is compared by: variants removed, text normalised.
    key: String,
    standalone: bool,
    /// A bounding wrapper (`timeout 100`) the command ran under.
    retained: Option<String>,
}

impl Unit {
    /// The command as written (what a recipe that included it shows).
    #[must_use]
    pub fn text(&self) -> String {
        self.render(None)
    }

    /// The comparison key; variants a recipe may leave out are not in it.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Whether the command is meaningful on its own (activation, shell options).
    #[must_use]
    pub fn is_standalone(&self) -> bool {
        self.standalone
    }

    fn render(&self, insert: Option<&str>) -> String {
        let mut words: Vec<&str> = self.first.iter().map(String::as_str).collect();
        if let Some(extra) = insert {
            words.insert(self.env_len.min(words.len()), extra);
        }
        format!("{}{}{}", self.prefix, words.join(" "), self.tail)
    }

    /// Whether this (recommended) unit stands for `recorded`: same command, and
    /// a bounding wrapper it names is the one that ran.
    #[must_use]
    pub fn matches(&self, recorded: &Unit) -> bool {
        self.key == recorded.key
            && self.standalone == recorded.standalone
            && (self.retained.is_none() || self.retained == recorded.retained)
    }

    /// The command to show for this recommendation matched to `recorded`: as
    /// written, with a bounding wrapper it left out restored.
    #[must_use]
    pub fn shown_for(&self, recorded: &Unit) -> String {
        match (&self.retained, &recorded.retained) {
            (None, Some(wrapper)) => self.render(Some(wrapper)),
            _ => self.text(),
        }
    }
}

#[derive(Default, Clone)]
struct Stage {
    /// Every word, redirections included.
    all: Vec<String>,
    /// Words that are part of the command (output redirections removed).
    core: Vec<String>,
}

struct Segment {
    sep_before: Sep,
    stages: Vec<Stage>,
}

fn is_environment_setting(core: &[String]) -> bool {
    let first = core
        .first()
        .map(|w| w.to_ascii_lowercase())
        .unwrap_or_default();
    if first == "set" && shell_option(core) {
        return false;
    }
    if first.starts_with("$env:") || ["export", "set", "setx"].contains(&first.as_str()) {
        return true;
    }
    core.iter().all(|w| is_assignment(w))
}

fn is_filter(stage: &Stage) -> bool {
    let Some(first) = stage.core.first() else {
        return false;
    };
    match find_row(&lookup_name(first), Category::Variant) {
        Some(Row {
            form: Form::Filter(check),
            ..
        }) => check(&stage.core),
        _ => false,
    }
}

fn is_standalone_command(words: &[String]) -> bool {
    let Some(first) = words.first() else {
        return false;
    };
    let name = lookup_name(first);
    if name.starts_with("$erroractionpreference") {
        return true;
    }
    if activation_path(first) {
        return true;
    }
    matches!(
        find_row(&name, Category::Standalone),
        Some(Row { form: Form::Standalone(check), .. }) if check(words)
    )
}

fn masks_failure(core: &[String]) -> bool {
    let lower: Vec<String> = core.iter().map(|w| w.to_ascii_lowercase()).collect();
    matches!(
        lower
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        ["true"] | [":"] | ["exit", "0"]
    )
}

enum Analysis {
    Navigation,
    Command(Box<Command>),
    /// The units a wrapper's payload expands to.
    Unwrapped(Vec<Unit>),
}

struct Command {
    key: String,
    standalone: bool,
    retained: Option<String>,
}

fn analyze(head: &Stage, allow_unwrap: bool, depth: usize) -> Option<Analysis> {
    let core = &head.core;
    let lead = leading_assignments(core);
    if lead == 0 && core.len() == 1 && is_group(&core[0]) && allow_unwrap {
        return unwrap_payload(&group_payload(&core[0]), depth);
    }
    let mut idx = lead;
    let mut retained: Option<String> = None;
    let mut kept: Vec<String> = Vec::new();
    while idx < core.len() {
        let words = &core[idx..];
        let Some(Row {
            form: Form::Wrapper(strip),
            ..
        }) = find_row(&lookup_name(&words[0]), Category::Variant)
        else {
            break;
        };
        let Some(s) = strip(words) else { break };
        if let Some(payload) = s.payload {
            if lead > 0 || retained.is_some() || !kept.is_empty() || !allow_unwrap {
                break;
            }
            return unwrap_payload(&payload, depth);
        }
        if s.retained {
            let text = words[..s.consumed].join(" ");
            retained = Some(match retained {
                Some(prev) => format!("{prev} {text}"),
                None => text,
            });
        }
        kept.extend(s.kept);
        idx += s.consumed;
    }
    let words = &core[idx..];
    let first = words.first()?;
    if NAVIGATION_COMMANDS.contains(&lookup_name(first).as_str()) {
        return Some(Analysis::Navigation);
    }
    let standalone = is_standalone_command(words);
    let mut key_words: Vec<String> = core[..lead].to_vec();
    key_words.extend(kept);
    let mut key = normalize_words(words);
    if standalone && words[0] == "." {
        key = format!("source{}", &key[1..]);
    }
    if !key_words.is_empty() {
        key = format!("{} {key}", key_words.join(" "));
    }
    Some(Analysis::Command(Box::new(Command {
        key,
        standalone,
        retained,
    })))
}

fn unwrap_payload(payload: &str, depth: usize) -> Option<Analysis> {
    Some(Analysis::Unwrapped(units_of(payload, depth + 1)?))
}

fn units_of(command: &str, depth: usize) -> Option<Vec<Unit>> {
    if depth > MAX_DEPTH {
        return None;
    }
    let mut segments = vec![Segment {
        sep_before: Sep::Semi,
        stages: vec![Stage::default()],
    }];
    let mut skip: Option<bool> = None;
    for tok in shell_tokens(command)? {
        match tok {
            ShellTok::Separator(sep) => {
                skip = None;
                segments.push(Segment {
                    sep_before: sep,
                    stages: vec![Stage::default()],
                });
            }
            ShellTok::Pipe => {
                skip = None;
                segments.last_mut()?.stages.push(Stage::default());
            }
            ShellTok::Word(word) => {
                if word.starts_with("@'") || word.starts_with("@\"") {
                    return None;
                }
                let stage = segments.last_mut()?.stages.last_mut()?;
                if let Some(keep) = skip.take() {
                    stage.all.push(word.clone());
                    if keep {
                        stage.core.push(word);
                    }
                    continue;
                }
                match redirection(&word) {
                    Redir::Content => return None,
                    Redir::None => {
                        stage.all.push(word.clone());
                        stage.core.push(word);
                    }
                    Redir::Output(target) => {
                        stage.all.push(word);
                        skip = target.then_some(false);
                    }
                    Redir::Input(target) => {
                        stage.all.push(word.clone());
                        stage.core.push(word);
                        skip = target.then_some(true);
                    }
                }
            }
        }
    }
    let mut units: Vec<Unit> = Vec::new();
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut previous: Option<usize> = None;
    let backgrounded: Vec<bool> = segments
        .iter()
        .skip(1)
        .map(|s| s.sep_before == Sep::Amp)
        .chain(std::iter::once(false))
        .collect();
    for (k, seg) in segments.iter().enumerate() {
        let before = previous.take();
        let stages: Vec<&Stage> = seg.stages.iter().filter(|s| !s.core.is_empty()).collect();
        let Some(head) = stages.first() else { continue };
        if stages.len() == 1
            && masks_failure(&head.core)
            && matches!(seg.sep_before, Sep::Or | Sep::Semi)
        {
            if let Some(u) = before {
                let suffix = format!(" {} {}", seg.sep_before.text(), head.all.join(" "));
                units[u].tail.push_str(&suffix);
                units[u].key.push_str(&suffix);
                previous = Some(u);
                continue;
            }
        }
        if is_environment_setting(&head.core) {
            pending.push((head.all.join(" "), normalize_words(&head.core)));
            continue;
        }
        let tail_stages = &stages[1..];
        let tail_filters = tail_stages.iter().all(|s| is_filter(s));
        let tail_visible: String = tail_stages
            .iter()
            .map(|s| format!(" | {}", s.all.join(" ")))
            .collect();
        let suffix = if backgrounded[k] { " &" } else { "" };
        match analyze(head, tail_filters && pending.is_empty(), depth)? {
            Analysis::Navigation => {}
            Analysis::Unwrapped(mut inner) => {
                if let [only] = inner.as_mut_slice() {
                    if only.retained.is_none() {
                        only.first = head.all.clone();
                        only.env_len = 0;
                        only.prefix.clear();
                        only.tail = tail_visible;
                    }
                }
                if let Some(last) = inner.last_mut() {
                    last.tail.push_str(suffix);
                    last.key.push_str(suffix);
                }
                units.extend(inner);
                previous = (!units.is_empty()).then_some(units.len().wrapping_sub(1));
            }
            Analysis::Command(cmd) => {
                let Command {
                    mut key,
                    standalone,
                    retained,
                } = *cmd;
                for stage in tail_stages {
                    if !is_filter(stage) {
                        key.push_str(" | ");
                        key.push_str(&normalize_words(&stage.core));
                    }
                }
                key.push_str(suffix);
                let mut prefix = String::new();
                if !standalone && !pending.is_empty() {
                    let visible: Vec<&str> = pending.iter().map(|p| p.0.as_str()).collect();
                    let keys: Vec<&str> = pending.iter().map(|p| p.1.as_str()).collect();
                    prefix = format!("{}; ", visible.join("; "));
                    key = format!("{}; {key}", keys.join("; "));
                    pending.clear();
                }
                units.push(Unit {
                    prefix,
                    env_len: leading_assignments(&head.all),
                    first: head.all.clone(),
                    tail: format!("{tail_visible}{suffix}"),
                    key,
                    standalone,
                    retained,
                });
                previous = Some(units.len() - 1);
            }
        }
    }
    Some(units)
}

/// The commands a command line is made of; see the module docs. Empty when the
/// line cannot be read with certainty.
#[must_use]
pub fn command_units(command: &str) -> Vec<Unit> {
    units_of(command, 0).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{Category, command_units};

    fn keys(command: &str) -> Vec<String> {
        command_units(command)
            .iter()
            .map(|u| u.key().to_string())
            .collect()
    }

    fn texts(command: &str) -> Vec<String> {
        command_units(command)
            .iter()
            .map(super::Unit::text)
            .collect()
    }

    /// Whether a recipe `proposed` stands for a unit of `recorded`.
    fn matches(proposed: &str, recorded: &str) -> bool {
        let rec = command_units(recorded);
        let prop = command_units(proposed);
        !prop.is_empty() && prop.iter().all(|p| rec.iter().any(|r| p.matches(r)))
    }

    #[test]
    fn filters_navigation_and_redirection_are_dropped() {
        assert_eq!(
            keys("fizz buzz --count 300 | head -n 10"),
            ["fizz buzz --count 300"]
        );
        assert_eq!(
            keys("cargo test -p app 2>&1 | grep -E 'FAILED|error'"),
            ["cargo test -p app"]
        );
        assert_eq!(
            keys("Get-Content a.log | Where-Object { $_ -match 'e' } | Select-Object -Last 5"),
            ["get-content a.log"]
        );
        assert_eq!(keys("dir /b | findstr /i \".rs\""), ["dir /b"]);
        assert_eq!(keys("x --check 2>&1 | more"), ["x --check"]);
        assert_eq!(keys("cd /d C:\\app & dir"), ["dir"]);
        assert_eq!(
            keys("Push-Location app; dotnet test > r.txt; Pop-Location"),
            ["dotnet test"]
        );
        assert_eq!(keys("a >> out.log 2>&1"), ["a"]);
    }

    #[test]
    fn what_the_recipe_wrote_is_shown_as_written() {
        assert_eq!(
            texts("fizz buzz --count 300 | head -n 10"),
            ["fizz buzz --count 300 | head -n 10"]
        );
        assert_eq!(texts("a >> out.log 2>&1"), ["a >> out.log 2>&1"]);
        assert_eq!(texts("cd app && npm test"), ["npm test"]);
        assert_eq!(texts("time  nice -n 5   foo"), ["time nice -n 5 foo"]);
    }

    #[test]
    fn every_separator_splits() {
        for sep in ["&&", ";", "||", "\n"] {
            assert_eq!(keys(&format!("a {sep} b")), ["a", "b"], "{sep:?}");
        }
        assert_eq!(keys("a & b"), ["a &", "b"]);
    }

    #[test]
    fn quoted_text_is_never_an_operator() {
        assert_eq!(keys("echo 'a && b' | head"), ["echo 'a && b'"]);
        assert_eq!(keys("echo \"a ; b\" > \"o u t\""), ["echo 'a ; b'"]);
        assert_eq!(keys("echo 'x > y'"), ["echo 'x > y'"]);
        assert_eq!(keys("echo 'foo | head'"), ["echo 'foo | head'"]);
        assert!(!matches("foo", "\"timeout 100 foo\""));
        assert!(!matches("foo", "echo 'x' '| sort'"));
        assert!(!matches("echo a", "echo 'a && b'"));
    }

    #[test]
    fn environment_settings_stay_with_their_command() {
        assert_eq!(
            keys("MODE=production npm test | head"),
            ["MODE=production npm test"]
        );
        assert_eq!(
            keys("cd app; $env:MODE='x'; npm test"),
            ["$env:MODE='x'; npm test"]
        );
        assert!(!matches("npm test", "MODE=production npm test"));
        assert!(!matches("npm test", "env MODE=production npm test"));
        assert!(matches(
            "MODE=production npm test",
            "env MODE=production npm test"
        ));
        assert!(!matches("npm test", "export MODE=1; npm test"));
        assert!(!matches("foo", "FOO=1 timeout 5 foo"));
        assert!(matches("FOO=1 foo", "FOO=1 timeout 5 foo"));
    }

    #[test]
    fn standalone_commands_may_be_omitted_but_are_kept_when_included() {
        assert!(matches("app run", "source foo && app run"));
        assert!(matches("source foo && app run", "source foo && app run"));
        assert!(!matches("source bar && app run", "source foo && app run"));
        assert!(!matches("source foo", "app run"));
        let units = command_units("source foo && app run");
        assert!(units[0].is_standalone() && !units[1].is_standalone());
        assert_eq!(texts("source foo && app run"), ["source foo", "app run"]);
        assert!(!matches("set -e", "set +e"));
        assert!(matches("set -e", "set -e; foo"));
        // A standalone command is not mistaken for a variant of a normal one.
        assert!(!matches("source", "source foo"));
    }

    #[test]
    fn near_misses_are_not_allowlisted() {
        assert!(!matches("foo --bar", "foo --baz"));
        assert!(!matches("foo", "sudo -i foo"));
        assert!(!matches("foo", "runas /user:x foo"));
        assert!(!matches("foo", "foo &"));
        assert!(!matches("foo", "foo || true"));
        assert!(!matches("foo", "foo; true"));
        assert!(!matches("foo", "foo || :"));
        assert!(matches("foo || true", "foo || true"));
        assert!(!matches("foo", "cd /tmp2 && ./foo"));
        assert!(!matches("jest", "./node_modules/.bin/jest"));
        assert!(!matches("foo", "yes | foo"));
        assert!(!matches("foo", "foo < /dev/null"));
        assert!(!matches("foo", "foo < input.txt"));
        assert!(matches("foo < input.txt", "foo < input.txt"));
        assert!(matches("foo", "foo | sort"));
    }

    #[test]
    fn timeout_wraps_without_changing_the_command() {
        for rec in [
            "timeout 100 foo --bar",
            "gtimeout 100 foo --bar",
            "timeout -s KILL 5.5s foo --bar",
            "timeout --signal=TERM -k 10 2m foo --bar",
            "timeout --preserve-status --foreground -v 1h foo --bar",
            "timeout -k5 3d foo --bar",
            "timeout 10 foo --bar | head",
        ] {
            assert!(matches("foo --bar", rec), "{rec}");
            let unit = &command_units("foo --bar")[0];
            let shown = unit.shown_for(&command_units(rec)[0]);
            assert!(
                shown.starts_with(rec.split(" foo").next().unwrap()),
                "{shown}"
            );
        }
        assert_eq!(
            command_units("foo --bar")[0].shown_for(&command_units("timeout 100 foo --bar")[0]),
            "timeout 100 foo --bar"
        );
        assert_eq!(
            command_units("A=1 foo")[0].shown_for(&command_units("A=1 timeout 5 foo")[0]),
            "A=1 timeout 5 foo"
        );
        // The bound the recipe names must be the one that ran.
        assert!(matches("timeout 100 foo", "timeout 100 foo"));
        assert!(!matches("timeout 50 foo", "timeout 100 foo"));
        assert!(!matches("timeout 100 foo", "foo"));
        // Not wrappers: sleeps, bare durations, quoted text, bad options.
        assert!(!matches("foo", "timeout /t 5"));
        assert_eq!(keys("timeout 5"), ["timeout 5"]);
        assert_eq!(keys("timeout /t 5 /nobreak"), ["timeout /t 5 /nobreak"]);
        assert!(!matches("foo", "timeout 5x foo"));
        assert!(!matches("foo", "timeout -x 5 foo"));
    }

    #[test]
    fn process_wrappers_are_variants() {
        for rec in [
            "nice foo",
            "nice -n 10 foo",
            "nice -10 foo",
            "ionice -c 3 foo",
            "ionice -c3 -n 7 foo",
            "chrt -f 10 foo",
            "chrt -o 0 foo",
            "time foo",
            "time -p foo",
            "stdbuf -oL foo",
            "stdbuf -o L -e0 foo",
            "unbuffer foo",
            "script -q /dev/null foo",
            "script -q -c foo /dev/null",
            "env -u HOME foo",
            "env -i foo",
            "Measure-Command { foo }",
            "nice -n 5 time stdbuf -oL foo",
        ] {
            assert!(matches("foo", rec), "{rec}");
        }
        // The option value is never read as the wrapped command.
        assert_eq!(keys("nice -n 10 foo"), ["foo"]);
        assert!(!matches("10 foo", "nice -n 10 foo"));
        // Not wrappers.
        assert!(!matches("foo", "nice"));
        assert!(!matches("foo", "time /t"));
        assert!(!matches("foo", "chrt -p 1234"));
        assert!(!matches("foo", "env"));
        assert!(!matches("foo", "script foo"));
        // A wrapper the recipe includes is kept as written.
        assert_eq!(texts("time foo"), ["time foo"]);
    }

    #[test]
    fn output_filters_and_redirections_expand() {
        for rec in [
            "foo | sort",
            "foo | sort -u",
            "foo | uniq -c",
            "foo | wc -l",
            "foo | cut -d, -f1",
            "foo | jq .a",
            "foo | less",
            "foo | cat",
            "foo | Out-String",
            "foo | Format-Table -AutoSize",
            "foo | Out-Host",
            "foo | Out-File x.txt",
            "foo | Out-Null",
            "foo | Tee-Object -FilePath x",
            "foo | Sort-Object Name",
            "foo | Measure-Object",
            "foo | sed -n '1,5p'",
            "foo | sed 's/a/b/g'",
            "foo | sed -e 's/a/b/' -e '/x/d'",
            "foo | awk '{print $1}'",
            "foo | awk -F, '{print $2}'",
            "foo >> out.log",
            "foo &> out.log",
            "foo |& tee x",
            "foo 2>/dev/null",
            "foo >NUL 2>&1",
            "foo *>&1",
            "foo | head | sort | uniq",
        ] {
            assert!(matches("foo", rec), "{rec}");
        }
        for rec in [
            "foo | sed -i 's/a/b/'",
            "foo | sed 's/a/b/w out'",
            "foo | sed 's/a/b/e'",
            "foo | sed -f script.sed",
            "foo | sed 'w out'",
            "foo | sed p file",
            "foo | awk '{print > \"x\"}'",
            "foo | awk '{system(\"rm x\")}'",
            "foo | awk '{print | \"sh\"}'",
            "foo | awk -f prog.awk",
            "foo | sort -o out.txt",
            "foo | cat file",
            "foo | bar",
        ] {
            assert!(!matches("foo", rec), "{rec}");
        }
    }

    #[test]
    fn shell_option_noise_is_standalone() {
        for rec in [
            "set -e && foo",
            "set -o pipefail; foo",
            "$ErrorActionPreference = 'Stop'; foo",
            "@echo off\nfoo",
            "chcp 65001 >NUL & foo",
            "clear; foo",
            "cls & foo",
            "echo --- ; foo",
            "echo '=====' && foo",
        ] {
            assert!(matches("foo", rec), "{rec}");
            assert!(command_units(rec)[0].is_standalone(), "{rec}");
        }
        assert!(matches("set -e && foo", "set -e && foo"));
        assert!(!matches("set -u && foo", "set -e && foo; bar"));
        // Real output and cmd's `set` are not noise.
        assert!(!command_units("echo hello")[0].is_standalone());
        assert!(!command_units("echo")[0].is_standalone());
        assert!(!matches("foo", "set X=1 & foo"));
    }

    #[test]
    fn activation_commands_are_standalone() {
        for rec in [
            "source .venv/bin/activate && pytest",
            ". .venv/bin/activate; pytest",
            "nvm use 20 && pytest",
            "conda activate base && pytest",
            ". .\\.venv\\Scripts\\Activate.ps1; pytest",
            ".venv\\Scripts\\activate.bat && pytest",
            "call vcvars64.bat && pytest",
        ] {
            assert!(matches("pytest", rec), "{rec}");
            assert!(command_units(rec)[0].is_standalone(), "{rec}");
        }
        assert!(matches(
            "source .venv/bin/activate && pytest",
            "source .venv/bin/activate && pytest"
        ));
        assert!(matches("source x/activate", ". x/activate"));
        assert!(matches(". x/activate", "source x/activate"));
        assert_eq!(texts(". x/activate && pytest"), [". x/activate", "pytest"]);
        assert!(!matches(
            "source .venv/bin/activate",
            "source other/bin/activate"
        ));
        assert!(!matches("nvm use 18", "nvm use 20"));
        assert!(!matches("conda activate b", "conda activate a"));
        // A build script run with `call` is a command, not setup.
        assert!(!command_units("call build.bat")[0].is_standalone());
    }

    #[test]
    fn shell_invocations_unwrap_piece_by_piece() {
        for rec in [
            "bash -c 'foo'",
            "sh -lc \"foo\"",
            "bash -cl 'foo'",
            "pwsh -NoProfile -NonInteractive -Command \"foo\"",
            "powershell -Command foo",
            "cmd /c foo",
            "cmd /d /c \"foo\"",
            "(foo)",
            "{ foo; }",
            "bash -c '(foo)'",
        ] {
            assert!(matches("foo", rec), "{rec}");
        }
        assert!(matches("a", "bash -c 'a && b'") && matches("b", "bash -c 'a && b'"));
        assert!(matches("a && b", "(a; b)"));
        assert!(matches("foo", "bash -c \"foo | head\""));
        assert!(matches("foo", "bash -c 'source x && foo'"));
        assert_eq!(texts("bash -c 'foo'"), ["bash -c 'foo'"]);
        // Other flags stay meaningful; env or a bound is not unwrapped.
        assert!(!matches("foo", "bash -x -c 'foo'"));
        assert!(!matches("foo", "bash -ec 'foo'"));
        assert!(!matches("foo", "pwsh -ExecutionPolicy Bypass -Command foo"));
        assert!(!matches("foo", "cmd /k foo"));
        assert!(!matches("foo", "A=1 bash -c foo"));
        assert!(!matches("foo", "bash -c 'foo' arg"));
        assert!(!matches("foo", "bash -c 'foo' | bar"));
        // Fail closed.
        assert!(keys("bash -c 'foo").is_empty());
        assert!(keys("bash -c 'cat <<EOF\nx\nEOF'").is_empty());
        let deep = "bash -c \"bash -c 'bash -c \\\"bash -c \\\\\\\"bash -c foo\\\\\\\"\\\"'\"";
        assert!(keys(deep).len() <= 1);
        let mut nested = "foo".to_string();
        for _ in 0..8 {
            nested = format!("({nested})");
        }
        assert!(keys(&nested).is_empty());
    }

    #[test]
    fn heredocs_and_here_strings_are_not_read() {
        assert!(keys("cat <<EOF\nhello\nEOF").is_empty());
        assert!(keys("cat <<-EOF\nhello\nEOF").is_empty());
        assert!(keys("grep x <<< \"a b\"").is_empty());
        assert!(keys("$x = @'\nhello\n'@").is_empty());
        assert!(keys("$x = @\"\nhello\n\"@").is_empty());
        assert!(!matches("cat", "cat <<EOF\nhello\nEOF"));
    }

    #[test]
    fn cosmetic_differences_do_not_matter() {
        assert!(matches("foo", "foo.exe"));
        assert!(matches("Get-Service", "get-service"));
        assert!(matches("DIR /b", "dir /b"));
        assert!(!matches("Foo", "foo"));
        assert!(!matches("foo --Bar", "foo --bar"));
        assert!(matches("type C:\\a\\b.txt", "type C:/a/b.txt"));
        assert!(matches("foo.exe C:\\a\\b", "foo C:/a/b"));
        assert!(!matches("foo C:\\a\\b", "foo C:/a/b"));
        assert!(matches("foo --a \"x y\"", "foo --a 'x y'"));
        assert!(matches("foo \"x\"", "foo x"));
        assert!(matches("foo   --a    b", "foo --a b"));
        assert!(!matches("foo '$HOME'", "foo \"$HOME\""));
        assert!(!matches("foo '$HOME'", "foo $HOME"));
        assert!(!matches("jest", "./node_modules/.bin/jest"));
        assert!(matches("foo --a b", "foo \\\n  --a \\\n  b"));
        assert!(matches("foo --a b", "foo `\n --a `\n b"));
        assert!(matches("foo --a b", "foo ^\r\n --a ^\r\n b"));
        assert!(!matches("echo a/b", "echo a\\b"));
    }

    // ---- compound commands and nesting ---------------------------------------

    fn all_match(proposed: &str, recorded: &[&str]) {
        for rec in recorded {
            assert!(
                matches(proposed, rec),
                "{proposed:?} should stand for {rec:?}"
            );
        }
    }

    fn none_match(proposed: &str, recorded: &[&str]) {
        for rec in recorded {
            assert!(
                !matches(proposed, rec),
                "{proposed:?} must not stand for {rec:?}"
            );
        }
    }

    /// Compound shapes: command names and the separator between them.
    const SHAPES: &[(&[&str], &str)] = &[
        (&["a", "b"], "&&"),
        (&["a", "b"], ";"),
        (&["a", "b"], "||"),
        (&["a", "b"], "\n"),
        (&["a", "b", "c"], "&&"),
        (&["a", "b", "c"], ";"),
        (&["a", "b", "c"], "||"),
        (&["a", "b", "c"], "\n"),
    ];

    fn compose(names: &[String], separator: &str) -> String {
        names.join(&format!(" {separator} "))
    }

    fn owned(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    /// Decorations a recipe may leave off any command of a compound.
    const VARIANTS: &[fn(&str) -> String] = &[
        |x| format!("{x} | head"),
        |x| format!("{x} | sort | uniq"),
        |x| format!("{x} 2>&1"),
        |x| format!("{x} > out.log 2>&1"),
        |x| format!("{x} *>&1"),
        |x| format!("{x} | Out-Null"),
        |x| format!("timeout 9 {x}"),
        |x| format!("nice -n 5 {x}"),
        |x| format!("time {x}"),
        |x| format!("env -u HOME {x}"),
        |x| format!("stdbuf -oL {x}"),
        |x| format!("sudo {x}"),
        |x| format!("sudo -n timeout 9 nice {x}"),
        |x| format!("({x})"),
        |x| format!("{{ {x}; }}"),
        |x| format!("bash -c '{x}'"),
        |x| format!("sh -lc \"{x}\""),
        |x| format!("pwsh -NoProfile -Command \"{x}\""),
        |x| format!("cmd /c {x}"),
        |x| format!("Measure-Command {{ {x} }}"),
        |x| format!("cd /tmp && {x}"),
        |x| format!("Push-Location app; {x}; Pop-Location"),
    ];

    /// Decorations that change what runs: a recipe may not drop them.
    const SIGNIFICANT: &[fn(&str) -> String] = &[
        |x| format!("FOO=1 {x}"),
        |x| format!("env FOO=1 {x}"),
        |x| format!("{x} < in.txt"),
        |x| format!("{x} || true"),
        |x| format!("{x} ; true"),
        |x| format!("yes | {x}"),
        |x| format!("sudo -i {x}"),
        |x| format!("runas /user:x {x}"),
        |x| format!("timeout 9 {x} || true"),
        |x| format!("({x} || true)"),
        |x| format!("bash -c '{x} || true'"),
        |x| format!("bash -c '{x} < in.txt'"),
        |x| format!("bash -c 'FOO=1 {x}'"),
    ];

    /// `names` with `decorate` applied to the commands at `positions`.
    fn decorated(names: &[&str], positions: &[usize], decorate: fn(&str) -> String) -> Vec<String> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                if positions.contains(&i) {
                    decorate(n)
                } else {
                    (*n).to_string()
                }
            })
            .collect()
    }

    /// Each single position, then all of them.
    fn position_sets(len: usize) -> Vec<Vec<usize>> {
        let mut sets: Vec<Vec<usize>> = (0..len).map(|i| vec![i]).collect();
        sets.push((0..len).collect());
        sets
    }

    #[test]
    fn variants_on_any_command_of_a_compound_may_be_left_out() {
        for (names, sep) in SHAPES {
            let bare = compose(&owned(names), sep);
            for (v, variant) in VARIANTS.iter().enumerate() {
                for positions in position_sets(names.len()) {
                    let rec = compose(&decorated(names, &positions, *variant), sep);
                    assert!(
                        matches(&bare, &rec),
                        "variant #{v} at {positions:?}: {bare:?} should stand for {rec:?}"
                    );
                    assert!(
                        matches(&rec, &rec),
                        "variant #{v} at {positions:?}: {rec:?} should stand for itself"
                    );
                }
            }
        }
    }

    #[test]
    fn significant_forms_on_any_command_of_a_compound_are_kept() {
        for (names, sep) in SHAPES {
            let bare = compose(&owned(names), sep);
            for (v, significant) in SIGNIFICANT.iter().enumerate() {
                for positions in position_sets(names.len()) {
                    let rec = compose(&decorated(names, &positions, *significant), sep);
                    assert!(
                        !command_units(&rec).is_empty(),
                        "significant #{v} at {positions:?}: {rec:?} must be readable"
                    );
                    assert!(
                        !matches(&bare, &rec),
                        "significant #{v} at {positions:?}: {bare:?} must not stand for {rec:?}"
                    );
                    assert!(
                        matches(&rec, &rec),
                        "significant #{v} at {positions:?}: {rec:?} should stand for itself"
                    );
                }
            }
        }
    }

    #[test]
    fn stacked_variants_on_one_command_may_all_be_left_out() {
        for (names, sep) in SHAPES {
            let bare = compose(&owned(names), sep);
            for (i, outer) in VARIANTS.iter().enumerate() {
                for (j, inner) in VARIANTS.iter().enumerate() {
                    let rec = compose(
                        &decorated(names, &[0], |x| x.to_string())
                            .iter()
                            .enumerate()
                            .map(|(k, n)| if k == 0 { outer(&inner(n)) } else { n.clone() })
                            .collect::<Vec<_>>(),
                        sep,
                    );
                    assert!(
                        matches(&bare, &rec),
                        "outer #{i} over inner #{j}: {bare:?} should stand for {rec:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_significant_form_under_a_variant_is_still_kept() {
        for (names, sep) in SHAPES {
            let bare = compose(&owned(names), sep);
            for (i, significant) in SIGNIFICANT.iter().enumerate() {
                for (j, variant) in VARIANTS.iter().enumerate() {
                    let rec = compose(
                        &owned(names)
                            .iter()
                            .enumerate()
                            .map(|(k, n)| {
                                if k == 0 {
                                    variant(&significant(n))
                                } else {
                                    n.clone()
                                }
                            })
                            .collect::<Vec<_>>(),
                        sep,
                    );
                    assert!(
                        !matches(&bare, &rec),
                        "variant #{j} over significant #{i}: {bare:?} must not stand for {rec:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn cosmetic_differences_hold_inside_compounds() {
        all_match(
            "foo --a \"x y\" && bar C:\\a\\b",
            &[
                "foo --a 'x y' && bar C:/a/b",
                "foo.exe --a 'x y'; bar.exe C:/a/b",
                "foo   --a   'x y'\nbar C:/a/b",
                "foo --a \\\n  'x y' && bar \\\n C:/a/b",
                "foo --a `\n 'x y' && bar `\n C:/a/b",
                "foo --a ^\r\n 'x y' && bar ^\r\n C:/a/b",
            ],
        );
        none_match(
            "foo --a \"x y\" && bar",
            &[
                "foo --a 'x  y' && bar",
                "foo --A 'x y' && bar",
                "Foo --a 'x y' && bar",
            ],
        );
        all_match(
            "Get-Service && Get-Process",
            &["get-service && GET-PROCESS"],
        );
    }

    #[test]
    fn a_changed_command_in_a_compound_never_matches() {
        for (names, sep) in SHAPES {
            let names = owned(names);
            let bare = compose(&names, sep);
            for i in 0..names.len() {
                let mut swapped = names.clone();
                swapped[i] = "x".to_string();
                let rec = compose(&swapped, sep);
                assert!(!matches(&bare, &rec), "{bare:?} vs {rec:?}");
                assert!(matches(&rec, &rec));
            }
            // A recipe naming a command that never ran does not match; a
            // recipe naming fewer of the commands that ran does.
            let shorter = compose(&names[..names.len() - 1], sep);
            assert!(!matches(&bare, &shorter), "{bare:?} vs {shorter:?}");
            assert!(matches(&shorter, &bare), "{shorter:?} vs {bare:?}");
        }
    }

    #[test]
    fn separators_need_no_surrounding_space() {
        assert_eq!(keys("a&&b"), ["a", "b"]);
        assert_eq!(keys("a;b"), ["a", "b"]);
        assert_eq!(keys("a||b"), ["a", "b"]);
        assert_eq!(keys("a|b"), ["a | b"]);
        assert_eq!(keys("a&b"), ["a &", "b"]);
        assert_eq!(keys("a;b;c"), ["a", "b", "c"]);
        assert!(matches("a && b", "a&&b"));
        assert!(matches("a;b", "a ; b"));
    }

    #[test]
    fn mixed_separators_each_split() {
        assert_eq!(keys("a && b || c"), ["a", "b", "c"]);
        assert_eq!(keys("a && b; c || d & e"), ["a", "b", "c", "d &", "e"]);
        assert_eq!(keys("a\nb\n\nc"), ["a", "b", "c"]);
        assert_eq!(keys("a && b | head && c | tail -n 3"), ["a", "b", "c"]);
        assert_eq!(keys("a | head && b"), ["a", "b"]);
        assert_eq!(keys("a 2>&1 && b >out.log; c"), ["a", "b", "c"]);
    }

    #[test]
    fn trailing_and_leading_separators_add_no_commands() {
        assert_eq!(keys("a;"), ["a"]);
        assert_eq!(keys("a ;"), ["a"]);
        assert_eq!(keys("a\n"), ["a"]);
        assert_eq!(keys("\n\na && b\n"), ["a", "b"]);
        assert!(keys("").is_empty());
        assert!(keys("   \n  ").is_empty());
        assert!(keys("cd app").is_empty());
        assert!(keys("cd app && cd ..").is_empty());
        assert!(!matches("", "a"));
    }

    #[test]
    fn pipelines_inside_compounds_keep_their_non_filter_stages() {
        assert_eq!(keys("a | b"), ["a | b"]);
        assert_eq!(keys("a | b | c"), ["a | b | c"]);
        assert_eq!(keys("a | b | head"), ["a | b"]);
        assert_eq!(keys("a | head | b"), ["a | b"]);
        assert_eq!(keys("a | b && c | d | tail"), ["a | b", "c | d"]);
        assert!(!matches("a", "a | b"));
        assert!(!matches("b", "a | b"));
        assert!(!matches("a | b", "a | c"));
        assert!(!matches("a | b", "b | a"));
        assert!(matches("a | b", "a | b | sort"));
        assert!(matches(
            "a | b && c",
            "cd x && a | b 2>&1 | tee log && c > o"
        ));
    }

    #[test]
    fn failure_masking_binds_to_the_command_it_follows() {
        assert_eq!(keys("a && b || true"), ["a", "b || true"]);
        assert_eq!(keys("a || true && b"), ["a || true", "b"]);
        assert_eq!(keys("a; true; b"), ["a; true", "b"]);
        assert_eq!(keys("a || exit 0"), ["a || exit 0"]);
        assert_eq!(keys("a | b || true"), ["a | b || true"]);
        assert_eq!(keys("a || : && b"), ["a || :", "b"]);
        assert!(!matches("a && b", "a && b || true"));
        assert!(!matches("a && b", "a || true && b"));
        assert!(!matches("a && b", "a && b; true"));
        assert!(matches("a && b || true", "a && b || true"));
        assert!(!matches("a && b || true", "a || true && b"));
        // `true` as the first command is not failure masking.
        assert_eq!(keys("true && a"), ["true", "a"]);
        assert_eq!(keys("a && true"), ["a", "true"]);
    }

    #[test]
    fn backgrounding_binds_to_the_command_it_follows() {
        assert_eq!(keys("a & b"), ["a &", "b"]);
        assert_eq!(keys("a && b & c"), ["a", "b &", "c"]);
        assert_eq!(keys("(a; b) & c"), ["a", "b &", "c"]);
        assert!(!matches("a && b", "a && b &"));
        assert!(!matches("a && b", "a & b"));
        assert!(matches("a & b", "a & b"));
        assert!(!matches("a", "a & b"));
        assert!(matches("b", "a & b"));
    }

    #[test]
    fn navigation_chains_vanish_wherever_they_appear() {
        assert_eq!(keys("cd a && cd b && pushd c && a"), ["a"]);
        assert_eq!(keys("cd a; popd; a; cd ..; b"), ["a", "b"]);
        assert_eq!(keys("a && cd b && c"), ["a", "c"]);
        assert_eq!(
            keys("cd /d C:\\app & dir & cd .. & dir /b"),
            ["dir &", "dir /b"]
        );
        assert!(matches("a && c", "a && cd b && c"));
        assert!(matches("cd x && a", "a"));
    }

    #[test]
    fn standalone_commands_in_compounds() {
        let rec = "set -e && source .venv/bin/activate && cd app && a | head && b 2>&1";
        let standalone: Vec<bool> = command_units(rec)
            .iter()
            .map(super::Unit::is_standalone)
            .collect();
        assert_eq!(standalone, [true, true, false, false]);
        all_match("a && b", &[rec]);
        all_match("a", &[rec]);
        all_match("set -e && a && b", &[rec]);
        all_match("source .venv/bin/activate && a", &[rec]);
        all_match("set -e && source .venv/bin/activate && a && b", &[rec]);
        none_match("set +e && a && b", &[rec]);
        none_match("source other/bin/activate && a", &[rec]);
        none_match("set -u && a", &[rec]);
        none_match("a && b && c", &[rec]);
        // A standalone command that was never run cannot be added.
        none_match("set -e && a", &["a"]);
        none_match("nvm use 20 && a", &["nvm use 18 && a"]);
        all_match(
            "nvm use 20 && a",
            &["nvm use 20 && a", "nvm use 20; cd x; a"],
        );
        // Several setup commands, in any combination, may be left out.
        all_match(
            "a",
            &[
                "set -e; set -o pipefail; echo --- ; source x; a",
                "$ErrorActionPreference = 'Stop'; . .\\.venv\\Scripts\\Activate.ps1; a",
                "@echo off & chcp 65001 >NUL & call vcvars64.bat & a",
            ],
        );
    }

    #[test]
    fn environment_settings_in_compounds() {
        assert_eq!(keys("A=1 a && B=2 b"), ["A=1 a", "B=2 b"]);
        assert_eq!(keys("A=1 B=2 a | head"), ["A=1 B=2 a"]);
        assert!(!matches("a && b", "A=1 a && b"));
        assert!(!matches("a && b", "a && B=2 b"));
        assert!(!matches("a && b", "export A=1; a && b"));
        assert!(!matches("a && b", "$env:A='1'; a; b"));
        assert!(!matches("a && b", "env A=1 a && b"));
        assert!(matches("A=1 a && b", "A=1 a && b | head"));
        assert!(matches("A=1 a && B=2 b", "A=1 timeout 5 a && B=2 nice b"));
        assert!(!matches("A=2 a && b", "A=1 a && b"));
        assert!(!matches("B=2 A=1 a", "A=1 B=2 a"));
        assert!(matches("env A=1 a", "A=1 a"));
        assert!(matches("A=1 a", "env -u HOME A=1 a"));
    }

    #[test]
    fn retained_wrappers_in_compounds_are_per_command() {
        let rec = command_units("timeout 5 a && timeout 7 b && c");
        assert_eq!(rec.len(), 3);
        let bare = command_units("a && b && c");
        assert_eq!(bare[0].shown_for(&rec[0]), "timeout 5 a");
        assert_eq!(bare[1].shown_for(&rec[1]), "timeout 7 b");
        assert_eq!(bare[2].shown_for(&rec[2]), "c");
        assert!(matches("a && b && c", "timeout 5 a && timeout 7 b && c"));
        assert!(matches(
            "timeout 5 a && timeout 7 b",
            "timeout 5 a && timeout 7 b && c"
        ));
        assert!(!matches("timeout 7 a && b", "timeout 5 a && timeout 7 b"));
        assert!(!matches(
            "timeout 5 a && timeout 5 b",
            "timeout 5 a && timeout 7 b"
        ));
        assert!(!matches("timeout 5 a && b", "a && b"));
        // The same command run twice under different bounds.
        assert!(matches("timeout 5 a", "timeout 5 a; timeout 7 a"));
        assert!(matches("timeout 7 a", "timeout 5 a; timeout 7 a"));
        assert!(!matches("timeout 9 a", "timeout 5 a; timeout 7 a"));
        // Sudo is restored the same way.
        let rec = command_units("sudo a && sudo -u bob b");
        let bare = command_units("a && b");
        assert_eq!(bare[0].shown_for(&rec[0]), "sudo a");
        assert_eq!(bare[1].shown_for(&rec[1]), "sudo -u bob b");
        assert!(!matches("sudo -u alice b", "sudo -u bob b"));
        assert!(!matches("sudo a", "a"));
        assert!(!matches("sudo a", "sudo -E a"));
    }

    #[test]
    fn sudo_is_a_retained_wrapper() {
        for rec in [
            "sudo foo",
            "doas foo",
            "sudo -n foo",
            "sudo -E foo",
            "sudo -H -n foo",
            "sudo -u root foo",
            "sudo --user=root foo",
            "sudo --user root foo",
            "sudo -g wheel foo",
            "sudo -- foo",
            "sudo foo | head",
            "sudo timeout 5 foo",
            "sudo nice -n 5 foo",
        ] {
            assert!(matches("foo", rec), "{rec}");
            let shown = command_units("foo")[0].shown_for(&command_units(rec)[0]);
            assert!(
                shown.starts_with(rec.split(" foo").next().unwrap()),
                "{shown}"
            );
        }
        assert_eq!(
            command_units("foo")[0].shown_for(&command_units("sudo -u root foo --x")[0]),
            "sudo -u root foo"
        );
        assert!(matches("sudo foo", "sudo foo"));
        assert!(!matches("sudo foo", "foo"));
        assert!(!matches("sudo -u a foo", "sudo -u b foo"));
        assert!(!matches("sudo foo", "sudo -E foo"));
        assert!(!matches("sudo foo", "doas foo"));
        assert!(!matches("timeout 5 foo", "sudo timeout 5 foo"));
        assert!(matches("A=1 foo", "A=1 sudo foo"));
        assert!(!matches("foo", "A=1 sudo foo"));
        // Not wrappers: a login shell, a shell, stdin passwords, other verbs.
        for rec in [
            "sudo -i foo",
            "sudo -s foo",
            "sudo -S foo",
            "sudo -l",
            "sudo -v",
            "sudo -u root",
            "sudo",
            "runas /user:x foo",
        ] {
            assert!(!matches("foo", rec), "{rec}");
        }
    }

    #[test]
    fn grouping_and_subshells_in_compounds() {
        assert_eq!(keys("(a && b)"), ["a", "b"]);
        assert_eq!(keys("(a; b) && c"), ["a", "b", "c"]);
        assert_eq!(keys("{ a; b; } | head"), ["a", "b"]);
        assert_eq!(keys("(cd x && a)"), ["a"]);
        assert_eq!(keys("(cd x; a) > out.log 2>&1"), ["a"]);
        assert_eq!(keys("(a) && (b)"), ["a", "b"]);
        assert_eq!(keys("(a || true)"), ["a || true"]);
        assert_eq!(keys("(a; b || true)"), ["a", "b || true"]);
        assert_eq!(keys("(a & b)"), ["a &", "b"]);
        assert_eq!(keys("(a; b) &"), ["a", "b &"]);
        assert_eq!(keys("(a | b)"), ["a | b"]);
        assert_eq!(keys("(a | head)"), ["a"]);
        assert!(matches("a && b", "(a; b)"));
        assert!(matches("a && b", "(cd x; a) && (b | head)"));
        assert!(!matches("a", "(a || true)"));
        assert!(!matches("a && b", "(a; b) &"));
        assert!(!matches("a && b", "(a; b) < in.txt"));
        // A group that is part of a larger pipeline is not unwrapped.
        assert!(!matches("a", "(a) | b"));
        assert!(!matches("a", "b | (a)"));
    }

    #[test]
    fn shell_invocations_nested_in_compounds() {
        assert_eq!(keys("bash -c 'a && b'"), ["a", "b"]);
        assert_eq!(keys("bash -c 'a; b' && c"), ["a", "b", "c"]);
        assert_eq!(keys("c && bash -c 'a && b'"), ["c", "a", "b"]);
        assert_eq!(keys("bash -c 'cd x && a'"), ["a"]);
        assert_eq!(keys("bash -c 'A=1 a'"), ["A=1 a"]);
        assert_eq!(keys("bash -c 'a | b'"), ["a | b"]);
        assert_eq!(keys("bash -c 'a | head' | tail"), ["a"]);
        assert_eq!(keys("bash -c 'a' 2>&1 | tee log"), ["a"]);
        assert_eq!(keys("bash -c 'a || true'"), ["a || true"]);
        assert_eq!(keys("bash -c 'a &' && b"), ["a &", "b"]);
        assert_eq!(keys("bash -c 'a' & b"), ["a &", "b"]);
        assert_eq!(keys("bash -c 'source x && a | head'"), ["source x", "a"]);
        assert!(matches("a && b", "bash -c 'a && b | head' 2>&1"));
        assert!(matches("a", "bash -c 'a || b'"));
        assert!(!matches("a", "bash -c 'a || true'"));
        assert!(!matches("a", "bash -c 'a' | b"));
        assert!(!matches("a", "bash -c 'a' < in.txt"));
        assert!(!matches("a", "bash -c 'a' &"));
        assert!(!matches("a && b", "bash -c 'a' && x"));
        assert!(matches("bash -c 'a'", "bash -c 'a'"));
        assert!(matches("bash -c 'a && b'", "bash -c 'a && b'"));
    }

    #[test]
    fn shell_invocations_nested_in_shell_invocations() {
        for rec in [
            "bash -c \"sh -c 'foo'\"",
            "sh -c \"bash -c 'foo'\"",
            "bash -c '(foo)'",
            "bash -c \"(sh -c 'foo')\"",
            "pwsh -NoProfile -Command \"bash -c 'foo'\"",
            "pwsh -Command \"cmd /c foo\"",
            "cmd /c \"pwsh -Command foo\"",
            "cmd /c \"bash -c 'foo'\"",
            "bash -c \"pwsh -NoProfile -NonInteractive -Command 'foo'\"",
            "((foo))",
            "({ foo; })",
            "bash -c '{ foo; }'",
        ] {
            assert!(matches("foo", rec), "{rec}");
            assert_eq!(keys(rec), ["foo"], "{rec}");
        }
        assert_eq!(keys("bash -c \"sh -c 'a && b'\""), ["a", "b"]);
        assert_eq!(keys("pwsh -Command \"cmd /c 'a && b'\""), ["a", "b"]);
        assert_eq!(
            keys("bash -c \"cd x && sh -c 'a; b' && c\""),
            ["a", "b", "c"]
        );
        assert_eq!(keys("bash -c \"sh -c 'a || true'\""), ["a || true"]);
        assert_eq!(keys("bash -c \"sh -c 'a &'; b\""), ["a &", "b"]);
        assert_eq!(keys("bash -c \"sh -c 'FOO=1 a'\""), ["FOO=1 a"]);
        assert!(!matches("a", "bash -c \"sh -c 'a < in.txt'\""));
        assert!(!matches("a", "bash -c \"sh -c 'a || true'\""));
        assert!(!matches("a", "bash -c \"sh -c 'a | b'\""));
        assert!(!matches("a", "bash -c \"sh -c 'FOO=1 a'\""));
        assert!(matches("a", "bash -c \"sh -c 'a | head'\""));
        assert!(matches("a && b", "bash -c \"sh -c 'a' && (b)\""));
    }

    #[test]
    fn wrappers_around_shell_invocations_are_not_unwrapped() {
        // A bound, a user or an environment on the outside applies to the
        // whole inner command line, so the pieces are not compared one by one.
        for rec in [
            "timeout 5 bash -c 'foo'",
            "sudo bash -c 'foo'",
            "sudo -u bob sh -c 'foo'",
            "A=1 bash -c 'foo'",
            "env A=1 bash -c 'foo'",
            "time timeout 5 bash -c 'foo'",
        ] {
            assert!(!matches("foo", rec), "{rec}");
            assert!(matches(rec, rec), "{rec}");
        }
        // Wrappers that carry nothing do not stop the unwrap of `bash -c`.
        assert!(matches("foo", "nice bash -c 'foo'"));
        assert!(matches("foo", "time bash -c 'foo'"));
        assert!(matches("foo", "env -u HOME bash -c 'foo'"));
        assert!(matches("foo", "nice -n 5 time bash -c 'foo'"));
    }

    #[test]
    fn nesting_depth_is_bounded_and_fails_closed() {
        fn nest(depth: usize) -> String {
            let mut command = "foo".to_string();
            for _ in 0..depth {
                command = format!("({command})");
            }
            command
        }
        assert_eq!(keys(&nest(1)), ["foo"]);
        assert_eq!(keys(&nest(super::MAX_DEPTH)), ["foo"]);
        assert!(keys(&nest(super::MAX_DEPTH + 1)).is_empty());
        assert!(keys(&nest(super::MAX_DEPTH + 4)).is_empty());
        assert!(!matches("foo", &nest(super::MAX_DEPTH + 1)));
        assert!(!matches(&nest(super::MAX_DEPTH + 1), "foo"));
        // Too deep anywhere in a compound makes the whole line unreadable.
        let too_deep = format!("a && {} && b", nest(super::MAX_DEPTH + 1));
        assert!(keys(&too_deep).is_empty());
        assert!(!matches("a && b", &too_deep));
        assert!(!matches("a", &too_deep));
    }

    #[test]
    fn anything_unreadable_in_a_compound_makes_it_all_unreadable() {
        for rec in [
            "a && bash -c 'b",
            "a && (b",
            "a && 'b",
            "a && \"b",
            "a && cat <<EOF\nx\nEOF",
            "a && grep x <<< y",
            "a && (b; cat <<EOF\nx\nEOF)",
            "a && bash -c 'cat <<EOF\nx\nEOF'",
            "a && $x = @'\nx\n'@",
            "(a && (b; $x = @\"\nx\n\"@))",
        ] {
            assert!(keys(rec).is_empty(), "{rec}");
            assert!(!matches("a", rec), "{rec}");
            assert!(!matches(rec, rec), "{rec}");
        }
        // And a recipe that cannot be read matches nothing.
        assert!(!matches("a && (b", "a && b"));
        assert!(!matches("a && bash -c 'b", "a && b"));
    }

    #[test]
    fn compounds_in_powershell_and_cmd() {
        assert_eq!(
            keys("Push-Location app; $env:MODE='x'; dotnet test | Out-Null; Pop-Location"),
            ["$env:MODE='x'; dotnet test"]
        );
        assert_eq!(
            keys("cd /d C:\\app && npm ci && npm test > t.log 2>&1"),
            ["npm ci", "npm test"]
        );
        assert_eq!(
            keys("call setup.bat && cargo test"),
            ["call setup.bat", "cargo test"]
        );
        assert!(matches(
            "Get-Service; Get-Process",
            "get-service ; GET-PROCESS | Out-Null"
        ));
        assert!(matches(
            "dotnet build && dotnet test",
            "pwsh -NoProfile -Command \"cd app; dotnet build; dotnet test\""
        ));
        assert!(matches(
            "npm ci && npm test",
            "cmd /d /c \"cd /d C:\\app && npm ci && npm test\""
        ));
        assert!(!matches(
            "dotnet build && dotnet test",
            "pwsh -Command \"dotnet build; dotnet test || true\""
        ));
        assert!(!matches("npm test", "cmd /c \"npm test < in.txt\""));
        assert!(!matches("npm test", "$env:MODE='x'; npm test"));
    }

    #[test]
    fn the_table_declares_each_category() {
        for row in super::ALLOWLIST {
            let consistent = matches!(
                (&row.form, row.category),
                (
                    super::Form::Filter(_) | super::Form::Wrapper(_),
                    Category::Variant
                ) | (super::Form::Standalone(_), Category::Standalone)
            );
            assert!(consistent, "{:?}", row.names);
        }
    }
}
