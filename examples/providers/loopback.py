#!/usr/bin/env python3
"""A reference machine provider (RAL-185) that runs work on *this* machine.

This is the worked example for `docs/machine-providers.md`. It implements every
verb in the contract, but its "remote machine" is the local host -- so you can
exercise the whole remote path (provision -> exec -> stream/status -> cancel)
end to end without a second machine, an SSH key, or a build farm.

Register it, then point a task at it:

    ralphus machine register --scheme loopback \\
        --program "python C:/path/to/examples/providers/loopback.py"

    # task.toml
    # [[task]]
    # name    = "demo"
    # project = "ralphus"
    # machine = "loopback:sandbox"
    #   [[task.session]]
    #   cwd    = "ralphus:new-worktree/demo-branch"
    #   prompt = "..."

`loopback:sandbox` -- the uri half -- is opaque to ralphus and interpreted only
here: this provider treats it as a namespace for its workspaces, so
`loopback:a` and `loopback:b` get separate checkouts of the same project.

**A real provider replaces the bodies, not the shapes.** Everything that makes
this file "local" is confined to `_run_locally` and `_provision_workspace`;
`main`'s dispatch, the JSON envelope, and the handle lifecycle are what a real
SSH/IncrediBuild provider would keep.

Protocol: argv is `<verb> --uri <uri> [--handle H] [--since N]`, the payload (if
any) arrives on stdin, and exactly one JSON object goes to stdout. Anything this
script wants to say to a human goes to *stderr* -- stdout is reserved for the
envelope, mirroring the daemon<->runner contract itself.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path
from typing import Any

# Must match `crate::machines::PROTOCOL_VERSION`. The daemon refuses a provider
# that declares a version it does not implement rather than guessing.
PROTOCOL_VERSION = 1

# Where this provider keeps its own state: one directory per in-flight handle
# (holding the child's pid, its captured output, and its final result), plus one
# workspace tree per `uri`. A real provider's equivalent lives on the remote
# machine; the *shape* is the same.
STATE_ROOT = Path(tempfile.gettempdir()) / "ralphus-loopback-provider"


def _reply(**fields: Any) -> None:
    """Emit the single JSON envelope this invocation is allowed to print."""
    payload: dict[str, Any] = {"protocol_version": PROTOCOL_VERSION}
    payload.update(fields)
    json.dump(payload, sys.stdout)
    sys.stdout.write("\n")
    sys.stdout.flush()


def _fail(message: str) -> None:
    """Report that the *invocation* failed.

    Distinct from a session that ran and failed -- that is `ok: true` with a
    `result.status` of `"failed"`. Collapsing the two would make a broken
    provider indistinguishable from legitimately failing work.
    """
    _reply(ok=False, error=message)


def _handle_dir(handle: str) -> Path:
    return STATE_ROOT / "handles" / handle


def _provision_workspace(uri: str, request: dict[str, Any]) -> str:
    """Ensure a workspace exists for `uri`, returning its absolute path.

    Idempotent by contract: a second call for the same `uri`/branch must reuse
    what is already there rather than recreating it, so a daemon restart does
    not discard an agent's uncommitted work.

    Note what this does *not* assume. `source.kind` names the VCS; only the
    `"git"` branch touches git at all. A provider backing a Perforce depot or a
    plain directory reads `kind` and does whatever that system needs -- the
    daemon never runs a VCS command on a provider's behalf.
    """
    source = request.get("source") or {}
    kind = source.get("kind") or ""
    branch = source.get("branch")
    url = source.get("url")

    # `uri` namespaces workspaces so two machines can hold the same branch.
    safe_uri = "".join(c if c.isalnum() or c in "-_" else "_" for c in uri)
    safe_branch = "".join(c if c.isalnum() or c in "-_" else "_" for c in (branch or "work"))
    workspace = STATE_ROOT / "workspaces" / safe_uri / safe_branch

    if workspace.exists():
        # Already provisioned -- reuse verbatim.
        return str(workspace)

    workspace.parent.mkdir(parents=True, exist_ok=True)

    if kind == "git" and url:
        subprocess.run(
            ["git", "clone", "--quiet", url, str(workspace)],
            check=True,
            capture_output=True,
        )
        if branch:
            # Check out the branch, creating it if the clone did not carry it.
            created = subprocess.run(
                ["git", "checkout", branch],
                cwd=str(workspace),
                capture_output=True,
            )
            if created.returncode != 0:
                subprocess.run(
                    ["git", "checkout", "-b", branch],
                    cwd=str(workspace),
                    check=True,
                    capture_output=True,
                )
    else:
        # Non-git (or git without a resolvable url): an empty directory is all
        # this example can honestly provide. A real provider would sync from
        # whatever source system `kind` names.
        workspace.mkdir(parents=True, exist_ok=True)
        print(
            f"ralphus [loopback] no git url for kind={kind!r}; "
            f"provisioned an empty workspace at {workspace}",
            file=sys.stderr,
        )

    return str(workspace)


def _run_locally(spec: dict[str, Any], handle: str) -> None:
    """Start `ralphus-runner` for `spec`, detached, recording state under `handle`.

    This is the only genuinely "local" part. A real provider would instead open
    an SSH session, submit to a build queue, or POST to an agent -- and would
    write the same three files (`pid`, `output`, `result`) from whatever it
    learns back.
    """
    d = _handle_dir(handle)
    d.mkdir(parents=True, exist_ok=True)

    runner_cmd = os.environ.get("RALPHUS_RUNNER_CMD", "ralphus-runner")
    argv = runner_cmd.split() if " " in runner_cmd else [runner_cmd]

    out_path = d / "output"
    # The runner speaks JSON on stdout and diagnostics (including the
    # `RALPHUS_EVENT:` markers the daemon forwards to Cartographer) on stderr.
    # Both are captured; `stream` serves the merged text, and `_finish` parses
    # the JSON out of stdout.
    with open(out_path, "wb") as out:
        child = subprocess.Popen(  # argv is operator-configured, not user input
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=out,
            cwd=spec.get("cwd") or None,
        )
    (d / "pid").write_text(str(child.pid), encoding="utf-8")

    # Hand the spec over and wait in this process. `exec` returned a handle to
    # the daemon before this point only if the caller forked; here we simply
    # block, then record the result -- see the note in `cmd_exec`.
    stdout, _ = child.communicate(json.dumps(spec).encode("utf-8"))
    (d / "result").write_bytes(stdout or b"")
    (d / "state").write_text("done" if child.returncode == 0 else "failed", encoding="utf-8")


def cmd_provision(uri: str, request: dict[str, Any]) -> None:
    try:
        workspace = _provision_workspace(uri, request)
    except subprocess.CalledProcessError as exc:
        detail = (exc.stderr or b"").decode("utf-8", "replace").strip()
        _fail(f"could not provision workspace: {detail or exc}")
        return
    except OSError as exc:
        _fail(f"could not provision workspace: {exc}")
        return
    _reply(ok=True, workspace=workspace)


def cmd_exec(uri: str, spec: dict[str, Any]) -> None:
    """Start the session and return a handle immediately.

    A provider that can only run synchronously may instead reply with
    `{"ok": true, "result": {...}}` and skip `status`/`stream`/`cancel`
    entirely -- both shapes are first-class. The async shape is what buys Live
    View and mid-run cancellation, so this example demonstrates it.
    """
    handle = uuid.uuid4().hex[:12]
    d = _handle_dir(handle)
    d.mkdir(parents=True, exist_ok=True)
    (d / "state").write_text("running", encoding="utf-8")
    (d / "uri").write_text(uri, encoding="utf-8")

    # Re-invoke ourselves detached so this process can return the handle now.
    # A real provider would return as soon as the remote side acknowledged.
    child_env = dict(os.environ, RALPHUS_LOOPBACK_HANDLE=handle)
    spec_path = d / "spec.json"
    spec_path.write_text(json.dumps(spec), encoding="utf-8")
    subprocess.Popen(  # re-invoking this same script, detached
        [sys.executable, str(Path(__file__).resolve()), "_worker", "--handle", handle],
        env=child_env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    _reply(ok=True, handle=handle)


def cmd_worker(handle: str) -> None:
    """Internal: the detached half of `exec`. Never called by the daemon."""
    d = _handle_dir(handle)
    try:
        spec = json.loads((d / "spec.json").read_text(encoding="utf-8"))
        _run_locally(spec, handle)
    except Exception as exc:  # must record *any* failure, or status hangs on "running"
        (d / "state").write_text("failed", encoding="utf-8")
        (d / "error").write_text(str(exc), encoding="utf-8")


def cmd_status(handle: str) -> None:
    d = _handle_dir(handle)
    if not d.exists():
        _fail(f"unknown handle {handle!r}")
        return
    state_file = d / "state"
    state = state_file.read_text(encoding="utf-8").strip() if state_file.exists() else "running"
    if state == "running":
        _reply(ok=True, state="running")
        return

    # Finished: hand back the runner's own SessionResult verbatim when it
    # produced one, so token/cost accounting survives the hop.
    result: dict[str, Any]
    raw = (d / "result").read_bytes() if (d / "result").exists() else b""
    try:
        result = json.loads(raw.decode("utf-8", "replace"))
    except (ValueError, UnicodeDecodeError):
        error = (d / "error").read_text(encoding="utf-8") if (d / "error").exists() else ""
        result = {
            "status": "failed",
            "summary": "the runner produced no parseable result",
            "error": error or None,
        }
    _reply(ok=True, state=state, result=result)


def cmd_stream(handle: str, since: int) -> None:
    """Return output produced after byte offset `since`, plus the next cursor.

    Optional in the contract: a provider that omits `stream` loses Live View
    but still runs work correctly.

    Also **re-emits any `RALPHUS_EVENT:` lines found in the new output onto this
    invocation's own stderr**, which is the non-obvious half. The daemon scrapes
    that marker from the stderr of *every* verb it invokes -- but for an async
    provider the session's own events are produced long after `exec` has
    returned its handle, so they would otherwise never reach Cartographer, and
    the live cost cap (which reads token/cost out of `llm-invoke` events) would
    silently stop being enforced. Forwarding them here is what closes that gap.
    """
    d = _handle_dir(handle)
    out = d / "output"
    if not out.exists():
        _reply(ok=True, output="", next=since)
        return
    data = out.read_bytes()
    chunk = data[since:] if since < len(data) else b""
    text = chunk.decode("utf-8", "replace")
    for line in text.splitlines():
        if line.startswith("RALPHUS_EVENT: "):
            print(line, file=sys.stderr)
    sys.stderr.flush()
    _reply(ok=True, output=text, next=len(data))


def cmd_cancel(handle: str) -> None:
    d = _handle_dir(handle)
    pid_file = d / "pid"
    if pid_file.exists():
        try:
            pid = int(pid_file.read_text(encoding="utf-8").strip())
        except ValueError:
            pid = 0
        if pid:
            # Kill the whole tree -- the runner spawns an agent of its own.
            if sys.platform == "win32":
                subprocess.run(["taskkill", "/F", "/T", "/PID", str(pid)], capture_output=True)
            else:
                subprocess.run(["kill", "-TERM", f"-{pid}"], capture_output=True)
    (d / "state").write_text("failed", encoding="utf-8")
    _reply(ok=True)


def cmd_run(uri: str, request: dict[str, Any]) -> None:
    """Run one VCS command in a workspace and return its output.

    Called ~23 times during a one-branch review merge, so a real provider should
    care about per-call cost here more than anywhere else in the contract. The
    transport is entirely this script's choice -- a fresh SSH invocation, an
    `ssh -o ControlMaster` multiplexed one, or a message into a persistent
    channel it keeps open. Nothing in the contract dictates it, which is the
    point: a farm on a LAN and a machine across a slow link can make different
    choices without ralphus changing.

    ("Channel", not "session" -- a session is already a ralphus concept, since a
    task contains sessions which contain verify steps.)

    Note `args` arrives already split. A provider must never re-join it into a
    shell string -- a branch name with a space or a path with a quote would then
    be mis-parsed on the far side, a whole class of bug this shape prevents.
    """
    cwd = request.get("cwd") or "."
    program = request.get("program") or "git"
    args = request.get("args") or []
    if not isinstance(args, list) or not all(isinstance(a, str) for a in args):
        _fail("'args' must be an array of strings")
        return
    try:
        proc = subprocess.run(  # argv is structured, never a shell string
            [program, *args],
            cwd=cwd,
            capture_output=True,
            check=False,
        )
    except OSError as exc:
        _fail(f"could not run {program}: {exc}")
        return
    # stdout on success, stderr on failure -- what a caller needs to see either
    # way, without having to ask for both.
    text = (proc.stdout if proc.returncode == 0 else proc.stderr).decode("utf-8", "replace")
    _reply(ok=True, exit_code=proc.returncode, stdout=text)


def cmd_channel(uri: str) -> None:
    """Serve many requests from one process (RAL-185 D7).

    Reads newline-delimited JSON requests on stdin, writes one newline-delimited
    JSON response each, until stdin closes. One spawn, one connection handshake,
    many commands -- which is the entire point across a slow link.

    Only worth implementing if reaching your machines is expensive. A provider
    that omits this verb is spawned per command and works identically, just with
    more overhead; the daemon falls back automatically if a channel cannot be
    opened or stops answering, so getting it wrong degrades rather than breaks.

    Each request is the same shape `run` takes on stdin. Requests and responses
    are strictly one-to-one and in order -- the daemon issues a merge's commands
    sequentially, so there is no pipelining to reconcile.
    """
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except ValueError as exc:
            _fail(f"malformed channel request: {exc}")
            continue
        cmd_run(uri, request)


def cmd_ping(uri: str) -> None:
    """Confirm this machine is reachable and ready, doing no work.

    Cheap by contract -- the daemon calls it to surface reachability in the
    board, not as part of running anything. A real provider would check that the
    remote host answers and has whatever it needs (a toolchain, a licence, free
    capacity) and say so in `detail`.
    """
    _reply(ok=True, detail=f"loopback provider on {os.name}, uri={uri!r}")


def cmd_cleanup(uri: str) -> None:
    safe_uri = "".join(c if c.isalnum() or c in "-_" else "_" for c in uri)
    shutil.rmtree(STATE_ROOT / "workspaces" / safe_uri, ignore_errors=True)
    _reply(ok=True)


def cmd_read_file(request: dict[str, Any]) -> None:
    """Return a file's contents (RAL-355 Phase 2/4 remainder).

    A missing/unreadable file is *not* special-cased -- the daemon layer
    already treats any failure here as "absent" rather than distinguishing
    why, per `docs/machine-providers.md`.
    """
    path = request.get("path") or ""
    try:
        content = Path(path).read_text(encoding="utf-8")
    except OSError as exc:
        _fail(f"could not read {path!r}: {exc}")
        return
    _reply(ok=True, stdout=content)


def cmd_write_file(request: dict[str, Any]) -> None:
    """Write `content` to `path`, creating parent directories as needed."""
    path = request.get("path") or ""
    content = request.get("content")
    if content is None:
        _fail("write-file request is missing \"content\"")
        return
    try:
        p = Path(path)
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content, encoding="utf-8")
    except OSError as exc:
        _fail(f"could not write {path!r}: {exc}")
        return
    _reply(ok=True)


def cmd_remove_path(request: dict[str, Any]) -> None:
    """Delete a file, or a directory tree when `recursive`.

    A path that does not exist is success, not an error.
    """
    path = request.get("path") or ""
    recursive = bool(request.get("recursive"))
    p = Path(path)
    try:
        if recursive and p.is_dir():
            shutil.rmtree(p, ignore_errors=True)
        elif p.exists():
            p.unlink()
    except OSError as exc:
        _fail(f"could not remove {path!r}: {exc}")
        return
    _reply(ok=True)


def cmd_capabilities(uri: str) -> None:
    """Report what this provider supports (RAL-355 Phase 0 remainder).

    Optional and best-effort by contract -- a provider that omits this verb
    entirely is treated identically to "no capability information
    available", not an error.
    """
    _reply(
        ok=True,
        capabilities={
            "os": os.name,
            "arch": None,
            "supported_ops": [
                "provision",
                "exec",
                "status",
                "stream",
                "cancel",
                "channel",
                "run",
                "read-file",
                "write-file",
                "remove-path",
                "ping",
                "cleanup",
                "capabilities",
            ],
            "async_exec": True,
            "terminal": False,
            "runner_version": None,
        },
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("verb")
    parser.add_argument("--uri", default="")
    parser.add_argument("--handle", default="")
    parser.add_argument("--since", type=int, default=0)
    args, _unknown = parser.parse_known_args(argv)

    # Payload-carrying verbs read stdin; handle-scoped/no-payload verbs don't.
    payload: dict[str, Any] = {}
    if args.verb in {"provision", "exec", "run", "read-file", "write-file", "remove-path"}:
        raw = sys.stdin.read()
        if raw.strip():
            try:
                payload = json.loads(raw)
            except ValueError as exc:
                _fail(f"malformed {args.verb} payload: {exc}")
                return 0

    if args.verb == "provision":
        cmd_provision(args.uri, payload)
    elif args.verb == "exec":
        cmd_exec(args.uri, payload)
    elif args.verb == "status":
        cmd_status(args.handle)
    elif args.verb == "stream":
        cmd_stream(args.handle, args.since)
    elif args.verb == "cancel":
        cmd_cancel(args.handle)
    elif args.verb == "channel":
        cmd_channel(args.uri)
    elif args.verb == "run":
        cmd_run(args.uri, payload)
    elif args.verb == "ping":
        cmd_ping(args.uri)
    elif args.verb == "cleanup":
        cmd_cleanup(args.uri)
    elif args.verb == "read-file":
        cmd_read_file(payload)
    elif args.verb == "write-file":
        cmd_write_file(payload)
    elif args.verb == "remove-path":
        cmd_remove_path(payload)
    elif args.verb == "capabilities":
        cmd_capabilities(args.uri)
    elif args.verb == "_worker":
        cmd_worker(args.handle)
    else:
        _fail(f"unknown verb {args.verb!r}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
