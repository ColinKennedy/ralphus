//! The `initialize solo-developer` answers file (RAL-576): every setting the run
//! resolved, with the value used and where it came from, written as TOML at
//! the end of the run and replayable with `--answers-file <path>`.
//!
//! Keys are the `INTERACTIVE_SETTINGS` flag names without `--`, with `-` as
//! `_` (`--review-auto-submit-pr-stack` is `review_auto_submit_pr_stack`).
//! Precedence is flag > answers file > interactive prompt/default: a loaded
//! file only fills options a flag left unset.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::{INTERACTIVE_SETTINGS, InitializeSetting, InitializeSoloDeveloperOptions};

const VERSION: i64 = 1;
const REDACTED: &str = "<redacted>";
const FORGE_TOKEN_ENV: &str = "RALPHUS_FORGE_TOKEN";
const KNOWN_ENTRY_FIELDS: [&str; 5] = ["value", "source", "prompt", "flag", "asked"];

/// Where a recorded value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Source {
    Flag,
    File,
    Prompt,
    Env,
    Default,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Flag => "flag",
            Source::File => "file",
            Source::Prompt => "prompt",
            Source::Env => "env",
            Source::Default => "default",
        }
    }
}

struct Entry {
    value: toml::Value,
    source: Source,
    asked: bool,
}

#[derive(Default)]
struct Recorder {
    entries: HashMap<String, Entry>,
    file_keys: HashSet<String>,
}

thread_local! {
    static RECORDER: RefCell<Recorder> = RefCell::new(Recorder::default());
}

pub(super) fn key_of(setting: &InitializeSetting) -> String {
    setting.flag.trim_start_matches("--").replace('-', "_")
}

/// Clears everything recorded or loaded so far; called at the start of a run.
pub(super) fn reset() {
    RECORDER.with(|recorder| *recorder.borrow_mut() = Recorder::default());
}

/// Whether `--answers-file` supplied this setting (as opposed to a flag).
pub(super) fn from_file(setting: &InitializeSetting) -> bool {
    RECORDER.with(|recorder| recorder.borrow().file_keys.contains(&key_of(setting)))
}

/// The source of a value that arrived pre-answered.
pub(super) fn supplied_source(setting: &InitializeSetting) -> Source {
    if from_file(setting) {
        Source::File
    } else {
        Source::Flag
    }
}

/// Strips credentials a URL embeds (`https://user:token@host/...`) so they are
/// never written out; an `ssh://git@host` user is not a credential and stays.
fn redact_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, tail) = match rest.find('/') {
            Some(index) => rest.split_at(index),
            None => (rest, ""),
        };
        if let Some((userinfo, host)) = authority.rsplit_once('@') {
            if scheme != "ssh" || userinfo.contains(':') {
                return format!("{scheme}://{host}{tail}");
            }
        }
    }
    url.to_string()
}

fn is_url_key(key: &str) -> bool {
    matches!(key, "project_url" | "project_fork_url" | "fork_url")
}

fn insert(setting: &InitializeSetting, value: toml::Value, source: Source, asked: bool) {
    let key = key_of(setting);
    let value = match value {
        toml::Value::String(text) if is_url_key(&key) => toml::Value::String(redact_url(&text)),
        other => other,
    };
    RECORDER.with(|recorder| {
        recorder.borrow_mut().entries.insert(
            key,
            Entry {
                value,
                source,
                asked,
            },
        );
    });
}

pub(super) fn record_str(setting: &InitializeSetting, value: &str, source: Source) {
    insert(
        setting,
        toml::Value::String(value.to_string()),
        source,
        true,
    );
}

pub(super) fn record_bool(setting: &InitializeSetting, value: bool, source: Source) {
    insert(setting, toml::Value::Boolean(value), source, true);
}

/// Starts the (possibly empty) list of hosts chosen in this run.
pub(super) fn begin_list(setting: &InitializeSetting) {
    insert(
        setting,
        toml::Value::Array(Vec::new()),
        Source::Default,
        true,
    );
}

/// Notes the answer for one list member; only selected members are kept.
pub(super) fn record_list_item(
    setting: &InitializeSetting,
    item: &str,
    selected: bool,
    source: Source,
) {
    let key = key_of(setting);
    RECORDER.with(|recorder| {
        let mut recorder = recorder.borrow_mut();
        let entry = recorder.entries.entry(key).or_insert_with(|| Entry {
            value: toml::Value::Array(Vec::new()),
            source,
            asked: true,
        });
        entry.source = source;
        if let (true, toml::Value::Array(items)) = (selected, &mut entry.value) {
            items.push(toml::Value::String(item.to_string()));
        }
    });
}

/// Records that a forge token was given, never the token itself.
pub(super) fn record_forge_token(setting: &InitializeSetting, source: Source) {
    insert(
        setting,
        toml::Value::String(REDACTED.to_string()),
        source,
        true,
    );
}

/// The forge token from `RALPHUS_FORGE_TOKEN`, if set and non-empty.
pub(super) fn forge_token_from_env() -> Option<String> {
    std::env::var(FORGE_TOKEN_ENV)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

// ---- field access ---------------------------------------------------------

enum Slot<'a> {
    Bool(&'a mut Option<bool>),
    Text(&'a mut Option<String>),
    List(&'a mut Vec<String>),
}

fn slot<'a>(setup: &'a mut InitializeSoloDeveloperOptions, key: &str) -> Option<Slot<'a>> {
    Some(match key {
        "install_tmux" => Slot::Bool(&mut setup.install_tmux),
        "tmux_program" => Slot::Text(&mut setup.tmux_program),
        "setup_mcp" => Slot::Bool(&mut setup.setup_mcp),
        "mcp_host" => Slot::List(&mut setup.mcp_hosts),
        "agent_logins" => Slot::Text(&mut setup.agent_logins),
        "register_project" => Slot::Bool(&mut setup.register_project),
        "project_name" => Slot::Text(&mut setup.project_name),
        "project_is_fork" => Slot::Bool(&mut setup.project_is_fork),
        "project_fork_url" => Slot::Text(&mut setup.project_fork_url),
        "project_url" => Slot::Text(&mut setup.project_url),
        "project_description" => Slot::Text(&mut setup.project_description),
        "review_auto_submit_pr_stack" => Slot::Bool(&mut setup.review_auto_submit_pr_stack),
        "review_resolver_agent" => Slot::Text(&mut setup.review_resolver_agent),
        "require_forks" => Slot::Bool(&mut setup.require_forks),
        "fork_user" => Slot::Text(&mut setup.fork_user),
        "fork_url" => Slot::Text(&mut setup.fork_url),
        "forge_host" => Slot::Text(&mut setup.forge_host),
        "forge_token" => Slot::Text(&mut setup.forge_token),
        "create_admin" => Slot::Bool(&mut setup.create_admin),
        "admin_name" => Slot::Text(&mut setup.admin_name),
        "setup_forge_token" => Slot::Bool(&mut setup.setup_forge_token),
        "forge_provider" => Slot::Text(&mut setup.forge_provider),
        "submit_sample" => Slot::Bool(&mut setup.submit_sample),
        "sample_mode" => Slot::Text(&mut setup.sample_mode),
        "sample_agent" => Slot::Text(&mut setup.sample_agent),
        _ => return None,
    })
}

// ---- loading --------------------------------------------------------------

fn line_of(text: &str, key: &str) -> Option<usize> {
    text.lines()
        .position(|line| {
            let line = line.trim_start();
            let name = line
                .trim_start_matches('[')
                .split(['=', ']', ' '])
                .next()
                .unwrap_or("");
            name.trim_matches('"') == key
        })
        .map(|index| index + 1)
}

fn describe_key(path: &Path, text: &str, key: &str) -> String {
    match line_of(text, key) {
        Some(line) => format!("{}:{line}: `{key}`", path.display()),
        None => format!("{}: `{key}`", path.display()),
    }
}

fn typed_value(
    path: &Path,
    text: &str,
    key: &str,
    value: &toml::Value,
    want_bool: bool,
    want_list: bool,
) -> Result<toml::Value, String> {
    let ok = if want_bool {
        value.is_bool()
    } else if want_list {
        value
            .as_array()
            .is_some_and(|items| items.iter().all(toml::Value::is_str))
    } else {
        value.is_str() || value.is_integer()
    };
    if ok {
        return Ok(value.clone());
    }
    let expected = if want_bool {
        "a boolean"
    } else if want_list {
        "a list of strings"
    } else {
        "a string"
    };
    Err(format!(
        "invalid answers file {}: expected {expected}, found {}",
        describe_key(path, text, key),
        value.type_str()
    ))
}

/// Reads `path` and fills every option a flag left unset. Flags win; keys not
/// covered by the file are left for the prompts or defaults.
pub(super) fn load_into(
    path: &Path,
    setup: &mut InitializeSoloDeveloperOptions,
) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read answers file {}: {error}", path.display()))?;
    let table: toml::Table = text
        .parse()
        .map_err(|error| format!("malformed answers file {}: {error}", path.display()))?;
    match table.get("version").and_then(toml::Value::as_integer) {
        Some(VERSION) => {}
        Some(other) => {
            return Err(format!(
                "answers file {} has unsupported version {other}; this ralphus reads version {VERSION}",
                path.display()
            ));
        }
        None => {
            return Err(format!(
                "answers file {} must start with `version = {VERSION}`",
                path.display()
            ));
        }
    }
    let known: HashSet<String> = INTERACTIVE_SETTINGS
        .iter()
        .map(|setting| key_of(setting))
        .collect();
    // Flags that imply an earlier yes/no answer stay authoritative over a file
    // value for that answer.
    let flag_implies_setup_mcp = setup.setup_mcp.is_none() && !setup.mcp_hosts.is_empty();
    let flag_implies_forge_token = setup.setup_forge_token.is_none() && setup.forge_token.is_some();
    for (key, entry) in &table {
        if key == "version" {
            continue;
        }
        if !known.contains(key) {
            return Err(format!(
                "unknown key in answers file {}",
                describe_key(path, &text, key)
            ));
        }
        let value = match entry {
            toml::Value::Table(fields) => {
                if let Some(unknown) = fields
                    .keys()
                    .find(|field| !KNOWN_ENTRY_FIELDS.contains(&field.as_str()))
                {
                    return Err(format!(
                        "unknown field `{unknown}` in answers file {}",
                        describe_key(path, &text, key)
                    ));
                }
                fields.get("value").ok_or_else(|| {
                    format!(
                        "answers file {} has no `value`",
                        describe_key(path, &text, key)
                    )
                })?
            }
            bare => bare,
        };
        let skip = (key == "setup_mcp" && flag_implies_setup_mcp)
            || (key == "setup_forge_token" && flag_implies_forge_token);
        let Some(target) = slot(setup, key) else {
            continue;
        };
        let applied = match target {
            Slot::Bool(field) => {
                let value = typed_value(path, &text, key, value, true, false)?;
                if field.is_none() && !skip {
                    *field = value.as_bool();
                    true
                } else {
                    false
                }
            }
            Slot::Text(field) => {
                let value = typed_value(path, &text, key, value, false, false)?;
                let text_value = match &value {
                    toml::Value::Integer(number) => number.to_string(),
                    other => other.as_str().unwrap_or_default().to_string(),
                };
                // The placeholder written for secrets means "not provided".
                if key == "forge_token" && text_value == REDACTED {
                    false
                } else if field.is_none() {
                    *field = Some(text_value);
                    true
                } else {
                    false
                }
            }
            Slot::List(field) => {
                let value = typed_value(path, &text, key, value, false, true)?;
                if field.is_empty() {
                    *field = value
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect();
                    true
                } else {
                    false
                }
            }
        };
        if applied {
            RECORDER.with(|recorder| {
                recorder.borrow_mut().file_keys.insert(key.clone());
            });
        }
    }
    Ok(())
}

// ---- finishing ------------------------------------------------------------

/// The default a setting takes when its question was never reached.
fn unasked_default(key: &str) -> toml::Value {
    use toml::Value::{Array, Boolean, String as Text};
    match key {
        "install_tmux" => Boolean(false),
        "tmux_program" | "project_fork_url" | "project_description" | "fork_url" => {
            Text(String::new())
        }
        "setup_mcp" | "project_is_fork" | "require_forks" | "create_admin" => Boolean(false),
        "mcp_host" => Array(Vec::new()),
        "agent_logins" => Text("none".to_string()),
        "register_project"
        | "review_auto_submit_pr_stack"
        | "setup_forge_token"
        | "submit_sample" => Boolean(true),
        "project_name" => Text(
            std::env::current_dir()
                .ok()
                .and_then(|cwd| {
                    cwd.file_name()
                        .map(|name| name.to_string_lossy().to_string())
                })
                .unwrap_or_else(|| "project".to_string()),
        ),
        "project_url" => Text(
            std::env::current_dir()
                .map(|cwd| super::default_upstream_url(&cwd, None))
                .unwrap_or_default(),
        ),
        "review_resolver_agent" | "sample_agent" => Text("claude-code".to_string()),
        "fork_user" => Text(super::default_user_name()),
        "forge_provider" => Text("github".to_string()),
        "forge_host" => Text("github.com".to_string()),
        "forge_token" => Text(REDACTED.to_string()),
        "admin_name" => Text("John Smith".to_string()),
        "sample_mode" => Text("agent".to_string()),
        _ => Text(String::new()),
    }
}

/// The value a flag or the loaded file supplied for `key`, if any.
fn supplied_value(setup: &mut InitializeSoloDeveloperOptions, key: &str) -> Option<toml::Value> {
    if key == "forge_token" {
        return setup
            .forge_token
            .is_some()
            .then(|| toml::Value::String(REDACTED.to_string()));
    }
    match slot(setup, key)? {
        Slot::Bool(field) => field.map(toml::Value::Boolean),
        Slot::Text(field) => field.clone().map(toml::Value::String),
        Slot::List(field) => (!field.is_empty())
            .then(|| toml::Value::Array(field.iter().cloned().map(toml::Value::String).collect())),
    }
}

/// Fills every setting the run never asked about with the value that applied
/// (a supplied one, else the default), then renders the file.
pub(super) fn render(setup: &mut InitializeSoloDeveloperOptions) -> String {
    let mut out = format!(
        "# ralphus initialize solo-developer answers (replay with `--answers-file <this file>`)\n\
         # Some values (tmux paths, MCP hosts, project names/URLs) are specific to the\n\
         # machine that produced this file; the forge token is never stored.\n\
         version = {VERSION}\n"
    );
    for setting in INTERACTIVE_SETTINGS {
        let key = key_of(setting);
        let recorded = RECORDER.with(|recorder| {
            recorder
                .borrow()
                .entries
                .get(&key)
                .map(|entry| (entry.value.clone(), entry.source, entry.asked))
        });
        let (value, source, asked) = match recorded {
            Some(found) => found,
            None => match supplied_value(setup, &key) {
                Some(value) => {
                    let from_file = RECORDER.with(|r| r.borrow().file_keys.contains(&key));
                    let source = if from_file {
                        Source::File
                    } else {
                        Source::Flag
                    };
                    (value, source, false)
                }
                None => (unasked_default(&key), Source::Default, false),
            },
        };
        let value = match value {
            toml::Value::String(text) if is_url_key(&key) => toml::Value::String(redact_url(&text)),
            other => other,
        };
        out.push_str(&format!("\n[{key}]\n"));
        if !asked {
            out.push_str("# not asked in this run\n");
        }
        out.push_str(&format!(
            "prompt = {}\nflag = {}\nvalue = {value}\nsource = \"{}\"\n",
            toml::Value::String(setting.prompt.to_string()),
            toml::Value::String(setting.flag.to_string()),
            source.as_str()
        ));
    }
    out
}

/// Where answers files are written: next to the global ralphus config.
fn answers_dir() -> PathBuf {
    ralphus_daemon::config::global_config_path()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
        .join("initialize-solo-developer")
}

/// Writes the timestamped file plus a stable `answers-latest.toml` copy;
/// returns the timestamped path.
pub(super) fn write(contents: &str) -> Result<PathBuf, String> {
    let dir = answers_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let path = dir.join(format!("answers-{stamp}-{}.toml", std::process::id()));
    std::fs::write(&path, contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    let latest = dir.join("answers-latest.toml");
    std::fs::write(&latest, contents)
        .map_err(|error| format!("could not write {}: {error}", latest.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, text: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ralphus-answers-test-{}-{}-{name}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::write(&path, text).unwrap();
        path
    }

    fn load(text: &str, setup: &mut InitializeSoloDeveloperOptions) -> Result<(), String> {
        reset();
        load_into(&temp_file("load", text), setup)
    }

    #[test]
    fn flag_beats_file_and_file_fills_the_rest() {
        let mut setup = InitializeSoloDeveloperOptions {
            admin_name: Some("From Flag".to_string()),
            ..Default::default()
        };
        load(
            "version = 1\nadmin_name = \"From File\"\nsample_mode = \"raw\"\n",
            &mut setup,
        )
        .unwrap();
        assert_eq!(setup.admin_name.as_deref(), Some("From Flag"));
        assert_eq!(setup.sample_mode.as_deref(), Some("raw"));
        assert!(from_file(&super::super::SAMPLE_MODE));
        assert!(!from_file(&super::super::ADMIN_NAME));
    }

    #[test]
    fn unknown_key_names_the_key_and_line() {
        let error = load(
            "version = 1\n\n[bogus_key]\nvalue = 1\n",
            &mut InitializeSoloDeveloperOptions::default(),
        )
        .unwrap_err();
        assert!(
            error.contains("bogus_key") && error.contains(":3:"),
            "{error}"
        );
    }

    #[test]
    fn malformed_file_and_missing_version_are_errors() {
        let mut setup = InitializeSoloDeveloperOptions::default();
        assert!(
            load("version = [", &mut setup)
                .unwrap_err()
                .contains("malformed")
        );
        assert!(
            load("admin_name = \"x\"\n", &mut setup)
                .unwrap_err()
                .contains("version")
        );
        assert!(
            load("version = 1\ncreate_admin = \"yes\"\n", &mut setup)
                .unwrap_err()
                .contains("create_admin")
        );
    }

    #[test]
    fn redacted_token_placeholder_means_not_provided() {
        let mut setup = InitializeSoloDeveloperOptions::default();
        load("version = 1\nforge_token = \"<redacted>\"\n", &mut setup).unwrap();
        assert!(setup.forge_token.is_none());
    }

    #[test]
    fn urls_lose_embedded_credentials_but_keep_ssh_users() {
        assert_eq!(
            redact_url("https://user:secret@github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        assert_eq!(
            redact_url("https://tok@github.com/o/r"),
            "https://github.com/o/r"
        );
        assert_eq!(
            redact_url("ssh://git@github.com/o/r"),
            "ssh://git@github.com/o/r"
        );
        assert_eq!(
            redact_url("git@github.com:o/r.git"),
            "git@github.com:o/r.git"
        );
    }

    #[test]
    fn rendered_file_round_trips_and_never_contains_the_token() {
        reset();
        let mut first = InitializeSoloDeveloperOptions {
            yes: true,
            admin_name: Some("Ada".to_string()),
            forge_token: Some("ghp_supersecret".to_string()),
            fork_url: Some("https://u:pw@github.com/ada/r.git".to_string()),
            mcp_hosts: vec!["claude".to_string()],
            ..Default::default()
        };
        record_forge_token(&super::super::FORGE_TOKEN, Source::Flag);
        record_str(
            &super::super::REVIEW_RESOLVER_AGENT,
            "codex",
            Source::Prompt,
        );
        let text = render(&mut first);
        assert!(
            !text.contains("ghp_supersecret") && !text.contains("pw@"),
            "{text}"
        );
        assert!(text.contains("# not asked in this run"));

        let mut second = InitializeSoloDeveloperOptions::default();
        load(&text, &mut second).unwrap();
        assert_eq!(second.admin_name.as_deref(), Some("Ada"));
        assert_eq!(second.review_resolver_agent.as_deref(), Some("codex"));
        assert_eq!(second.mcp_hosts, vec!["claude".to_string()]);
        assert_eq!(
            second.fork_url.as_deref(),
            Some("https://github.com/ada/r.git")
        );
        assert!(second.forge_token.is_none());
        // Every setting is covered, so a second render is the same file.
        reset();
        let mut third = InitializeSoloDeveloperOptions::default();
        load(&text, &mut third).unwrap();
        let again = render(&mut third);
        let values = |text: &str| {
            text.lines()
                .filter(|line| line.starts_with("value = ") || line.starts_with('['))
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(values(&text), values(&again));
    }
}
