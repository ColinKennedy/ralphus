//! RAL-565: suggested manual checks in a project's `.ralphus.toml`.
//!
//! A project file is not a task submission. Its `[[review.action]]` tables
//! describe manual checks the project's maintainers recommend, and each may
//! carry `[[review.action.hint]]` tables saying when the check applies. They
//! inform whoever writes the real submission -- the auto-review agent,
//! `ralphus tutor`, a person -- and are never run, merged into a submission, or
//! read by the daemon's scheduler. A submission that defines its own
//! `[[review.action]]` checks is authoritative; layering the project's on top
//! would break "Run all" with a minimal set of checks whenever the submission
//! and the project differ even slightly.
//!
//! A suggested action has the same shape as a submitted `[[review.action]]`
//! (`label`, `command`, `prepare`, `input`, ...), so copying it is mechanical,
//! plus `hint` tables:
//!
//! ```toml
//! [[review.action]]
//! label = "Board in an isolated dev stack"
//! command = "bash scripts/build-debug.sh --daemon-port {daemon_port}"
//! auto_run = false
//!
//! [[review.action.input]]
//! name = "daemon_port"
//! message = "Port for the second daemon"
//! default = "7891"
//!
//! [[review.action.prepare]]
//! command = "npm ci"
//!
//! [[review.action.hint]]
//! include_when = "Any change to the web board."
//! paths = ["librarian/**"]
//! ```
//!
//! A `[[review.action.prepare.hint]]` may say when one prepare step is needed,
//! but an action that applies almost always needs all of its steps, so it is
//! rarely worth writing.
//!
//! Suggestions are gathered from the nearest `.ralphus.toml` up through every
//! parent directory. For one `label`, the file nearest the start directory wins
//! and a parent only fills labels the nearer files did not mention.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::schema::{PreparationStepDef, ReviewActionInputDef};

/// A rule for when a suggested action (or one of its prepare steps) applies.
/// An action with several `hint` tables applies when any one matches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HintRule {
    /// Plain-language rule, e.g. "Any change to X GUI should include this
    /// check." Read by an agent or a person.
    #[serde(default)]
    pub include_when: Option<String>,
    /// Optional path globs (`*` within a segment, `**` across segments) that
    /// make the rule checkable against a diff.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// A suggested prepare step, optionally with a rule for when it is needed.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SuggestedPrepare {
    /// The step itself, in the same shape as `[[review.action.prepare]]`.
    #[serde(flatten)]
    pub step: PreparationStepDef,
    /// When this step is needed; rarely written, see the module docs.
    #[serde(default)]
    pub hint: Vec<HintRule>,
}

/// One suggested manual check.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestedAction {
    /// Identity of the suggestion. A nearer file's action replaces a parent's
    /// with the same label.
    pub label: String,
    /// Suggested shell command, with `{name}` placeholders for its inputs; the
    /// submitter copies it into a real `[[review.action]]`.
    #[serde(default)]
    pub command: Option<String>,
    /// Suggested prompt, as an alternative to `command`.
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub success: Option<String>,
    #[serde(default)]
    pub cleanup_command: Option<String>,
    /// Whether the check suits auto-run. `false` means it should not be
    /// offered for auto-run (it is interactive, destructive, or heavy).
    #[serde(default)]
    pub auto_run: Option<bool>,
    /// Named, defaulted values the command refers to as `{name}`, editable in
    /// the UI just before the check runs.
    #[serde(default)]
    pub input: Vec<ReviewActionInputDef>,
    /// Suggested build steps.
    #[serde(default)]
    pub prepare: Vec<SuggestedPrepare>,
    /// When the check applies.
    #[serde(default)]
    pub hint: Vec<HintRule>,
}

/// A suggested action and the file it came from.
#[derive(Debug, Clone)]
pub struct CollectedHint {
    pub action: SuggestedAction,
    pub source: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct ProjectFile {
    review: Option<ReviewSection>,
}

#[derive(Debug, Default, Deserialize)]
struct ReviewSection {
    #[serde(default)]
    action: Vec<SuggestedAction>,
}

/// Parse the suggested actions out of one `.ralphus.toml`'s text. A file with
/// no `[[review.action]]` yields an empty list; any other section is ignored.
///
/// # Errors
/// Returns a message when the TOML is malformed, an action has an unknown key,
/// an empty or repeated `label`, both a `command` and a `prompt`, or a `{name}`
/// placeholder with no matching `input`, or a repeated input `name`.
pub fn parse_project_hints(text: &str) -> Result<Vec<SuggestedAction>, String> {
    let file: ProjectFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let actions = file.review.map(|review| review.action).unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    for action in &actions {
        if action.label.trim().is_empty() {
            return Err("a [[review.action]] has an empty `label`".to_string());
        }
        if action.command.is_some() && action.prompt.is_some() {
            return Err(format!(
                "action \"{}\" sets both `command` and `prompt`; set at most one",
                action.label
            ));
        }
        if !seen.insert(action.label.clone()) {
            return Err(format!(
                "action label \"{}\" appears more than once in this file",
                action.label
            ));
        }
        let mut names = std::collections::HashSet::new();
        for input in &action.input {
            if !names.insert(input.name.as_str()) {
                return Err(format!(
                    "action \"{}\" declares input \"{}\" more than once",
                    action.label, input.name
                ));
            }
        }
        for text in [&action.command, &action.cleanup_command]
            .into_iter()
            .flatten()
        {
            for name in placeholders(text) {
                if !names.contains(name.as_str()) {
                    return Err(format!(
                        "action \"{}\" uses {{{name}}} but declares no input named \"{name}\"",
                        action.label
                    ));
                }
            }
        }
    }
    Ok(actions)
}

/// The `{name}` placeholders in `text`: a brace pair around a word of letters,
/// digits, `_` or `-` that starts with a letter or `_`.
fn placeholders(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            break;
        };
        let name = &after[..close];
        let mut chars = name.chars();
        let looks_like_a_name = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        // `${HOME}` is shell expansion, not one of the check's inputs.
        let shell_expansion = rest[..open].ends_with('$');
        if looks_like_a_name && !shell_expansion && !found.iter().any(|f| f == name) {
            found.push(name.to_string());
        }
        rest = &after[close + 1..];
    }
    found
}

/// Gather suggested actions from the `.ralphus.toml` at or above `start`,
/// nearest file first, keeping the nearest one for each label. A file that
/// cannot be read or parsed contributes a warning instead of hiding the files
/// above it.
#[must_use]
pub fn collect_project_hints(start: &Path) -> (Vec<CollectedHint>, Vec<String>) {
    let mut collected: Vec<CollectedHint> = Vec::new();
    let mut warnings = Vec::new();
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(".ralphus.toml");
        if candidate.is_file() {
            match std::fs::read_to_string(&candidate) {
                Err(e) => warnings.push(format!("{}: {e}", candidate.display())),
                Ok(text) => match parse_project_hints(&text) {
                    Err(e) => warnings.push(format!("{}: {e}", candidate.display())),
                    Ok(actions) => {
                        for action in actions {
                            if !collected.iter().any(|c| c.action.label == action.label) {
                                collected.push(CollectedHint {
                                    action,
                                    source: candidate.clone(),
                                });
                            }
                        }
                    }
                },
            }
        }
        dir = d.parent();
    }
    (collected, warnings)
}

/// Render suggestions as a block of guidance for an agent or a person. Empty
/// when there are none. The wording states that they inform the submission and
/// are not part of it, and asks for consistent parameters across checks.
#[must_use]
pub fn render_hints(hints: &[CollectedHint]) -> String {
    if hints.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "Project manual-check suggestions. These come from the project's .ralphus.toml, \
         nearest file first; they inform your [[review.action]] entries and are not part of a \
         submission themselves. Include a suggestion's check when one of its hints applies to \
         the change, copy its command, inputs and prepare steps into a real [[review.action]], \
         and do not set auto_run = true on a check whose suggestion says auto_run = false. \
         Look at every suggestion you include together: checks that share a parameter (the \
         same port, path, branch name) must declare the same input name with the same default \
         in each of them, so one value is consistent across all of the review's checks.\n",
    );
    for collected in hints {
        let action = &collected.action;
        out.push_str(&format!("\n- {}", action.label));
        if let Some(command) = &action.command {
            out.push_str(&format!("\n  command: {command}"));
        }
        if let Some(prompt) = &action.prompt {
            out.push_str(&format!("\n  prompt: {prompt}"));
        }
        for input in &action.input {
            out.push_str(&format!(
                "\n  input: {} (default \"{}\") -- {}",
                input.name, input.default, input.message
            ));
        }
        for prepare in &action.prepare {
            if let Some(command) = &prepare.step.command {
                out.push_str(&format!("\n  prepare: {}", command.commands().join(" && ")));
            }
            for rule in &prepare.hint {
                out.push_str(&format!("\n    needed when: {}", describe_rule(rule)));
            }
        }
        if let Some(auto_run) = action.auto_run {
            out.push_str(&format!("\n  auto_run: {auto_run}"));
        }
        if let Some(description) = &action.description {
            out.push_str(&format!("\n  description: {description}"));
        }
        if let Some(success) = &action.success {
            out.push_str(&format!("\n  success: {success}"));
        }
        for rule in &action.hint {
            out.push_str(&format!("\n  include when: {}", describe_rule(rule)));
        }
        out.push_str(&format!("\n  from: {}", collected.source.display()));
    }
    out
}

/// One rule as a line of text: its prose, then its path globs.
fn describe_rule(rule: &HintRule) -> String {
    let mut parts = Vec::new();
    if let Some(text) = &rule.include_when {
        parts.push(text.clone());
    }
    if !rule.paths.is_empty() {
        parts.push(format!("paths {}", rule.paths.join(", ")));
    }
    parts.join("; ")
}

/// Whether `path` (forward-slash separated, relative) matches `glob`, where `*`
/// matches within one path segment and `**` matches across segments.
#[must_use]
pub fn path_matches(glob: &str, path: &str) -> bool {
    fn go(glob: &[u8], path: &[u8]) -> bool {
        match (glob.first(), path.first()) {
            (None, None) => true,
            (Some(b'*'), _) if glob.get(1) == Some(&b'*') => {
                let rest = &glob[2..];
                let rest = rest.strip_prefix(b"/").unwrap_or(rest);
                (0..=path.len()).any(|i| go(rest, &path[i..]))
            }
            (Some(b'*'), _) => {
                let rest = &glob[1..];
                (0..=path.len())
                    .take_while(|&i| i == 0 || path[i - 1] != b'/')
                    .any(|i| go(rest, &path[i..]))
            }
            (Some(g), Some(p)) if g == p => go(&glob[1..], &path[1..]),
            _ => false,
        }
    }
    go(glob.as_bytes(), path.as_bytes())
}

/// The suggestions with a `hint` whose `paths` match at least one of
/// `changed_paths`. A hint with no `paths` is never selected here -- it carries
/// only prose for an agent to judge.
#[must_use]
pub fn hints_for_paths<'a>(
    hints: &'a [CollectedHint],
    changed_paths: &[&str],
) -> Vec<&'a CollectedHint> {
    hints
        .iter()
        .filter(|collected| {
            collected.action.hint.iter().any(|rule| {
                rule.paths
                    .iter()
                    .any(|glob| changed_paths.iter().any(|path| path_matches(glob, path)))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[review]
auto_run = false

[[review.action]]
label = "GUI smoke test"
command = "npm test -- --port {port}"
auto_run = true

[[review.action.input]]
name = "port"
message = "Port to serve on"
default = "7891"

[[review.action.prepare]]
command = "npm ci"

[[review.action.prepare.hint]]
include_when = "Only when package-lock.json changed."

[[review.action.hint]]
include_when = "Any change under librarian/assets/ should include this check."
paths = ["librarian/assets/**"]

[[review.action.hint]]
include_when = "A color documented in docs/colors.md changed."
paths = ["docs/colors.md"]

[[review.action]]
label = "Slow soak"
prompt = "Run the soak test"
auto_run = false
"#;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-hints-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn collected(text: &str) -> Vec<CollectedHint> {
        parse_project_hints(text)
            .unwrap()
            .into_iter()
            .map(|action| CollectedHint {
                action,
                source: PathBuf::from("/repo/.ralphus.toml"),
            })
            .collect()
    }

    #[test]
    fn parses_actions_with_inputs_prepare_and_hints() {
        let actions = parse_project_hints(SAMPLE).unwrap();
        assert_eq!(actions.len(), 2);
        let first = &actions[0];
        assert_eq!(first.label, "GUI smoke test");
        assert_eq!(first.command.as_deref(), Some("npm test -- --port {port}"));
        assert_eq!(first.auto_run, Some(true));
        assert_eq!(first.input.len(), 1);
        assert_eq!(first.input[0].name, "port");
        assert_eq!(first.input[0].default, "7891");
        assert_eq!(first.prepare.len(), 1);
        assert_eq!(first.prepare[0].hint.len(), 1);
        assert_eq!(first.hint.len(), 2);
        assert_eq!(first.hint[0].paths, vec!["librarian/assets/**".to_string()]);
        assert!(
            first.hint[0]
                .include_when
                .as_deref()
                .unwrap()
                .contains("librarian")
        );
        assert_eq!(actions[1].auto_run, Some(false));
    }

    #[test]
    fn a_file_without_actions_yields_none_and_other_sections_are_ignored() {
        assert!(
            parse_project_hints("[review]\nauto_run = true\n")
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_project_hints("[agent.profiles.x]\nbackend = \"codex\"\n")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_bad_actions() {
        let bad =
            |body: &str| parse_project_hints(&format!("[[review.action]]\n{body}")).unwrap_err();
        assert!(bad("label = \"a\"\ncomand = \"x\"\n").contains("comand"));
        assert!(bad("label = \" \"\n").contains("empty"));
        assert!(bad("label = \"a\"\ncommand = \"x\"\nprompt = \"y\"\n").contains("at most one"));
        assert!(
            bad("label = \"a\"\ncommand = \"serve {port}\"\n").contains("no input named \"port\"")
        );
        let twice = "[[review.action]]\nlabel = \"a\"\n[[review.action]]\nlabel = \"a\"\n";
        assert!(
            parse_project_hints(twice)
                .unwrap_err()
                .contains("more than once")
        );
        let dup_input = "[[review.action]]\nlabel = \"a\"\n\
             [[review.action.input]]\nname = \"p\"\nmessage = \"m\"\n\
             [[review.action.input]]\nname = \"p\"\nmessage = \"m\"\n";
        assert!(
            parse_project_hints(dup_input)
                .unwrap_err()
                .contains("input \"p\" more than once")
        );
    }

    #[test]
    fn the_old_dotted_hint_form_is_rejected() {
        let old = "[[review.action]]\nlabel = \"a\"\nhint.paths = [\"x\"]\n";
        assert!(parse_project_hints(old).is_err());
    }

    #[test]
    fn braces_that_are_not_placeholders_are_left_alone() {
        let ok = "[[review.action]]\nlabel = \"a\"\ncommand = \"awk '{ print $1 }' f; echo ${HOME} {1}\"\n";
        assert!(parse_project_hints(ok).is_ok());
        assert_eq!(placeholders("a {x} {y-z} {x} { no } {1}"), vec!["x", "y-z"]);
    }

    #[test]
    fn the_nearest_file_wins_per_label_and_parents_fill_the_rest() {
        let root = scratch("walk");
        let leaf = root.join("pkg").join("ui");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(
            root.join(".ralphus.toml"),
            "[[review.action]]\nlabel = \"shared\"\ncommand = \"parent-shared\"\n\
             [[review.action]]\nlabel = \"only-parent\"\ncommand = \"parent-only\"\n",
        )
        .unwrap();
        std::fs::write(
            leaf.join(".ralphus.toml"),
            "[[review.action]]\nlabel = \"shared\"\ncommand = \"leaf-shared\"\n",
        )
        .unwrap();
        let (hints, warnings) = collect_project_hints(&leaf);
        assert!(warnings.is_empty(), "{warnings:?}");
        let by_label = |label: &str| {
            hints
                .iter()
                .find(|c| c.action.label == label)
                .and_then(|c| c.action.command.clone())
        };
        assert_eq!(by_label("shared").as_deref(), Some("leaf-shared"));
        assert_eq!(by_label("only-parent").as_deref(), Some("parent-only"));
        assert_eq!(hints.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_broken_nearer_file_warns_but_does_not_hide_its_parent() {
        let root = scratch("broken");
        let leaf = root.join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(
            root.join(".ralphus.toml"),
            "[[review.action]]\nlabel = \"ok\"\n",
        )
        .unwrap();
        std::fs::write(leaf.join(".ralphus.toml"), "[[review.action]]\nlabel = \n").unwrap();
        let (hints, warnings) = collect_project_hints(&leaf);
        assert_eq!(hints.len(), 1);
        assert_eq!(warnings.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rendered_suggestions_say_they_inform_a_submission_and_ask_for_consistent_inputs() {
        let text = render_hints(&collected(SAMPLE));
        assert!(text.contains("not part of a submission"));
        assert!(text.contains("same input name with the same default"));
        assert!(text.contains("GUI smoke test"));
        assert!(text.contains("input: port (default \"7891\") -- Port to serve on"));
        assert!(text.contains("include when: Any change under librarian/assets/"));
        assert!(text.contains(
            "include when: A color documented in docs/colors.md changed.; paths docs/colors.md"
        ));
        assert!(text.contains("needed when: Only when package-lock.json changed."));
        assert!(text.contains("auto_run: false"));
        assert_eq!(render_hints(&[]), "");
    }

    #[test]
    fn ralphus_own_project_file_parses_and_every_suggestion_says_when_it_applies() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.ralphus.toml");
        let text = std::fs::read_to_string(&path).expect("the repository's .ralphus.toml");
        let actions = parse_project_hints(&text).unwrap();
        assert!(!actions.is_empty());
        for action in &actions {
            assert!(
                !action.hint.is_empty()
                    && action
                        .hint
                        .iter()
                        .all(|rule| rule.include_when.is_some() && !rule.paths.is_empty()),
                "action \"{}\" must say when it applies, in prose and as path globs",
                action.label
            );
            assert!(
                action.command.is_some() || action.prompt.is_some(),
                "action \"{}\" suggests nothing to run",
                action.label
            );
            assert!(
                action.auto_run.is_some(),
                "action \"{}\" must state whether it suits auto_run",
                action.label
            );
        }
        let (collected, warnings) = collect_project_hints(path.parent().unwrap());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(collected.len(), actions.len());
    }

    #[test]
    fn ralphus_stack_checks_share_input_names_and_defaults() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.ralphus.toml");
        let actions = parse_project_hints(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for action in &actions {
            for input in &action.input {
                let default = seen
                    .entry(input.name.clone())
                    .or_insert_with(|| input.default.clone());
                assert_eq!(
                    *default, input.default,
                    "input \"{}\" has a different default in \"{}\"",
                    input.name, action.label
                );
            }
        }
        assert!(seen.contains_key("daemon_port") && seen.contains_key("librarian_port"));
    }

    #[test]
    fn path_globs_match_within_and_across_segments() {
        assert!(path_matches(
            "librarian/assets/**",
            "librarian/assets/board/65-reviews.js"
        ));
        assert!(path_matches(
            "librarian/assets/board/*.js",
            "librarian/assets/board/65-reviews.js"
        ));
        assert!(!path_matches(
            "librarian/assets/board/*.js",
            "librarian/assets/board/sub/x.js"
        ));
        assert!(path_matches("**/schema.rs", "core/src/schema.rs"));
        assert!(!path_matches("daemon/**", "core/src/schema.rs"));
    }

    #[test]
    fn suggestions_are_selected_by_changed_paths() {
        let all = collected(SAMPLE);
        let hit = hints_for_paths(&all, &["librarian/assets/board/65-reviews.js"]);
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].action.label, "GUI smoke test");
        let by_second_rule = hints_for_paths(&all, &["docs/colors.md"]);
        assert_eq!(by_second_rule.len(), 1);
        assert!(hints_for_paths(&all, &["daemon/src/server.rs"]).is_empty());
    }
}
