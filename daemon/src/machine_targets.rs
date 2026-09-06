//! Statically-configured remote execution **targets** (RAL-355 Phase 2).
//!
//! A **target** names one `machine` value's durable remote storage and
//! runner policy — where its persistent Git clones/worktrees/job state live
//! on that machine, and how its `ralphus-runner` gets invoked there. See
//! [`docs/glossary.md`](../../docs/glossary.md)'s "target" entry.
//!
//! Config-file-only for v1, mirroring [`crate::agent_profiles`]'s pattern
//! (a `[section.name]` TOML table, parsed at read time, no daemon-store
//! CRUD) rather than [`crate::machines`]'s registry (SQLite-backed, live
//! API registration) — that choice was made deliberately during RAL-355
//! Phase 2's design interview, with an explicit caveat from the plan's
//! owner: they expect this will likely need to move to a DB-backed registry
//! (matching `projects`/`machine providers`) once this is exercised in a
//! real production environment. [`MachineTarget`] is kept decoupled from
//! *how* it was sourced for exactly that reason — a later swap to a
//! DB-backed loader only touches [`load_machine_targets`], not any
//! consumer of the parsed struct.
//!
//! Unlike agent profiles, this is loaded from **global scope only** — no
//! project-local `.ralphus.toml` is consulted. A machine's storage root and
//! runner policy describe the *machine*, not the project being worked on;
//! letting a project-local file silently redefine another machine's
//! `remote_root` would be a surprising, hard-to-audit way for one project's
//! config to affect every other project's remote work on that same machine.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::global_config_path;

/// One configured remote execution target: a `machine` value plus that
/// machine's durable storage root and runner policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineTarget {
    /// The `[machine.targets.<name>]` table key.
    pub name: String,
    /// The full `<scheme>:<uri>` machine value this target configures.
    /// Deliberately not named `uri` — that word already means only the
    /// right half of a machine value (see `docs/glossary.md`).
    pub machine: String,
    /// Absolute path, **on the remote machine**, persistent project clones,
    /// job state, and uploaded runners live under. Never expanded/validated
    /// against the daemon's own filesystem — see the module doc.
    pub remote_root: String,
    pub runner_mode: RunnerMode,
    /// The command used to invoke the runner on the remote machine. May be
    /// a bare executable path or an arbitrary compound shell command (e.g.
    /// `"some_env_manager signin -- ralphus-runner"`), the same way
    /// `RALPHUS_CLAUDE_COMMAND` and its siblings already work for local
    /// agent commands — interpreting that shape is the invoking provider's
    /// job, not this module's.
    pub runner_command: String,
    /// Daemon-host artifact paths keyed by Rust target triple. The provider
    /// probes the remote target before selecting one.
    pub runner_artifacts: BTreeMap<String, String>,
    /// Per-agent executable overrides for this target (RAL-355 Phase 7
    /// remainder), keyed by agent name (e.g. `"claude-code"`) — distinct
    /// from `runner_command`, which names the `ralphus-runner` binary
    /// itself, not an agent backend it launches. A daemon-local absolute
    /// executable path (an agent profile's explicit override, resolved for
    /// *this* daemon's own filesystem) is never valid on a different
    /// machine; this table is how an operator deliberately maps an agent
    /// name to a command that *is* valid there. See
    /// `ProviderRunner::resolve_remote_executable`, the dispatch-time
    /// consumer.
    pub agent_executables: BTreeMap<String, String>,
}

/// How a target's runner gets onto the remote machine (RAL-355 Phase 5).
/// Only [`RunnerMode::Installed`] has an implementation today —
/// [`RunnerMode::Upload`] is accepted and stored now so a target's config
/// doesn't need to change shape once Phase 5 lands, but nothing yet acts on
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerMode {
    /// The configured `runner_command` already resolves to a working
    /// `ralphus-runner` on the remote machine's `PATH` (or is an absolute
    /// path there). The safe default — never copies anything to the
    /// remote machine.
    Installed,
    /// The daemon uploads a matching runner artifact to the remote machine
    /// before first use. Not implemented yet.
    Upload,
}

const DEFAULT_RUNNER_COMMAND: &str = "ralphus-runner";

#[derive(Debug, Default, Deserialize)]
struct MachineTargetsFile {
    #[serde(default)]
    machine: Option<MachineTable>,
}

#[derive(Debug, Default, Deserialize)]
struct MachineTable {
    #[serde(default)]
    targets: BTreeMap<String, RawMachineTarget>,
}

#[derive(Debug, Deserialize)]
struct RawMachineTarget {
    machine: String,
    remote_root: String,
    /// Escape hatch for "reject known ephemeral locations by default" —
    /// set explicitly when ephemeral execution is genuinely wanted.
    #[serde(default)]
    allow_ephemeral_remote_root: bool,
    #[serde(default)]
    runner: RawRunner,
    /// `[machine.targets.<name>.agents]`: agent name -> remote executable
    /// override.
    #[serde(default)]
    agents: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRunner {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    artifacts: BTreeMap<String, String>,
}

/// Case-insensitive substrings marking a `remote_root` as an OS-cleaned
/// ephemeral location (RAL-355 Phase 2's "reject `%TEMP%`, `/tmp` ... by
/// default" requirement). Real path expansion happens on the remote
/// machine, not here — this is a best-effort textual check against what
/// the user actually typed, not a resolved path.
const EPHEMERAL_MARKERS: &[&str] = &["/tmp", "/var/tmp", "%temp%", "%tmp%", "$tmpdir", "$temp"];

fn looks_ephemeral(remote_root: &str) -> bool {
    let lower = remote_root.to_ascii_lowercase();
    EPHEMERAL_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Fast local sanity check that `remote_root` is *some* absolute form —
/// POSIX, UNC, Windows drive-letter, or an environment-variable-prefixed
/// form (`%TEMP%\...`, `$TMPDIR/...`) that expands to an absolute path on
/// the remote machine. Deliberately not a full validation: the remote
/// machine may be either OS regardless of the daemon host's own, so real
/// validation (does this path actually exist/is writable there) happens via
/// [`crate::remote_runner`]'s readiness probe against the actual machine,
/// not by pattern-matching a string here.
fn looks_absolute(remote_root: &str) -> bool {
    let b = remote_root.as_bytes();
    remote_root.starts_with('/')
        || remote_root.starts_with('\\')
        || remote_root.starts_with('%')
        || remote_root.starts_with('$')
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'\\' || b[2] == b'/'))
}

fn parse_targets_file(path: &Path) -> Result<BTreeMap<String, MachineTarget>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let parsed: MachineTargetsFile =
        toml::from_str(&text).map_err(|e| format!("could not parse {}: {e}", path.display()))?;
    let mut out = BTreeMap::new();
    for (name, raw) in parsed.machine.unwrap_or_default().targets {
        if raw.machine.trim().is_empty() {
            return Err(format!(
                "{}: machine target {name:?} has an empty machine value",
                path.display()
            ));
        }
        if raw.remote_root.trim().is_empty() {
            return Err(format!(
                "{}: machine target {name:?} has an empty remote_root",
                path.display()
            ));
        }
        // Checked before the absolute-path shape: a `%TEMP%\...`/`$TMPDIR/...`
        // form is env-var-expanded on the remote machine into *something*
        // absolute, but as literal text here it doesn't match any of
        // `looks_absolute`'s recognized prefixes — rejecting it as "not
        // absolute" first would be a confusing, wrong-cause error for a path
        // that's unambiguously an ephemeral-location reference.
        if looks_ephemeral(&raw.remote_root) && !raw.allow_ephemeral_remote_root {
            return Err(format!(
                "{}: machine target {name:?} remote_root {:?} looks like an OS-cleaned ephemeral location; set allow_ephemeral_remote_root = true if this is deliberate",
                path.display(),
                raw.remote_root
            ));
        }
        if !looks_absolute(&raw.remote_root) {
            return Err(format!(
                "{}: machine target {name:?} remote_root {:?} must be an absolute path (POSIX, UNC, drive-letter, or environment-variable-prefixed form)",
                path.display(),
                raw.remote_root
            ));
        }
        let runner_mode = match raw.runner.mode.as_deref() {
            None | Some("installed") => RunnerMode::Installed,
            Some("upload") => RunnerMode::Upload,
            Some(other) => {
                return Err(format!(
                    "{}: machine target {name:?} has unknown runner.mode {other:?}; expected \"installed\" or \"upload\"",
                    path.display()
                ));
            }
        };
        let runner_command = raw
            .runner
            .command
            .unwrap_or_else(|| DEFAULT_RUNNER_COMMAND.to_string());
        if runner_mode == RunnerMode::Upload && raw.runner.artifacts.is_empty() {
            return Err(format!(
                "{}: machine target {name:?} uses runner.mode \"upload\" but configures no runner.artifacts target-triple paths",
                path.display()
            ));
        }
        if raw
            .runner
            .artifacts
            .iter()
            .any(|(target, path)| target.trim().is_empty() || path.trim().is_empty())
        {
            return Err(format!(
                "{}: machine target {name:?} has an empty runner artifact target or path",
                path.display()
            ));
        }
        if raw
            .agents
            .iter()
            .any(|(agent, command)| agent.trim().is_empty() || command.trim().is_empty())
        {
            return Err(format!(
                "{}: machine target {name:?} has an empty agent name or command in [agents]",
                path.display()
            ));
        }
        out.insert(
            name.clone(),
            MachineTarget {
                name,
                machine: raw.machine,
                remote_root: raw.remote_root,
                runner_mode,
                runner_command,
                runner_artifacts: raw.runner.artifacts,
                agent_executables: raw.agents,
            },
        );
    }
    Ok(out)
}

/// Parses `$RALPHUS_CONFIGURATION_PATH` (a `PATH`-separated list of
/// `.ralphus.toml` files, left-to-right, later wins) — same convention
/// [`crate::agent_profiles`]'s own `configuration_path_entries` reads.
/// Duplicated rather than shared: both are three lines, and sharing would
/// mean either exposing agent_profiles's private helper or adding a third
/// module neither owns, for no real benefit.
fn configuration_path_entries(configuration_path_env: Option<&str>) -> Vec<PathBuf> {
    let Some(raw) = configuration_path_env else {
        return Vec::new();
    };
    std::env::split_paths(raw).collect()
}

/// Precedence, lowest to highest: `$RALPHUS_CONFIG_HOME/config.toml` (or its
/// `~/.config/ralphus/` default), then `$RALPHUS_CONFIGURATION_PATH` entries
/// in order. No project-local `.ralphus.toml` — see the module doc.
fn load_machine_targets_with(
    configuration_path_env: Option<&str>,
) -> Result<BTreeMap<String, MachineTarget>, String> {
    let mut merged = match global_config_path() {
        Some(path) if path.is_file() => parse_targets_file(&path)?,
        _ => BTreeMap::new(),
    };
    for path in configuration_path_entries(configuration_path_env) {
        if path.is_file() {
            for (name, target) in parse_targets_file(&path)? {
                merged.insert(name, target);
            }
        }
    }
    Ok(merged)
}

/// Load every configured machine target from global scope. See the module
/// doc for why project-local config is deliberately excluded.
pub fn load_machine_targets() -> Result<BTreeMap<String, MachineTarget>, String> {
    let raw = std::env::var("RALPHUS_CONFIGURATION_PATH").ok();
    load_machine_targets_with(raw.as_deref())
}

/// Look up the target configured for a resolved `machine` value (e.g.
/// `"ssh:devbox"`), if any. `None` is the common case — most machines run
/// with no target configured at all; Phase 2 is additive, not required.
#[must_use]
pub fn find_by_machine<'a>(
    targets: &'a BTreeMap<String, MachineTarget>,
    machine: &str,
) -> Option<&'a MachineTarget> {
    targets.values().find(|t| t.machine == machine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-machine-targets-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    #[test]
    fn parses_a_full_target_including_a_compound_runner_command() {
        let dir = tempdir("full");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.devbox]
machine = "ssh:devbox"
remote_root = "/home/me/.ralphus/remote-work"

[machine.targets.devbox.runner]
mode = "installed"
command = "some_env_manager signin -- ralphus-runner"
"#,
        )
        .expect("write config");

        let targets = parse_targets_file(&file).expect("parse");
        let devbox = targets.get("devbox").expect("devbox target");
        assert_eq!(devbox.name, "devbox");
        assert_eq!(devbox.machine, "ssh:devbox");
        assert_eq!(devbox.remote_root, "/home/me/.ralphus/remote-work");
        assert_eq!(devbox.runner_mode, RunnerMode::Installed);
        assert_eq!(
            devbox.runner_command,
            "some_env_manager signin -- ralphus-runner"
        );
    }

    #[test]
    fn parses_per_agent_executable_overrides() {
        let dir = tempdir("agents");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.devbox]
machine = "ssh:devbox"
remote_root = "/home/me/.ralphus/remote-work"

[machine.targets.devbox.agents]
claude-code = "claude"
codex = "/opt/tools/codex-wrapper"
"#,
        )
        .expect("write config");

        let targets = parse_targets_file(&file).expect("parse");
        let devbox = targets.get("devbox").expect("devbox target");
        assert_eq!(
            devbox
                .agent_executables
                .get("claude-code")
                .map(String::as_str),
            Some("claude")
        );
        assert_eq!(
            devbox.agent_executables.get("codex").map(String::as_str),
            Some("/opt/tools/codex-wrapper")
        );
    }

    #[test]
    fn agents_table_is_optional_and_defaults_empty() {
        let dir = tempdir("agents-default");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.plain]
machine = "ssh:plain"
remote_root = "/srv/ralphus"
"#,
        )
        .expect("write config");

        let targets = parse_targets_file(&file).expect("parse");
        assert!(targets["plain"].agent_executables.is_empty());
    }

    #[test]
    fn an_empty_agent_name_or_command_is_rejected() {
        let dir = tempdir("agents-empty");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.devbox]
machine = "ssh:devbox"
remote_root = "/srv/ralphus"

[machine.targets.devbox.agents]
"claude-code" = ""
"#,
        )
        .expect("write config");

        let err = parse_targets_file(&file).expect_err("empty command must be rejected");
        assert!(err.contains("empty agent name or command"), "{err}");
    }

    #[test]
    fn runner_table_is_optional_and_defaults() {
        let dir = tempdir("defaults");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.plain]
machine = "ssh:plain"
remote_root = "/srv/ralphus"
"#,
        )
        .expect("write config");

        let targets = parse_targets_file(&file).expect("parse");
        let plain = targets.get("plain").expect("plain target");
        assert_eq!(plain.runner_mode, RunnerMode::Installed);
        assert_eq!(plain.runner_command, DEFAULT_RUNNER_COMMAND);
    }

    #[test]
    fn upload_mode_is_accepted() {
        let dir = tempdir("upload-mode");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.up]
machine = "ssh:up"
remote_root = "/srv/ralphus"

[machine.targets.up.runner]
mode = "upload"

[machine.targets.up.runner.artifacts]
"x86_64-unknown-linux-musl" = "/artifacts/ralphus-runner"
"#,
        )
        .expect("write config");

        let targets = parse_targets_file(&file).expect("parse");
        assert_eq!(targets["up"].runner_mode, RunnerMode::Upload);
        assert_eq!(
            targets["up"].runner_artifacts["x86_64-unknown-linux-musl"],
            "/artifacts/ralphus-runner"
        );
    }

    #[test]
    fn upload_mode_requires_an_artifact() {
        let dir = tempdir("upload-without-artifact");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.up]
machine = "ssh:up"
remote_root = "/srv/ralphus"

[machine.targets.up.runner]
mode = "upload"
"#,
        )
        .expect("write config");
        let err = parse_targets_file(&file).expect_err("upload must name artifacts");
        assert!(err.contains("no runner.artifacts"), "{err}");
    }

    #[test]
    fn unknown_runner_mode_is_rejected() {
        let dir = tempdir("bad-mode");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.bad]
machine = "ssh:bad"
remote_root = "/srv/ralphus"

[machine.targets.bad.runner]
mode = "teleport"
"#,
        )
        .expect("write config");

        let err = parse_targets_file(&file).expect_err("must reject unknown mode");
        assert!(err.contains("teleport"), "{err}");
        assert!(err.contains("bad"), "{err}");
    }

    #[test]
    fn empty_machine_value_is_rejected() {
        let dir = tempdir("empty-machine");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.x]
machine = ""
remote_root = "/srv/ralphus"
"#,
        )
        .expect("write config");

        let err = parse_targets_file(&file).expect_err("must reject empty machine");
        assert!(err.contains("empty machine value"), "{err}");
    }

    #[test]
    fn empty_remote_root_is_rejected() {
        let dir = tempdir("empty-root");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.x]
machine = "ssh:x"
remote_root = ""
"#,
        )
        .expect("write config");

        let err = parse_targets_file(&file).expect_err("must reject empty remote_root");
        assert!(err.contains("empty remote_root"), "{err}");
    }

    #[test]
    fn relative_remote_root_is_rejected() {
        let dir = tempdir("relative-root");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.x]
machine = "ssh:x"
remote_root = "relative/path"
"#,
        )
        .expect("write config");

        let err = parse_targets_file(&file).expect_err("must reject a relative remote_root");
        assert!(err.contains("must be an absolute path"), "{err}");
    }

    #[test]
    fn windows_drive_letter_remote_root_is_accepted_as_absolute() {
        let dir = tempdir("windows-root");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.x]
machine = "ssh:x"
remote_root = 'C:\Users\me\.ralphus\remote-work'
"#,
        )
        .expect("write config");

        let targets = parse_targets_file(&file).expect("parse");
        assert_eq!(
            targets["x"].remote_root,
            "C:\\Users\\me\\.ralphus\\remote-work"
        );
    }

    #[test]
    fn tmp_remote_root_is_rejected_by_default() {
        let dir = tempdir("tmp-root");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.x]
machine = "ssh:x"
remote_root = "/tmp/ralphus-work"
"#,
        )
        .expect("write config");

        let err = parse_targets_file(&file).expect_err("must reject /tmp by default");
        assert!(err.contains("ephemeral"), "{err}");
        assert!(err.contains("allow_ephemeral_remote_root"), "{err}");
    }

    #[test]
    fn windows_temp_remote_root_is_rejected_by_default() {
        let dir = tempdir("windows-temp-root");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.x]
machine = "ssh:x"
remote_root = 'C:\Users\me\AppData\Local\Temp\ralphus'
"#,
        )
        .expect("write config");

        // The literal %TEMP% marker isn't present here (the user typed an
        // already-expanded-looking path), so this specific path is not
        // caught by the marker list -- documenting that the check is
        // textual, not semantic path resolution (which happens remotely).
        let targets = parse_targets_file(&file).expect("parse");
        assert_eq!(
            targets["x"].remote_root,
            "C:\\Users\\me\\AppData\\Local\\Temp\\ralphus"
        );
    }

    #[test]
    fn literal_percent_temp_percent_remote_root_is_rejected_by_default() {
        let dir = tempdir("percent-temp-root");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.x]
machine = "ssh:x"
remote_root = '%TEMP%\ralphus'
"#,
        )
        .expect("write config");

        let err = parse_targets_file(&file).expect_err("must reject a literal %TEMP% by default");
        assert!(err.contains("ephemeral"), "{err}");
    }

    #[test]
    fn ephemeral_remote_root_is_accepted_with_explicit_override() {
        let dir = tempdir("tmp-root-override");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.x]
machine = "ssh:x"
remote_root = "/tmp/ralphus-work"
allow_ephemeral_remote_root = true
"#,
        )
        .expect("write config");

        let targets = parse_targets_file(&file).expect("parse");
        assert_eq!(targets["x"].remote_root, "/tmp/ralphus-work");
    }

    #[test]
    fn env_var_prefixed_ephemeral_remote_root_is_accepted_with_explicit_override() {
        // Regression: `%TEMP%\...` must not additionally fail the
        // absolute-path check once ephemeral is explicitly allowed --
        // `looks_absolute` treats a leading `%`/`$` as an absolute-equivalent
        // env-var reference for exactly this case.
        let dir = tempdir("percent-temp-root-override");
        let file = dir.join(".ralphus.toml");
        fs::write(
            &file,
            r#"
[machine.targets.x]
machine = "ssh:x"
remote_root = '%TEMP%\ralphus'
allow_ephemeral_remote_root = true
"#,
        )
        .expect("write config");

        let targets = parse_targets_file(&file).expect("parse");
        assert_eq!(targets["x"].remote_root, "%TEMP%\\ralphus");
    }

    #[test]
    fn load_machine_targets_with_reads_configuration_path_entries() {
        let config_dir = tempdir("configuration-path-source");
        let config_file = config_dir.join(".ralphus.toml");
        fs::write(
            &config_file,
            r#"
[machine.targets.from-configuration-path]
machine = "ssh:from-configuration-path"
remote_root = "/srv/ralphus"
"#,
        )
        .expect("write configuration-path file");

        let targets =
            load_machine_targets_with(Some(config_file.to_str().unwrap())).expect("load targets");
        assert!(targets.contains_key("from-configuration-path"));
    }

    #[test]
    fn later_configuration_path_entry_wins_on_name_collision() {
        let first_dir = tempdir("collision-first");
        let first = first_dir.join(".ralphus.toml");
        fs::write(
            &first,
            r#"
[machine.targets.shared]
machine = "ssh:first"
remote_root = "/srv/first"
"#,
        )
        .expect("write first");

        let second_dir = tempdir("collision-second");
        let second = second_dir.join(".ralphus.toml");
        fs::write(
            &second,
            r#"
[machine.targets.shared]
machine = "ssh:second"
remote_root = "/srv/second"
"#,
        )
        .expect("write second");

        let path_value = std::env::join_paths([&first, &second])
            .expect("join paths")
            .into_string()
            .expect("utf8 path list");
        let targets = load_machine_targets_with(Some(&path_value)).expect("load targets");
        assert_eq!(targets["shared"].machine, "ssh:second");
    }

    #[test]
    fn find_by_machine_returns_none_for_an_unconfigured_machine() {
        let targets = BTreeMap::new();
        assert!(find_by_machine(&targets, "ssh:nope").is_none());
    }

    #[test]
    fn find_by_machine_matches_the_full_machine_value() {
        let mut targets = BTreeMap::new();
        targets.insert(
            "devbox".to_string(),
            MachineTarget {
                name: "devbox".to_string(),
                machine: "ssh:devbox".to_string(),
                remote_root: "/srv/ralphus".to_string(),
                runner_mode: RunnerMode::Installed,
                runner_command: DEFAULT_RUNNER_COMMAND.to_string(),
                runner_artifacts: BTreeMap::new(),
                agent_executables: BTreeMap::new(),
            },
        );
        let found = find_by_machine(&targets, "ssh:devbox").expect("should find devbox");
        assert_eq!(found.name, "devbox");
        // A bare uri half or a different scheme must not match.
        assert!(find_by_machine(&targets, "devbox").is_none());
        assert!(find_by_machine(&targets, "incredibuild:devbox").is_none());
    }
}
