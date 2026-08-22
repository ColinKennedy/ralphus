//! The `codex` `ModelBackend`, ported from
//! `cli/src/ralphus/runner/codex_backend.py`. Drives `codex exec`
//! non-interactively, prompt piped over stdin, parsing its JSONL
//! `ThreadEvent` stream on stdout.

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::Value;

use crate::backend::{BackendError, BackendOutcome, ModelBackend, RunOptions};
use crate::cli_agent_common::{live_session_path, write_live_session_id};
use crate::shellcmd::{self, Env, SpawnArgs};
use crate::tools::Workspace;

const DEFAULT_PROGRAM: &str = "codex";

pub struct CodexBackend {
    pub keep_temporary_files: bool,
    pub program_override: Option<String>,
}

impl ModelBackend for CodexBackend {
    fn run(
        &self,
        prompt: &str,
        workspace: &Workspace,
        options: &RunOptions<'_>,
    ) -> Result<BackendOutcome, BackendError> {
        let program = self.program_override.clone().unwrap_or_else(|| {
            std::env::var("RALPHUS_CODEX_COMMAND").unwrap_or_else(|_| DEFAULT_PROGRAM.to_string())
        });
        let compound = crate::cli_agent_common::is_compound_command(&program);

        // `-c developer_instructions=...` must precede `exec` -- Codex's own
        // root-level arg parser is the only one that recognizes it; `exec`'s
        // subcommand struct deliberately does not re-declare it.
        let mut args: Vec<String> = Vec::new();
        if let Some(sp) = options.append_system_prompt {
            args.push("-c".to_string());
            args.push(format!("developer_instructions={sp}"));
        }
        args.push("exec".to_string());
        args.push("--json".to_string());
        args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
        args.push("--skip-git-repo-check".to_string());
        args.push("-C".to_string());
        args.push(workspace.root().display().to_string());
        if let Some(model) = options.model {
            args.push("-m".to_string());
            args.push(model.to_string());
        }
        if let Some(id) = options.resume_agent_session_id {
            args.push("resume".to_string());
            args.push(id.to_string());
        }
        args.push("-".to_string());

        let mut child = spawn(&program, compound, &args, workspace)
            .map_err(|e| BackendError(format!("could not spawn {program}: {e}")))?;

        // Human-readable header for the live tmux pane (RAL-102) -- everything
        // below this is Codex's own text/tool activity, not runner logging.
        print_header(options.model, workspace);

        // Stdin is written on its own thread, same reason as the daemon's
        // background stderr drain elsewhere: writing then closing stdin
        // could otherwise deadlock against Codex's own stdout/stderr filling
        // up before it starts reading.
        if let Some(mut stdin) = child.stdin.take() {
            let prompt = prompt.to_string();
            std::thread::spawn(move || {
                let _ = stdin.write_all(prompt.as_bytes());
            });
        }

        let outcome = drive_thread_events(&mut child, workspace, options.timeout_sec);

        if !self.keep_temporary_files {
            let _ = std::fs::remove_file(live_session_path(workspace.root()));
        }

        outcome
    }
}

fn spawn(
    program: &str,
    compound: bool,
    args: &[String],
    workspace: &Workspace,
) -> std::io::Result<Child> {
    if compound {
        let shell = shellcmd::resolve_shell(None);
        let _ = shellcmd::detect_parent_shell(&Env::from_process());
        let line = shellcmd::build_compound_command_line(&shell, program, args);
        match shellcmd::shell_spawn_args(&shell, &line) {
            SpawnArgs::RawShellLine(raw) => Command::new("cmd")
                .arg("/C")
                .arg(raw)
                .current_dir(workspace.root())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn(),
            SpawnArgs::Argv(argv) => {
                let mut cmd = Command::new(&argv[0]);
                cmd.args(&argv[1..]);
                cmd.current_dir(workspace.root())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            }
        }
    } else {
        Command::new(program)
            .args(args)
            .current_dir(workspace.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }
}

fn drive_thread_events(
    child: &mut Child,
    workspace: &Workspace,
    timeout_sec: Option<u64>,
) -> Result<BackendOutcome, BackendError> {
    let stderr = child.stderr.take();
    let stderr_thread = stderr.map(|s| {
        std::thread::spawn(move || {
            let reader = BufReader::new(s);
            for line in reader.lines().map_while(Result::ok) {
                crate::cartographer::emit(
                    "codex",
                    &line,
                    "debug",
                    crate::cartographer::EventContext::default(),
                    Value::Null,
                );
            }
        })
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| BackendError("codex: no stdout pipe".to_string()))?;
    let reader = BufReader::new(stdout);

    let mut agent_session_id: Option<String> = None;
    let mut latest_agent_message = String::new();
    let mut tokens_in = 0i64;
    let mut tokens_out = 0i64;
    let mut turn_error: Option<String> = None;
    let mut saw_turn = false;

    for line in reader.lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("thread.started") => {
                if let Some(id) = event["thread_id"].as_str() {
                    agent_session_id = Some(id.to_string());
                    write_live_session_id(workspace.root(), id);
                    crate::cartographer::emit(
                        "codex",
                        "thread-id known",
                        "info",
                        crate::cartographer::EventContext::default(),
                        serde_json::json!({"agent_session_id": id}),
                    );
                }
            }
            Some("item.completed") => {
                let item = &event["item"];
                match item["type"].as_str() {
                    Some("agent_message") => {
                        // The model's own reply -- the whole point of the
                        // live tmux pane is to let a human read this.
                        if let Some(text) = item["text"].as_str() {
                            if !text.is_empty() {
                                latest_agent_message = text.to_string();
                                print_line(text);
                            }
                        }
                    }
                    Some("command_execution") => {
                        let command = item["command"].as_str().unwrap_or("");
                        let status = item["status"].as_str().unwrap_or("");
                        eprintln!("[tool] exec({command:?}) status={status}");
                    }
                    Some("error") => {
                        let message = item["message"].as_str().unwrap_or("");
                        eprintln!("[error] {message}");
                    }
                    _ => {}
                }
            }
            Some("turn.completed") => {
                saw_turn = true;
                // Accumulate (not overwrite) -- a multi-turn `codex exec` run
                // reports per-turn usage; summing is what makes a long
                // session's token count reflect total spend (RAL-187).
                tokens_in += event["usage"]["input_tokens"].as_i64().unwrap_or(0);
                tokens_out += event["usage"]["output_tokens"].as_i64().unwrap_or(0);
            }
            Some("turn.failed") => {
                turn_error = event["error"]["message"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| Some("codex turn failed".to_string()));
            }
            Some("error") => {
                turn_error = event["message"].as_str().map(str::to_string);
            }
            _ => {}
        }
    }

    let status =
        wait_for_child(child, timeout_sec).map_err(|e| BackendError(format!("codex: {e}")))?;
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }

    if let Some(err) = turn_error {
        return Err(BackendError(err));
    }
    if !saw_turn {
        return Err(BackendError(format!(
            "codex exited ({status:?}) without a completed turn"
        )));
    }

    Ok(BackendOutcome {
        summary: latest_agent_message,
        tokens_in,
        tokens_out,
        // No cost field at all in Codex's own output -- the board renders
        // this as "N/A" rather than "$0.0000" so it never misreads as free
        // (RAL-187).
        cost_usd: 0.0,
        agent_session_id,
    })
}

/// Human-readable header for the live tmux pane (RAL-102) -- printed once,
/// before Codex's own streamed text/tool activity below it.
#[allow(clippy::print_stdout)] // intentional: this process runs tmux-wrapped (see `daemon/src/runner.rs::run_via_tmux`), so stdout is the live pane, not the daemon<->runner JSON channel (that contract is file-based here -- see `main.rs`'s `--result-file` handling)
fn print_header(model: Option<&str>, workspace: &Workspace) {
    println!(
        "Codex · model={}\ncwd: {}\n",
        model.unwrap_or("default"),
        workspace.root().display()
    );
}

/// Prints one of Codex's own agent-message replies to the live tmux pane --
/// the whole point of RAL-102 is to let a human read this.
#[allow(clippy::print_stdout)] // intentional: see `print_header`
fn print_line(text: &str) {
    println!("{text}");
}

fn wait_for_child(
    child: &mut Child,
    timeout_sec: Option<u64>,
) -> std::io::Result<std::process::ExitStatus> {
    let Some(secs) = timeout_sec else {
        return child.wait();
    };
    let deadline = Duration::from_secs(secs);
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            return child.wait();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
