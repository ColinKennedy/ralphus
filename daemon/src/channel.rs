//! Persistent provider channels (RAL-185 D7, option (c)).
//!
//! A machine provider is normally spawned once per command. That is fine on a
//! LAN — a review merge issues ~23 git commands (measured; see
//! `REMOTE.local.md`), and 23 cheap spawns cost nothing anyone notices. Across
//! a slow link it is 23 full connection handshakes, and on a Windows daemon
//! host SSH's own `ControlMaster` multiplexing is unavailable to amortise them.
//!
//! A **channel** is one long-lived provider process the daemon streams requests
//! into: one spawn, one handshake, many commands.
//!
//! ## "Channel", not "session"
//!
//! A *session* is already a first-class ralphus concept — a task contains
//! sessions, which contain verify steps — and it appears throughout the schema,
//! the store and the board. This is transport reuse and gets its own word. See
//! `docs/glossary.md`.
//!
//! ## Opt-in, and safe to get wrong
//!
//! A provider advertises channel support at registration. A provider that does
//! not is spawned per command exactly as before, so nothing about the existing
//! contract changes.
//!
//! When a channel *fails* — the program does not support it, the process dies,
//! it stops answering — the caller falls back to a one-shot spawn and logs it.
//! That is deliberate: the fallback is semantically identical, just slower, so
//! degrading quietly-but-loggedly is better than failing work over a transport
//! optimisation. A *command* that fails still fails; only the transport is
//! forgiving.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The verb a channel-capable provider is invoked with. It then reads
/// newline-delimited JSON requests on stdin and writes one newline-delimited
/// JSON response per request to stdout, until stdin closes.
pub const VERB_CHANNEL: &str = "channel";

/// How long to wait for a channel to answer one request before giving up on it.
///
/// Generous: a `git rebase` over a slow link can legitimately take a while, and
/// killing a working channel would be worse than waiting. This bounds the
/// pathological case — a provider that has stopped answering entirely — so the
/// daemon degrades to one-shot spawns instead of hanging a merge forever.
const REPLY_TIMEOUT: Duration = Duration::from_secs(300);

/// One live provider process, plus the plumbing to talk to it.
///
/// Responses are read on a dedicated thread rather than inline: `ChildStdout`
/// has no read timeout, so a provider that goes silent would otherwise block
/// the calling thread — and therefore a merge — with no way out. The thread
/// pushes lines into a channel the caller can wait on with a deadline.
struct Channel {
    child: Child,
    stdin: ChildStdin,
    replies: Receiver<String>,
}

impl Channel {
    /// Spawn `program` in channel mode.
    fn open(program: &str, args: &[String], uri: &str) -> Result<Self, String> {
        let mut child = Command::new(program)
            .args(args)
            .arg(VERB_CHANNEL)
            .arg("--uri")
            .arg(uri)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not open channel to {program}: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "channel child has no stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "channel child has no stdout".to_string())?;
        let (tx, replies) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    // The channel was dropped; nothing is listening any more.
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            replies,
        })
    }

    /// Send one request and wait for its reply.
    ///
    /// Requests and replies are strictly one-to-one and in order — the daemon
    /// issues a merge's commands sequentially, so there is no pipelining to
    /// reconcile and no need for request ids.
    fn request(&mut self, payload: &str) -> Result<String, String> {
        writeln!(self.stdin, "{payload}").map_err(|e| format!("channel write failed: {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("channel flush failed: {e}"))?;
        match self.replies.recv_timeout(REPLY_TIMEOUT) {
            Ok(line) => Ok(line),
            Err(RecvTimeoutError::Timeout) => Err(format!(
                "channel did not answer within {}s",
                REPLY_TIMEOUT.as_secs()
            )),
            Err(RecvTimeoutError::Disconnected) => {
                Err("channel closed unexpectedly (the provider exited)".to_string())
            }
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        // Closing stdin is the provider's cue to exit cleanly; kill only if it
        // ignores that. Leaving a stray process per machine would accumulate
        // across a long-running daemon.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Live channels, one per machine.
///
/// Keyed by `scheme:uri` so two reviews targeting the same machine share a
/// connection, and two machines behind one provider program do not.
type Pool = HashMap<String, Arc<Mutex<Channel>>>;

fn pool() -> &'static Mutex<Pool> {
    static POOL: OnceLock<Mutex<Pool>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Send `payload` to `scheme:uri`'s channel, opening one if needed.
///
/// # Errors
/// Any failure to open the channel or get a reply. **Callers should treat an
/// error as "fall back to a one-shot spawn", not as the command failing** — see
/// the module doc.
pub fn request(
    scheme: &str,
    uri: &str,
    program: &str,
    args: &[String],
    payload: &str,
) -> Result<String, String> {
    let key = format!("{scheme}:{uri}");
    let chan = {
        let mut guard = pool().lock().expect("channel pool poisoned");
        match guard.get(&key) {
            Some(c) => Arc::clone(c),
            None => {
                let opened = Arc::new(Mutex::new(Channel::open(program, args, uri)?));
                guard.insert(key.clone(), Arc::clone(&opened));
                opened
            }
        }
    };
    // The pool lock is released before the request: a slow command on one
    // machine must not block every other machine's channel lookup.
    let mut c = chan.lock().expect("channel poisoned");
    match c.request(payload) {
        Ok(reply) => Ok(reply),
        Err(e) => {
            // A broken channel is not reusable. Drop it so the next call opens
            // a fresh one rather than compounding the failure.
            drop(c);
            close(scheme, uri);
            Err(e)
        }
    }
}

/// Close and forget `scheme:uri`'s channel, if one is open.
///
/// Called when a channel misbehaves, and available for a caller that knows it
/// is done with a machine.
pub fn close(scheme: &str, uri: &str) {
    let key = format!("{scheme}:{uri}");
    let _ = pool().lock().expect("channel pool poisoned").remove(&key);
}

/// Whether a channel is currently open for `scheme:uri`.
///
/// Per-machine rather than a global count on purpose: the pool is process-wide
/// by design, so a total is meaningless to any caller that does not control
/// every machine — including a test running alongside others.
#[must_use]
pub fn is_open(scheme: &str, uri: &str) -> bool {
    pool()
        .lock()
        .expect("channel pool poisoned")
        .contains_key(&format!("{scheme}:{uri}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider script that echoes each request back with a marker, proving
    /// the same process served every request.
    fn echo_channel_script(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ral185-chan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Python is already a hard test dependency elsewhere in this crate.
        let py = dir.join("chan.py");
        std::fs::write(
            &py,
            "import sys, os\n\
             pid = os.getpid()\n\
             for line in sys.stdin:\n\
             \x20   line = line.strip()\n\
             \x20   if not line:\n\
             \x20       continue\n\
             \x20   print('{\"ok\":true,\"protocol_version\":1,\"exit_code\":0,\"stdout\":\"pid=%d\"}' % pid, flush=True)\n",
        )
        .expect("write");
        py
    }

    fn python_available() -> bool {
        Command::new("python")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn one_channel_serves_many_requests_from_a_single_process() {
        // The whole point: N commands, one spawn. If each request re-spawned,
        // the reported pids would differ.
        if !python_available() {
            println!("SKIP: python not on PATH");
            return;
        }
        let script = echo_channel_script("reuse");
        let args = vec![script.to_string_lossy().into_owned()];
        let mut seen = Vec::new();
        for _ in 0..5 {
            let reply = request("t-reuse", "A", "python", &args, "{\"args\":[\"status\"]}")
                .expect("channel request");
            seen.push(reply);
        }
        assert_eq!(seen.len(), 5);
        assert!(
            seen.windows(2).all(|w| w[0] == w[1]),
            "every reply must come from the same process: {seen:?}"
        );
        assert!(
            is_open("t-reuse", "A"),
            "the channel should be pooled for reuse"
        );
        close("t-reuse", "A");
        let _ = std::fs::remove_dir_all(script.parent().unwrap());
    }

    #[test]
    fn a_provider_that_cannot_open_a_channel_errors_for_the_caller_to_fall_back_on() {
        let err = request(
            "t-missing",
            "A",
            "/definitely/not/a/real/provider",
            &[],
            "{}",
        )
        .expect_err("must not succeed");
        assert!(err.contains("could not open channel"), "{err}");
        assert!(
            !is_open("t-missing", "A"),
            "a failed open must leave nothing pooled"
        );
    }

    #[test]
    fn a_channel_that_dies_is_dropped_rather_than_reused() {
        // A provider that exits immediately: the first request fails, and the
        // dead channel must not linger to fail the next one too.
        if !python_available() {
            println!("SKIP: python not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("ral185-chan-die-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let py = dir.join("die.py");
        std::fs::write(&py, "import sys; sys.exit(0)\n").unwrap();
        let args = vec![py.to_string_lossy().into_owned()];
        let err = request("t-die", "A", "python", &args, "{}").expect_err("must fail");
        assert!(err.contains("closed unexpectedly"), "{err}");
        assert!(
            !is_open("t-die", "A"),
            "a broken channel must be evicted, not left to fail the next call"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channels_for_different_machines_are_not_shared() {
        // Two uris behind one provider program are two machines; sharing a
        // channel between them would run work on the wrong one.
        if !python_available() {
            println!("SKIP: python not on PATH");
            return;
        }
        let script = echo_channel_script("distinct");
        let args = vec![script.to_string_lossy().into_owned()];
        let a = request("t-distinct", "A", "python", &args, "{}").expect("A");
        let b = request("t-distinct", "B", "python", &args, "{}").expect("B");
        assert_ne!(a, b, "each machine must get its own process");
        assert!(is_open("t-distinct", "A") && is_open("t-distinct", "B"));
        close("t-distinct", "A");
        close("t-distinct", "B");
        let _ = std::fs::remove_dir_all(script.parent().unwrap());
    }
}
