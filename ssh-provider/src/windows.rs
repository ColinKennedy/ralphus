//! Windows machines over SSH.
//!
//! Windows' built-in OpenSSH Server runs a session's command through
//! `cmd.exe /c`, and has no POSIX shell to run this provider's scripts. So on
//! a machine whose target runner policy says `"os": "windows"`, every verb
//! except `ping` and `channel` runs on the machine as
//!
//! ```text
//! <runner> machine <verb> --remote-root <root>
//! ```
//!
//! with the verb's JSON request on the session's stdin: `ralphus-runner`
//! carries the verbs' implementation (`runner/src/machine/`), the same code
//! `ralphus-outpost` runs. The reply is the runner's one stdout line, and its
//! `RALPHUS_EVENT:` lines on stderr are forwarded to this process's stderr.
//!
//! The only shell syntax involved is the command line above -- bare tokens
//! that mean the same to `cmd.exe` and PowerShell -- plus, in `upload` mode,
//! one existence check (`if exist`, cmd) and the upload itself: `mkdir` (cmd)
//! for the destination directory, `scp` of the runner to a temporary name next
//! to `<root>\runners\<triple>\<sha256>\ralphus-runner.exe`, and a PowerShell
//! script passed as `-EncodedCommand` (so no quoting reaches either shell)
//! that checks the copy's digest and moves it into place. The runner is
//! uploaded when a verb finds it missing, so the first call pays for the copy
//! and later ones do not.
//!
//! The runner does not travel on the session's stdin: Windows PowerShell 5.1
//! reading `[Console]::OpenStandardInput()` under OpenSSH's sshd blocks
//! forever on anything beyond a few bytes (the same stdin reaches a native
//! program such as `git hash-object --stdin` intact), so the copy goes
//! through the SFTP subsystem Windows' OpenSSH Server ships with.
//!
//! Because nothing is quoted, `remote_root` and an installed runner's path
//! must be made only of letters, digits and `. _ - : \ /` (no spaces).
//! Credentials in a request travel on the session's stdin like every other
//! request field, never on the command line.

use std::io::Write as _;
use std::process::{Command, Stdio};

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::exec::EffectiveConfig;
use crate::runner_install::{self, RunnerPolicy};
use crate::{protocol, ssh, uri};

/// What one command on the machine produced.
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub code: Option<i32>,
}

/// Runs one command line on the machine with `stdin` fed to it.
pub trait RemoteShell {
    /// # Errors
    /// The command could not be delivered (the transport failed).
    fn run(&self, command_line: &str, stdin: &[u8]) -> Result<Output, String>;

    /// Copy the local file `local` to `remote_path` (a drive path with `/`
    /// separators) on the machine.
    ///
    /// # Errors
    /// The copy failed.
    fn copy_in(&self, local: &std::path::Path, remote_path: &str) -> Result<(), String>;
}

/// [`RemoteShell`] over a non-interactive `ssh` session.
pub struct Ssh<'a> {
    pub uri: &'a str,
    pub config: &'a EffectiveConfig,
}

impl RemoteShell for Ssh<'_> {
    fn run(&self, command_line: &str, stdin: &[u8]) -> Result<Output, String> {
        let target = uri::parse(self.uri).map_err(|e| e.to_string())?;
        let args = ssh::command_args(
            &target.target_string(),
            self.config.connect_timeout_secs,
            command_line,
            self.config.ssh_config_file.as_deref(),
        );
        let mut child = Command::new("ssh")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run ssh to reach {target}: {e}"))?;
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(stdin);
        }
        let output = child
            .wait_with_output()
            .map_err(|e| format!("could not wait on ssh to {target}: {e}"))?;
        // ssh itself failing (255) is a transport problem, not the command's.
        if output.status.code() == Some(255) {
            return Err(ssh::interpret_failure(
                "ssh",
                output.status.code(),
                &String::from_utf8_lossy(&output.stderr),
            ));
        }
        Ok(Output {
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            code: output.status.code(),
        })
    }

    fn copy_in(&self, local: &std::path::Path, remote_path: &str) -> Result<(), String> {
        let target = uri::parse(self.uri).map_err(|e| e.to_string())?;
        let output = Command::new("scp")
            .args(scp_args(
                &target.target_string(),
                self.config.connect_timeout_secs,
                self.config.ssh_config_file.as_deref(),
                local,
                remote_path,
            ))
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("could not run scp to reach {target}: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        Err(ssh::interpret_failure(
            "scp",
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
        ))
    }
}

/// `scp` arguments copying `local` to `remote_path` on `target`, with the
/// same non-interactive options as every `ssh` call (less `-T`, which means
/// something else to `scp`).
#[must_use]
pub fn scp_args(
    target: &str,
    connect_timeout_secs: u32,
    config_file: Option<&str>,
    local: &std::path::Path,
    remote_path: &str,
) -> Vec<String> {
    let mut args: Vec<String> = ssh::non_interactive_args(connect_timeout_secs, config_file)
        .into_iter()
        .filter(|a| a != "-T")
        .collect();
    args.push("-q".to_string());
    args.push(local.to_string_lossy().into_owned());
    args.push(format!("{target}:{remote_path}"));
    args
}

/// Verbs this module serves for a Windows machine.
pub const VERBS: &[&str] = &[
    "provision",
    "exec",
    "status",
    "stream",
    "cancel",
    "job-cleanup",
    "run",
    "read-file",
    "write-file",
    "remove-path",
    "materialize",
    "cleanup",
    "capabilities",
    "retire",
    "preflight",
];

/// The machine's OS as the target runner policy declares it.
#[must_use]
pub fn declared_os(config: &EffectiveConfig) -> Option<String> {
    runner_install::policy_from_env(config.target_runner_config.as_deref())
        .ok()
        .flatten()
        .and_then(|p| p.os)
        .map(|os| os.to_ascii_lowercase())
}

/// Whether `verb` against this target is served here.
#[must_use]
pub fn serves(verb: &str, config: &EffectiveConfig) -> bool {
    VERBS.contains(&verb) && declared_os(config).as_deref() == Some("windows")
}

/// Serve `verb` on the Windows machine at `uri`: the reply envelope.
#[must_use]
pub fn serve(
    verb: &str,
    uri: &str,
    handle: Option<&str>,
    since: i64,
    payload: &str,
    config: &EffectiveConfig,
) -> Value {
    let shell = Ssh { uri, config };
    let mut request: Value = if payload.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str(payload) {
            Ok(value) => value,
            Err(e) => return error(&format!("could not parse the {verb} request on stdin: {e}")),
        }
    };
    if let (Some(fields), Some(handle)) = (request.as_object_mut(), handle) {
        fields.insert("handle".into(), handle.into());
        fields.insert("since".into(), since.into());
    }
    match runner_install::policy_from_env(config.target_runner_config.as_deref()) {
        Ok(Some(policy)) => serve_with(&shell, verb, &request, &policy),
        Ok(None) => error("a Windows machine needs a configured target (remote_root and runner)"),
        Err(e) => error(&e),
    }
}

fn error(message: &str) -> Value {
    json!({"ok": false, "protocol_version": protocol::PROTOCOL_VERSION, "error": message})
}

/// [`serve`] over any [`RemoteShell`].
#[must_use]
pub fn serve_with(
    shell: &dyn RemoteShell,
    verb: &str,
    request: &Value,
    policy: &RunnerPolicy,
) -> Value {
    if verb == "retire" {
        return error(&format!(
            "ralphus-ssh-provider does not implement {verb:?}; see docs/machine-providers.md \
             for its supported verbs"
        ));
    }
    let runner = match Runner::resolve(shell, policy) {
        Ok(runner) => runner,
        Err(e) if verb == "capabilities" => return capabilities_without_runner(shell, policy, &e),
        Err(e) => return error(&e),
    };
    let mut reply = match runner.call(shell, verb, request, verb != "capabilities") {
        Ok(reply) => reply,
        Err(e) if verb == "capabilities" => return capabilities_without_runner(shell, policy, &e),
        Err(e) => return error(&e),
    };
    if verb == "materialize" && reply["ok"] == true {
        if let Err(e) = publish_materialized(request, &mut reply) {
            return error(&e);
        }
    }
    reply
}

/// `capabilities` when the runner is not on the machine yet: what is known
/// without it (a probe never uploads anything).
fn capabilities_without_runner(shell: &dyn RemoteShell, policy: &RunnerPolicy, why: &str) -> Value {
    let arch = policy
        .arch
        .clone()
        .or_else(|| probe_arch(shell).ok())
        .map(|a| normalize_arch(&a));
    json!({
        "ok": true,
        "protocol_version": protocol::PROTOCOL_VERSION,
        "capabilities": {
            "os": "windows",
            "arch": arch,
            "supported_ops": VERBS.iter().filter(|v| **v != "retire").chain(["ping"].iter()).collect::<Vec<_>>(),
            "async_exec": true,
            "runner_version": null,
            "detail": why,
        },
    })
}

fn normalize_arch(arch: &str) -> String {
    match arch.trim().to_ascii_lowercase().as_str() {
        "amd64" | "x64" | "x86_64" => "x86_64".to_string(),
        "arm64" | "aarch64" => "aarch64".to_string(),
        other => other.to_string(),
    }
}

/// `%PROCESSOR_ARCHITECTURE%` of the machine (`AMD64`, `ARM64`).
fn probe_arch(shell: &dyn RemoteShell) -> Result<String, String> {
    let output = shell.run("echo %PROCESSOR_ARCHITECTURE%", b"")?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() || text.contains('%') {
        return Err(format!(
            "could not read the machine's architecture (is its OpenSSH default shell cmd.exe?): {}",
            output.stderr.trim()
        ));
    }
    Ok(text)
}

/// The Rust target triple of a Windows runner for `arch`.
fn triple(arch: &str) -> Result<&'static str, String> {
    match normalize_arch(arch).as_str() {
        "x86_64" => Ok("x86_64-pc-windows-msvc"),
        "aarch64" => Ok("aarch64-pc-windows-msvc"),
        other => Err(format!(
            "no Windows runner target for architecture {other:?}"
        )),
    }
}

/// Whether `token` can sit unquoted on a command line that `cmd.exe` and
/// PowerShell both read the same way.
fn safe_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '\\' | '/'))
}

/// A Windows `remote_root` checked for [`safe_token`], with `\` separators.
fn windows_root(policy: &RunnerPolicy) -> Result<String, String> {
    let root = policy.remote_root.trim().trim_end_matches(['/', '\\']);
    let bytes = root.as_bytes();
    let drive = bytes.len() > 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    if !drive || !safe_token(root) || root.split(['/', '\\']).any(|p| p == "..") {
        return Err(format!(
            "remote_root {:?} must be an absolute drive path (C:\\ralphus\\fleet) made only of \
             letters, digits and . _ - \\ / on a Windows machine",
            policy.remote_root
        ));
    }
    Ok(root.replace('/', "\\"))
}

/// Where the runner is on the machine and how to put it there.
struct Runner {
    /// Its path or command, safe to put on a command line unquoted.
    program: String,
    /// The runner artifact on this host and its digest, to upload when
    /// missing (`upload` mode).
    upload: Option<(std::path::PathBuf, String)>,
    root: String,
}

impl Runner {
    fn resolve(shell: &dyn RemoteShell, policy: &RunnerPolicy) -> Result<Self, String> {
        let root = windows_root(policy)?;
        match policy.mode.as_str() {
            "installed" => {
                let program = policy.command.trim().replace('/', "\\");
                if !safe_token(&program) {
                    return Err(format!(
                        "installed runner command {:?} must be a path made only of letters, \
                         digits and . _ - : \\ / on a Windows machine",
                        policy.command
                    ));
                }
                Ok(Self {
                    program,
                    upload: None,
                    root,
                })
            }
            "upload" => {
                let arch = match &policy.arch {
                    Some(arch) => arch.clone(),
                    None => probe_arch(shell)?,
                };
                let triple = triple(&arch)?;
                let artifact = policy.artifacts.get(triple).ok_or_else(|| {
                    format!(
                        "no runner artifact is configured for remote target {triple}; \
                         configured targets: {}",
                        policy
                            .artifacts
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
                let bytes = std::fs::read(artifact)
                    .map_err(|e| format!("could not read runner artifact {artifact}: {e}"))?;
                let digest = format!("{:x}", Sha256::digest(&bytes));
                Ok(Self {
                    program: format!("{root}\\runners\\{triple}\\{digest}\\ralphus-runner.exe"),
                    upload: Some((std::path::PathBuf::from(artifact), digest)),
                    root,
                })
            }
            other => Err(format!(
                "unsupported target runner mode {other:?}; expected \"installed\" or \"upload\""
            )),
        }
    }

    /// Run `verb`, uploading the runner first if the machine lacks it (and
    /// `may_upload`).
    fn call(
        &self,
        shell: &dyn RemoteShell,
        verb: &str,
        request: &Value,
        may_upload: bool,
    ) -> Result<Value, String> {
        let command_line = machine_command(&self.program, verb, &self.root);
        let body = request.to_string();
        let output = shell.run(&command_line, body.as_bytes())?;
        if let Some(reply) = reply_of(&output) {
            return Ok(reply);
        }
        let Some((artifact, digest)) = &self.upload else {
            return Err(no_reply(verb, &output));
        };
        if !may_upload || self.present(shell)? {
            return Err(no_reply(verb, &output));
        }
        self.upload(shell, artifact, digest)?;
        let output = shell.run(&command_line, body.as_bytes())?;
        reply_of(&output).ok_or_else(|| no_reply(verb, &output))
    }

    fn present(&self, shell: &dyn RemoteShell) -> Result<bool, String> {
        let output = shell.run(&exists_command(&self.program), b"")?;
        let text = String::from_utf8_lossy(&output.stdout);
        if text.contains("ralphus-runner-present") {
            Ok(true)
        } else if text.contains("ralphus-runner-missing") {
            Ok(false)
        } else {
            Err(format!(
                "could not check for the runner on the machine (is its OpenSSH default shell \
                 cmd.exe?): {}",
                output.stderr.trim()
            ))
        }
    }

    fn upload(
        &self,
        shell: &dyn RemoteShell,
        artifact: &std::path::Path,
        digest: &str,
    ) -> Result<(), String> {
        crate::emit_event(
            "uploading runner to windows machine",
            "info",
            json!({"path": self.program, "artifact": artifact.to_string_lossy()}),
        );
        let (directory, _) = self
            .program
            .rsplit_once('\\')
            .ok_or_else(|| format!("runner path {} has no directory", self.program))?;
        let output = shell.run(&mkdir_command(directory), b"")?;
        if output.code != Some(0) {
            return Err(format!(
                "could not create {directory} on the machine (exit {}): {}",
                output.code.unwrap_or(-1),
                output.stderr.trim()
            ));
        }
        let temporary = format!("{}.upload-{}", self.program, upload_nonce());
        shell.copy_in(artifact, &temporary.replace('\\', "/"))?;
        let output = shell.run(&upload_command(&self.program, &temporary, digest), b"")?;
        let said = String::from_utf8_lossy(&output.stdout);
        if output.code == Some(0)
            && said
                .lines()
                .any(|l| l.trim().starts_with("ralphus-runner "))
        {
            return Ok(());
        }
        Err(format!(
            "uploading the runner to {} failed (exit {}): {} {}",
            self.program,
            output.code.unwrap_or(-1),
            said.trim(),
            output.stderr.trim()
        ))
    }
}

/// `<runner> machine <verb> --remote-root <root>`.
#[must_use]
pub fn machine_command(program: &str, verb: &str, root: &str) -> String {
    format!("{program} machine {verb} --remote-root {root}")
}

/// A cmd.exe check that prints whether `program` exists.
#[must_use]
pub fn exists_command(program: &str) -> String {
    format!("if exist {program} (echo ralphus-runner-present) else (echo ralphus-runner-missing)")
}

/// A cmd.exe command creating `directory` (a [`safe_token`]) if missing.
#[must_use]
pub fn mkdir_command(directory: &str) -> String {
    format!("if not exist {directory} mkdir {directory}")
}

/// A random suffix for an upload's temporary name, so concurrent uploads of
/// the same runner never write the same file.
fn upload_nonce() -> String {
    let mut bytes = [0_u8; 8];
    if getrandom::getrandom(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        bytes = (nanos as u64 ^ u64::from(std::process::id())).to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The PowerShell script moving an uploaded runner from `temporary` to
/// `destination` once it hashes to `digest`.
#[must_use]
pub fn upload_script(destination: &str, temporary: &str, digest: &str) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $ProgressPreference = 'SilentlyContinue'\n\
         function Get-Sha256($path) {{\n\
         \x20 $stream = [IO.File]::OpenRead($path)\n\
         \x20 try {{ $hash = [Security.Cryptography.SHA256]::Create().ComputeHash($stream) }} finally {{ $stream.Close() }}\n\
         \x20 ([BitConverter]::ToString($hash) -replace '-', '').ToLower()\n\
         }}\n\
         $dest = {dest}\n\
         $tmp = {tmp}\n\
         $actual = Get-Sha256 $tmp\n\
         if ($actual -ne {digest}) {{ Remove-Item -Force -LiteralPath $tmp; [Console]::Error.WriteLine('runner checksum mismatch after upload'); exit 1 }}\n\
         if (Test-Path -LiteralPath $dest) {{\n\
         \x20 Remove-Item -Force -LiteralPath $tmp\n\
         \x20 $existing = Get-Sha256 $dest\n\
         \x20 if ($existing -ne {digest}) {{ [Console]::Error.WriteLine('existing content-addressed runner checksum mismatch'); exit 1 }}\n\
         }} else {{ Move-Item -LiteralPath $tmp -Destination $dest }}\n\
         & $dest --version\n\
         exit $LASTEXITCODE\n",
        dest = quote(destination),
        tmp = quote(temporary),
        digest = quote(digest),
    )
}

/// `powershell ... -EncodedCommand <script as base64 UTF-16LE>`: a command
/// line of bare tokens, read the same by `cmd.exe` and PowerShell.
#[must_use]
pub fn upload_command(destination: &str, temporary: &str, digest: &str) -> String {
    let script = upload_script(destination, temporary, digest);
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!(
        "powershell -NoProfile -NonInteractive -EncodedCommand {}",
        base64::engine::general_purpose::STANDARD.encode(utf16)
    )
}

/// The runner's reply: the last stdout line that parses as a JSON object.
/// Its stderr event lines are forwarded on the way.
fn reply_of(output: &Output) -> Option<Value> {
    for line in output.stderr.lines() {
        if line.trim_start().starts_with("RALPHUS_EVENT:") {
            eprintln!("{}", line.trim_end());
        }
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .filter(Value::is_object)
}

fn no_reply(verb: &str, output: &Output) -> String {
    let stderr: Vec<&str> = output
        .stderr
        .lines()
        .filter(|l| !l.trim_start().starts_with("RALPHUS_EVENT:"))
        .collect();
    format!(
        "the runner on the Windows machine gave no reply to {verb} (exit {}): {}",
        output.code.unwrap_or(-1),
        crate::redacted(stderr.join("\n").trim())
    )
}

/// Write a `materialize` reply's files under the request's `destination` on
/// this host (the SSH provider's contract), dropping them from the reply.
fn publish_materialized(request: &Value, reply: &mut Value) -> Result<(), String> {
    let destination = request
        .get("destination")
        .and_then(Value::as_str)
        .filter(|d| !d.trim().is_empty())
        .ok_or_else(|| "materialize request is missing \"destination\"".to_string())?;
    let destination = std::path::PathBuf::from(destination);
    let files = reply
        .as_object_mut()
        .and_then(|r| r.remove("files"))
        .and_then(|f| f.as_array().cloned())
        .unwrap_or_default();
    for file in files {
        let relative = file["path"].as_str().unwrap_or_default();
        if relative
            .split('/')
            .any(|p| p == ".." || p.contains('\\') || p.contains(':'))
        {
            return Err(format!("materialized file has an unsafe path {relative:?}"));
        }
        let target = if relative.is_empty() {
            destination.clone()
        } else {
            relative
                .split('/')
                .fold(destination.clone(), |p, c| p.join(c))
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(file["content_base64"].as_str().unwrap_or_default())
            .map_err(|e| format!("materialized file {relative:?} is not base64: {e}"))?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, bytes)
            .map_err(|e| format!("could not write {}: {e}", target.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn policy(mode: &str) -> RunnerPolicy {
        serde_json::from_value(json!({
            "mode": mode,
            "command": "C:\\ralphus\\bin\\ralphus-runner.exe",
            "artifacts": {},
            "remote_root": "C:/ralphus/fleet",
            "os": "windows",
            "arch": "x86_64",
        }))
        .unwrap()
    }

    /// Answers a command line, given how many came before it.
    type Answerer = Box<dyn Fn(&str, usize) -> Output>;

    /// A machine that records each command line and answers from a script.
    struct Scripted {
        seen: RefCell<Vec<(String, Vec<u8>)>>,
        answer: Answerer,
    }

    impl RemoteShell for Scripted {
        fn run(&self, command_line: &str, stdin: &[u8]) -> Result<Output, String> {
            let n = self.seen.borrow().len();
            self.seen
                .borrow_mut()
                .push((command_line.to_string(), stdin.to_vec()));
            Ok((self.answer)(command_line, n))
        }

        fn copy_in(&self, local: &std::path::Path, remote_path: &str) -> Result<(), String> {
            let bytes = std::fs::read(local).map_err(|e| e.to_string())?;
            self.seen
                .borrow_mut()
                .push((format!("scp {remote_path}"), bytes));
            Ok(())
        }
    }

    fn out(stdout: &str, stderr: &str, code: i32) -> Output {
        Output {
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.to_string(),
            code: Some(code),
        }
    }

    #[test]
    fn a_verb_runs_the_runner_with_the_request_on_stdin_never_the_command_line() {
        let shell = Scripted {
            seen: RefCell::new(Vec::new()),
            answer: Box::new(|_, _| out("{\"ok\":true,\"workspace\":\"C:/w\"}\n", "", 0)),
        };
        let request = json!({"project": "p", "env": {"TOKEN": "s3cret"}});
        let reply = serve_with(&shell, "provision", &request, &policy("installed"));
        assert_eq!(reply["workspace"], "C:/w");
        let seen = shell.seen.borrow();
        assert_eq!(
            seen[0].0,
            "C:\\ralphus\\bin\\ralphus-runner.exe machine provision --remote-root C:\\ralphus\\fleet"
        );
        assert!(!seen[0].0.contains("s3cret"));
        assert!(String::from_utf8_lossy(&seen[0].1).contains("s3cret"));
    }

    #[test]
    fn a_missing_runner_is_uploaded_once_then_the_verb_is_retried() {
        let dir = ralphus_test_support::TestDir::new("win-artifact");
        let artifact = dir.join("ralphus-runner.exe");
        std::fs::write(&artifact, b"runner-bytes").unwrap();
        let mut policy = policy("upload");
        policy.artifacts.insert(
            "x86_64-pc-windows-msvc".to_string(),
            artifact.to_string_lossy().into_owned(),
        );
        let shell = Scripted {
            seen: RefCell::new(Vec::new()),
            answer: Box::new(|line, n| {
                if line.starts_with("if exist") {
                    out("ralphus-runner-missing\r\n", "", 0)
                } else if line.starts_with("powershell") {
                    out("ralphus-runner 0.1.0\r\n", "", 0)
                } else if n == 0 {
                    out(
                        "",
                        "'C:\\x' is not recognized as an internal or external command",
                        1,
                    )
                } else {
                    out("{\"ok\":true,\"stdout\":\"hi\",\"exit_code\":0}", "", 0)
                }
            }),
        };
        let reply = serve_with(&shell, "run", &json!({"cwd": "C:/ralphus/fleet"}), &policy);
        assert_eq!(reply["stdout"], "hi", "{reply}");
        let seen = shell.seen.borrow();
        let lines: Vec<&str> = seen.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert!(lines[0].contains("\\runners\\x86_64-pc-windows-msvc\\"));
        assert!(lines[0].ends_with("machine run --remote-root C:\\ralphus\\fleet"));
        assert!(lines[2].starts_with("if not exist C:\\ralphus\\fleet\\runners\\"));
        assert!(lines[3].starts_with("scp C:/ralphus/fleet/runners/x86_64-pc-windows-msvc/"));
        assert!(
            lines[3].contains("/ralphus-runner.exe.upload-"),
            "{lines:?}"
        );
        assert_eq!(seen[3].1, b"runner-bytes", "the runner is copied over scp");
        assert!(lines[4].starts_with("powershell -NoProfile -NonInteractive -EncodedCommand "));
        assert!(
            seen[4].1.is_empty(),
            "nothing travels on PowerShell's stdin"
        );
    }

    #[test]
    fn capabilities_never_upload_and_still_report_the_platform() {
        let shell = Scripted {
            seen: RefCell::new(Vec::new()),
            answer: Box::new(|line, _| {
                if line.starts_with("echo") {
                    out("AMD64\r\n", "", 0)
                } else {
                    out("", "not recognized", 1)
                }
            }),
        };
        let mut policy = policy("installed");
        policy.arch = None;
        let reply = serve_with(&shell, "capabilities", &json!({}), &policy);
        assert_eq!(reply["capabilities"]["os"], "windows");
        assert_eq!(reply["capabilities"]["arch"], "x86_64");
        assert!(
            shell
                .seen
                .borrow()
                .iter()
                .all(|(l, _)| !l.starts_with("powershell"))
        );
    }

    #[test]
    fn unsafe_roots_and_runner_paths_are_refused_before_any_command() {
        let shell = Scripted {
            seen: RefCell::new(Vec::new()),
            answer: Box::new(|_, _| out("", "", 0)),
        };
        for root in [
            "C:/Program Files/ralphus",
            "/srv/ralphus",
            "C:/a&calc",
            "C:/x/../y",
        ] {
            let mut p = policy("installed");
            p.remote_root = root.to_string();
            let reply = serve_with(&shell, "run", &json!({}), &p);
            assert_eq!(reply["ok"], false, "{root}");
        }
        let mut p = policy("installed");
        p.command = "C:\\Program Files\\r.exe".to_string();
        assert_eq!(serve_with(&shell, "run", &json!({}), &p)["ok"], false);
        assert!(shell.seen.borrow().is_empty());
    }

    #[test]
    fn the_upload_script_is_encoded_utf16_and_checks_the_digest() {
        let line = upload_command(
            "C:\\r\\runners\\t\\abc\\ralphus-runner.exe",
            "C:\\r\\runners\\t\\abc\\ralphus-runner.exe.upload-1",
            "abc",
        );
        let encoded = line.rsplit(' ').next().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let script = String::from_utf16(&units).unwrap();
        assert!(script.contains("SHA256]::Create()"), "{script}");
        assert!(script.contains("$actual -ne 'abc'"), "{script}");
        assert!(
            script.contains("$tmp = 'C:\\r\\runners\\t\\abc\\ralphus-runner.exe.upload-1'"),
            "{script}"
        );
        assert!(!script.contains("OpenStandardInput"), "{script}");
        assert!(safe_token("C:\\r\\x.exe"));
        assert!(!safe_token("C:\\Program Files\\x.exe"));
    }

    /// The real flow on this machine, with `cmd.exe /c` standing in for
    /// Windows OpenSSH's session shell and the built runner as the upload
    /// artifact.
    #[cfg(windows)]
    #[test]
    fn the_whole_flow_runs_through_cmd_exe_with_the_real_runner() {
        struct LocalCmd;
        impl RemoteShell for LocalCmd {
            fn run(&self, command_line: &str, stdin: &[u8]) -> Result<Output, String> {
                use std::os::windows::process::CommandExt as _;
                let mut child = Command::new("cmd.exe")
                    .raw_arg("/c")
                    .raw_arg(command_line)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| e.to_string())?;
                let mut pipe = child.stdin.take().unwrap();
                let bytes = stdin.to_vec();
                let writer = std::thread::spawn(move || {
                    let _ = pipe.write_all(&bytes);
                });
                let output = child.wait_with_output().map_err(|e| e.to_string())?;
                let _ = writer.join();
                Ok(Output {
                    stdout: output.stdout,
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    code: output.status.code(),
                })
            }

            fn copy_in(&self, local: &std::path::Path, remote_path: &str) -> Result<(), String> {
                std::fs::copy(local, remote_path)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
        }
        let Some(runner) = std::env::current_exe().ok().and_then(|exe| {
            let candidate = exe.parent()?.parent()?.join("ralphus-runner.exe");
            candidate.is_file().then_some(candidate)
        }) else {
            eprintln!("skipped: build ralphus-runner first");
            return;
        };
        let dir = ralphus_test_support::TestDir::new("win-ssh-flow");
        let root = dir.to_string_lossy().replace('/', "\\");
        if !safe_token(&root) {
            eprintln!("skipped: scratch path {root} has characters this flow refuses");
            return;
        }
        let policy: RunnerPolicy = serde_json::from_value(json!({
            "mode": "upload",
            "command": "ralphus-runner",
            "artifacts": {"x86_64-pc-windows-msvc": runner.to_string_lossy()},
            "remote_root": root,
            "os": "windows",
        }))
        .unwrap();
        let reply = serve_with(
            &LocalCmd,
            "write-file",
            &json!({"path": format!("{root}\\w\\a.txt"), "content": "hello"}),
            &policy,
        );
        assert_eq!(reply["ok"], true, "{reply}");
        let reply = serve_with(
            &LocalCmd,
            "read-file",
            &json!({"path": format!("{root}/w/a.txt")}),
            &policy,
        );
        assert_eq!(reply["stdout"], "hello", "{reply}");
        let reply = serve_with(&LocalCmd, "capabilities", &json!({}), &policy);
        assert_eq!(reply["capabilities"]["os"], "windows", "{reply}");
        assert!(
            reply["capabilities"]["runner_version"].is_string(),
            "{reply}"
        );
    }
}
