//! RAL-469: backends run from the project while their personal homes stay
//! redirected. The stand-in CLIs inspect the process that each real backend
//! launches, rather than merely testing argument construction.

#![cfg(windows)]

use std::path::{Path, PathBuf};

use ralphus_runner::backend::{ModelBackend, RunOptions};
use ralphus_runner::claude_code_backend::ClaudeCodeBackend;
use ralphus_runner::codex_backend::CodexBackend;
use ralphus_runner::pi_backend::PiBackend;
use ralphus_runner::tools::Workspace;

const CLAUDE_MARKER: &str = "RAL469_PROJECT_CLAUDE";
const AGENTS_MARKER: &str = "RAL469_PROJECT_AGENTS";
const PERSONAL_MARKER: &str = "RAL469_PERSONAL_SETTINGS";
const CHILD_ENV: &str = "RALPHUS_RAL469_INSTRUCTION_PROBE_CHILD";
const ROOT_ENV: &str = "RALPHUS_RAL469_INSTRUCTION_PROBE_ROOT";

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// A stand-in agent executable. It reports what the child can actually see,
/// then emits the smallest successful event stream understood by the selected
/// backend.
fn write_probe(dir: &Path) -> (PathBuf, PathBuf) {
    let script = dir.join("instruction-probe.js");
    let report = dir.join("instruction-report.json");
    std::fs::write(
        &script,
        format!(
            r#"
const fs = require("fs");
const args = process.argv.slice(2);
const kind = args.includes("--setting-sources") ? "claude" :
  args.includes("exec") ? "codex" : "pi";
const home = process.env[kind === "claude" ? "CLAUDE_CONFIG_DIR" :
  kind === "codex" ? "CODEX_HOME" : "PI_CODING_AGENT_DIR"];
const settingsFile = kind === "claude" ? "settings.json" :
  kind === "codex" ? "config.toml" : "settings.json";
fs.writeFileSync({report:?}, JSON.stringify({{
  kind,
  args,
  claude: fs.readFileSync("CLAUDE.md", "utf8"),
  agents: fs.readFileSync("AGENTS.md", "utf8"),
  home,
  personalSettingsPresent: home ? fs.existsSync(require("path").join(home, settingsFile)) : true
}}, null, 2));
if (kind === "claude") {{
  process.stdout.write('{{"type":"result","subtype":"success","result":"ok","usage":{{"input_tokens":1,"output_tokens":1}}}}\n');
}} else if (kind === "codex") {{
  process.stdout.write('{{"type":"thread.started","thread_id":"ral469"}}\n');
  process.stdout.write('{{"type":"turn.completed","usage":{{"input_tokens":1,"output_tokens":1}}}}\n');
}} else {{
  process.stdout.write('{{"type":"agent_end","messages":[{{"role":"assistant","content":[{{"type":"text","text":"ok"}}]}}]}}\n');
}}
"#,
            report = report.display().to_string(),
        ),
    )
    .expect("write probe script");
    let shim = dir.join("instruction-probe.cmd");
    std::fs::write(
        &shim,
        format!("@echo off\r\nnode \"{}\" %*\r\n", script.display()),
    )
    .expect("write probe shim");
    (shim, report)
}

fn assert_project_only(report: &Path, expected_kind: &str) {
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(report).expect("probe report"))
            .expect("probe JSON");
    assert_eq!(report["kind"], expected_kind);
    assert_eq!(report["claude"], CLAUDE_MARKER);
    assert_eq!(report["agents"], AGENTS_MARKER);
    assert!(report["home"].as_str().is_some_and(|home| !home.is_empty()));
    assert_eq!(report["personalSettingsPresent"], false);
}

#[test]
fn backends_surface_project_instructions_without_personal_instruction_homes() {
    if !node_available() {
        eprintln!("SKIP: node is not on PATH");
        return;
    }
    let root = std::env::var_os(ROOT_ENV).map_or_else(
        || std::env::temp_dir().join(format!("ralphus-ral469-{}", std::process::id())),
        PathBuf::from,
    );
    if std::env::var_os(CHILD_ENV).is_none() {
        std::fs::create_dir_all(&root).expect("test root");
        let personal = root.join("personal-settings");
        for directory in ["claude", "codex", "pi"] {
            std::fs::create_dir_all(personal.join(directory)).expect("personal settings directory");
        }
        std::fs::write(personal.join("claude/settings.json"), PERSONAL_MARKER)
            .expect("Claude personal settings");
        std::fs::write(personal.join("codex/config.toml"), PERSONAL_MARKER)
            .expect("Codex personal settings");
        std::fs::write(personal.join("pi/settings.json"), PERSONAL_MARKER)
            .expect("Pi personal settings");
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "backends_surface_project_instructions_without_personal_instruction_homes",
            ])
            .env(CHILD_ENV, "1")
            .env(ROOT_ENV, &root)
            .env("CLAUDE_CONFIG_DIR", personal.join("claude"))
            .env("CODEX_HOME", personal.join("codex"))
            .env("PI_CODING_AGENT_DIR", personal.join("pi"))
            .status()
            .expect("run isolated probe child");
        let _ = std::fs::remove_dir_all(&root);
        assert!(status.success(), "probe child failed: {status}");
        return;
    }
    std::fs::create_dir_all(&root).expect("test root");
    std::fs::write(root.join("CLAUDE.md"), CLAUDE_MARKER).expect("project CLAUDE.md");
    std::fs::write(root.join("AGENTS.md"), AGENTS_MARKER).expect("project AGENTS.md");
    let (shim, report) = write_probe(&root);
    let workspace = Workspace::create(&root).expect("workspace");
    let program = shim.display().to_string();

    ClaudeCodeBackend {
        keep_temporary_files: false,
        program_override: Some(program.clone()),
    }
    .run("probe", &workspace, &RunOptions::default())
    .expect("Claude probe run");
    assert_project_only(&report, "claude");

    CodexBackend {
        keep_temporary_files: false,
        program_override: Some(program.clone()),
    }
    .run("probe", &workspace, &RunOptions::default())
    .expect("Codex probe run");
    assert_project_only(&report, "codex");

    PiBackend {
        keep_temporary_files: false,
        program_override: Some(program),
    }
    .run("probe", &workspace, &RunOptions::default())
    .expect("Pi probe run");
    assert_project_only(&report, "pi");

    let _ = std::fs::remove_dir_all(root);
}
