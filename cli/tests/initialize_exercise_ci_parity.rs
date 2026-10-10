//! Keeps the guided `ralphus initialize <exercise>` commands wired into CI and
//! free of LLM calls. The rules are in
//! `src/commands/initialize/AGENTS.md`; this is their mechanical enforcement:
//! `EXERCISES`, the exercise modules on disk, the CI runner script, and the CI
//! workflow must all name the same set, in both local and remote modes.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use ralphus_cli::commands::initialize::EXERCISES;

/// Modules in `commands/initialize/` that are not guided exercises.
///
/// `followup` is a documented exemption (see `initialize/AGENTS.md`): the
/// follow-up flow it proves only exists for prompt cells, whose
/// `RALPHUS_PROPHECY:` markers are the only way a `deferred` prophecy is
/// written, and its review merge is a git fast-forward in the daemon's own
/// checkout, so it can follow neither the no-agent rule nor `--remote`. Its
/// stub agent is a renamed copy of the CLI binary, so it still calls no model.
const NON_EXERCISE_MODULES: &[&str] = &["mod", "exercise", "solo_developer", "followup"];

/// Substrings that would mean an exercise reaches for a model or agent.
const LLM_MARKERS: &[&str] = &[
    "claude",
    "anthropic",
    "ollama",
    "codex",
    "openai",
    "\"prompt\"",
    "prompt =",
];

fn cli_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_file(relative: &str) -> String {
    let path = cli_dir().join("..").join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()))
}

fn exercise_source(name: &str) -> String {
    let path = cli_dir().join("src/commands/initialize").join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()))
}

#[test]
fn every_exercise_module_is_listed_and_every_listed_exercise_has_a_module() {
    let dir = cli_dir().join("src/commands/initialize");
    let on_disk: BTreeSet<String> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension()? == "rs").then(|| path.file_stem().unwrap().to_string_lossy().into())
        })
        .filter(|stem: &String| !NON_EXERCISE_MODULES.contains(&stem.as_str()))
        .collect();
    let listed: BTreeSet<String> = EXERCISES.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        on_disk, listed,
        "commands/initialize/ modules and EXERCISES (initialize/mod.rs) disagree"
    );
}

#[test]
fn ci_runner_script_runs_every_exercise_in_both_modes() {
    let script = repo_file("scripts/check-initialize-exercises.sh");
    let line = script
        .lines()
        .find(|l| l.trim_start().starts_with("exercises=\""))
        .expect("scripts/check-initialize-exercises.sh has no exercises=\"...\" line");
    let in_script: BTreeSet<&str> = line.split('"').nth(1).unwrap().split_whitespace().collect();
    let listed: BTreeSet<&str> = EXERCISES.iter().copied().collect();
    assert_eq!(
        in_script, listed,
        "scripts/check-initialize-exercises.sh exercises= list and EXERCISES disagree"
    );
    assert!(
        script.contains("modes=\"local remote\""),
        "the runner script must default to running every exercise locally and --remote"
    );
}

#[test]
fn ci_workflow_runs_the_exercise_script() {
    let ci = repo_file(".github/workflows/ci.yml");
    assert!(
        ci.contains("initialize-exercises:"),
        "ci.yml lost its initialize-exercises job"
    );
    assert!(
        ci.contains("scripts/check-initialize-exercises.sh"),
        "ci.yml must run scripts/check-initialize-exercises.sh"
    );
    assert!(
        !ci.contains("--only"),
        "ci.yml must run every exercise, not a --only subset"
    );
}

#[test]
fn exercises_make_no_llm_calls() {
    let mut files = vec!["exercise.rs".to_string()];
    files.extend(EXERCISES.iter().map(|name| format!("{name}.rs")));
    for file in files {
        let source = exercise_source(&file).to_lowercase();
        for marker in LLM_MARKERS {
            assert!(
                !source.contains(marker),
                "initialize/{file} mentions {marker:?}: exercises must run with no LLM (use raw command cells/proofs)"
            );
        }
    }
}
