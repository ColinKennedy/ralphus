#!/usr/bin/env python3
"""A reference machine provider (RAL-185) that runs work on *this* machine.

This is the worked example for `docs/machine-providers.md`. It implements every
verb in the contract, but its "remote machine" is the local host -- so you can
exercise the whole remote path (provision -> exec -> stream/status -> cancel)
end to end without a second machine, an SSH key, or a build farm.

Register it, then point a task at it:

    ralphus machine register --scheme loopback \\
        --program python --arg C:/path/to/examples/providers/loopback.py

    # task.toml
    # [[task]]
    # name    = "demo"
    # project = "ralphus"
    # machine = "loopback:sandbox"
    #   [[task.cell]]
    #   cwd    = "<<ralphus:new-worktree/demo-branch?upstream=<<default>>>>"
    #   prompt = "..."

A git project provisioned onto a machine needs a `[machine.targets.*]` entry
with a `remote_root`. Given one, this provider lays workspaces out exactly as
`ralphus-ssh-provider` does:

    <remote_root>/projects/<project>-<url-hash>/repository/
    <remote_root>/projects/<project>-<url-hash>/worktrees/<branch>-<hash>/

`loopback:sandbox` -- the uri half -- is opaque to ralphus and interpreted only
here: without a `remote_root`, this provider treats it as a namespace for its
workspaces, so `loopback:a` and `loopback:b` get separate checkouts.

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
import contextlib
import hashlib
import json
import os
import shutil
import signal
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
# workspace tree per `uri` when no `remote_root` is configured. A real
# provider's equivalent lives on the remote machine; the *shape* is the same.
STATE_ROOT = (
    Path(os.environ.get("RALPHUS_LOOPBACK_STATE_ROOT", tempfile.gettempdir()))
    / "ralphus-loopback-provider"
)


# Strict mode makes this provider behave like a machine with its own
# filesystem: every path it is asked to touch must live under its state root
# or a `remote_root` it has provisioned into. A daemon that hands a provider a
# path from its *own* disk (which loopback could otherwise reach, being the
# same host) fails loudly here instead of only on a real remote machine.
STRICT = os.environ.get("RALPHUS_LOOPBACK_STRICT", "") not in ("", "0")
ROOTS_FILE = STATE_ROOT / "remote-roots.txt"


def _remember_root(remote_root: str) -> None:
    STATE_ROOT.mkdir(parents=True, exist_ok=True)
    known = _known_roots()
    resolved = str(Path(remote_root).resolve())
    if resolved not in known:
        with open(ROOTS_FILE, "a", encoding="utf-8") as f:
            f.write(resolved + "\n")


def _known_roots() -> list[str]:
    try:
        return [line for line in ROOTS_FILE.read_text(encoding="utf-8").splitlines() if line]
    except OSError:
        return []


def _foreign_path(path: str) -> str | None:
    """In strict mode, why `path` is not on "this machine"; `None` when it is."""
    if not STRICT or not path:
        return None
    resolved = Path(path).resolve()
    for root in [str(STATE_ROOT.resolve()), *_known_roots()]:
        if resolved == Path(root) or Path(root) in resolved.parents:
            return None
    return (
        f"strict loopback: {path!r} is not on this machine (outside the provider state root "
        f"and every provisioned remote_root); a real remote host would not have it"
    )


def _reply(**fields: Any) -> None:
    """Emit the single JSON envelope this invocation is allowed to print."""
    payload: dict[str, Any] = {"protocol_version": PROTOCOL_VERSION}
    payload.update(fields)
    json.dump(payload, sys.stdout)
    sys.stdout.write("\n")
    sys.stdout.flush()


def _fail(message: str) -> None:
    """Report that the *invocation* failed.

    Distinct from a cell that ran and failed -- that is `ok: true` with a
    `result.status` of `"failed"`. Collapsing the two would make a broken
    provider indistinguishable from legitimately failing work.
    """
    _reply(ok=False, error=message)


def _handle_dir(handle: str) -> Path:
    return STATE_ROOT / "handles" / handle


def _slug(value: str) -> str:
    """The final path component of `value`, unsafe characters collapsed."""
    last = next(
        (part for part in reversed(value.replace("\\", "/").split("/")) if part), "workspace"
    )
    return "".join(c if c.isalnum() or c in "-_" else "_" for c in last)[:48]


def _digest(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()[:16]


def _project_dir(remote_root: str, project: str, clone_url: str) -> Path:
    return Path(remote_root) / "projects" / f"{_slug(project)}-{_digest(clone_url)}"


def _worktree_dir(project_dir: Path, branch: str) -> Path:
    return project_dir / "worktrees" / f"{_slug(branch)}-{_digest(branch)}"


def _git(cwd: Path, *args: str) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(["git", *args], cwd=str(cwd), capture_output=True, check=False)


def _git_ok(cwd: Path, *args: str) -> None:
    proc = _git(cwd, *args)
    if proc.returncode != 0:
        raise subprocess.CalledProcessError(
            proc.returncode, ["git", *args], proc.stdout, proc.stderr
        )


def _provision_durable(remote_root: str, request: dict[str, Any], url: str, branch: str) -> Path:
    """Provision under `remote_root`: one persistent clone, one worktree per branch."""
    project_dir = _project_dir(remote_root, request.get("project") or "project", url)
    repository = project_dir / "repository"
    if not (repository / ".git").exists():
        project_dir.mkdir(parents=True, exist_ok=True)
        _git_ok(project_dir, "clone", "--quiet", url, str(repository))
    else:
        _git_ok(repository, "fetch", "--quiet", "--prune", "origin")

    worktree = _worktree_dir(project_dir, branch)
    if worktree.exists():
        # Already provisioned -- reuse verbatim so uncommitted work survives.
        return worktree
    worktree.parent.mkdir(parents=True, exist_ok=True)
    if (
        _git(
            repository, "rev-parse", "--verify", "--quiet", f"refs/remotes/origin/{branch}"
        ).returncode
        == 0
    ):
        _git_ok(repository, "worktree", "add", "-B", branch, str(worktree), f"origin/{branch}")
        return worktree
    upstream = (request.get("source") or {}).get("upstream") or ""
    candidates = [f"origin/{upstream}", upstream] if upstream else []
    candidates.append("HEAD")
    for base in candidates:
        if base and _git(repository, "rev-parse", "--verify", "--quiet", base).returncode == 0:
            _git_ok(repository, "worktree", "add", "-B", branch, str(worktree), base)
            return worktree
    raise OSError(f"no base commit for branch {branch!r} (upstream {upstream!r})")


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
    remote_root = request.get("remote_root")

    if kind == "git" and url and branch and remote_root:
        _remember_root(remote_root)
        return str(_provision_durable(remote_root, request, url, branch))

    # No configured remote_root: `uri` namespaces workspaces so two machines
    # can hold the same branch.
    workspace = STATE_ROOT / "workspaces" / _slug(uri) / _slug(branch or "work")

    if workspace.exists():
        return str(workspace)

    workspace.parent.mkdir(parents=True, exist_ok=True)

    if kind == "git" and url:
        _git_ok(workspace.parent, "clone", "--quiet", url, str(workspace))
        if branch and _git(workspace, "checkout", branch).returncode != 0:
            _git_ok(workspace, "checkout", "-b", branch)
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


def _environment(overrides: Any) -> dict[str, str]:
    """This process's environment with the daemon-supplied overrides on top."""
    env = dict(os.environ)
    if isinstance(overrides, dict):
        env.update({str(k): str(v) for k, v in overrides.items()})
    return env


def _detached_kwargs() -> dict[str, Any]:
    """Start a child in its own process group so `cancel` can kill the tree."""
    if sys.platform == "win32":
        return {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP}
    return {"start_new_session": True}


def _run_locally(spec: dict[str, Any], handle: str) -> None:
    """Start `ralphus-runner` for `spec`, recording state under `handle`.

    This is the only genuinely "local" part. A real provider would instead open
    an SSH session, submit to a build queue, or POST to an agent -- and would
    write the same three files (`pid`, `output`, `result`) from whatever it
    learns back.

    `execution_environment` is the cell's resolved environment (cell, proof,
    and agent-profile variables, secrets included). It is applied to the
    runner's process environment and removed from the spec, exactly as a
    remote provider would export it on the far side.
    """
    d = _handle_dir(handle)
    d.mkdir(parents=True, exist_ok=True)

    env = _environment(spec.pop("execution_environment", None))
    runner_cmd = os.environ.get("RALPHUS_RUNNER_CMD", "ralphus-runner")
    argv = (
        runner_cmd.split() if " " in runner_cmd and not Path(runner_cmd).exists() else [runner_cmd]
    )

    out_path = d / "output"
    # The runner speaks JSON on stdout and diagnostics (including the
    # `RALPHUS_EVENT:` markers the daemon forwards to Cartographer) on stderr.
    # Both are captured; `stream` serves the merged text, and `cmd_status`
    # parses the JSON out of stdout.
    with open(out_path, "wb") as out:
        child = subprocess.Popen(  # argv is operator-configured, not user input
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=out,
            cwd=spec.get("cwd") or None,
            env=env,
            **_detached_kwargs(),
        )
    (d / "pid").write_text(str(child.pid), encoding="utf-8")

    stdout, _ = child.communicate(json.dumps(spec).encode("utf-8"))
    (d / "result").write_bytes(stdout or b"")
    if (d / "cancelled").exists():
        # `cancel` already recorded the terminal state; a killed runner's exit
        # must not overwrite it.
        return
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
    """Start the cell and return a handle immediately.

    A provider that can only run synchronously may instead reply with
    `{"ok": true, "result": {...}}` and skip `status`/`stream`/`cancel`
    entirely -- both shapes are first-class. The async shape is what buys Live
    View and mid-run cancellation, so this example demonstrates it.
    """
    foreign = _foreign_path(spec.get("cwd") or "")
    if foreign:
        _fail(foreign)
        return
    handle = uuid.uuid4().hex[:12]
    d = _handle_dir(handle)
    d.mkdir(parents=True, exist_ok=True)
    (d / "state").write_text("running", encoding="utf-8")
    (d / "uri").write_text(uri, encoding="utf-8")

    # Re-invoke ourselves detached so this process can return the handle now.
    # A real provider would return as soon as the remote side acknowledged.
    spec_path = d / "spec.json"
    spec_path.write_text(json.dumps(spec), encoding="utf-8")
    subprocess.Popen(  # re-invoking this same script, detached
        [sys.executable, str(Path(__file__).resolve()), "_worker", "--handle", handle],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        **_detached_kwargs(),
    )
    _reply(ok=True, handle=handle)


def cmd_worker(handle: str) -> None:
    """Internal: the detached half of `exec`. Never called by the daemon."""
    d = _handle_dir(handle)
    try:
        spec = json.loads((d / "spec.json").read_text(encoding="utf-8"))
        # The spec may carry secrets; it has been read, so drop it from disk.
        (d / "spec.json").unlink(missing_ok=True)
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

    # Finished: hand back the runner's own CellResult verbatim when it produced
    # one, so token/cost accounting survives the hop.
    result: dict[str, Any]
    raw = (d / "result").read_bytes() if (d / "result").exists() else b""
    try:
        result = json.loads(raw.decode("utf-8", "replace"))
    except (ValueError, UnicodeDecodeError):
        error = (d / "error").read_text(encoding="utf-8") if (d / "error").exists() else ""
        if (d / "cancelled").exists():
            error = error or "cancelled"
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
    provider the cell's own events are produced long after `exec` has returned
    its handle, so they would otherwise never reach Cartographer, and the live
    cost cap (which reads token/cost out of `llm-invoke` events) would silently
    stop being enforced. Forwarding them here is what closes that gap.
    """
    out = _handle_dir(handle) / "output"
    if not out.exists():
        _reply(ok=True, output="", next=since)
        return
    with open(out, "rb") as f:
        f.seek(since)
        chunk = f.read()
    text = chunk.decode("utf-8", "replace")
    for line in text.splitlines():
        if line.startswith("RALPHUS_EVENT: "):
            print(line, file=sys.stderr)
    sys.stderr.flush()
    _reply(ok=True, output=text, next=since + len(chunk))


def cmd_cancel(handle: str) -> None:
    d = _handle_dir(handle)
    (d / "cancelled").write_text("1", encoding="utf-8")
    pid_file = d / "pid"
    if pid_file.exists():
        try:
            pid = int(pid_file.read_text(encoding="utf-8").strip())
        except ValueError:
            pid = 0
        if pid:
            # Kill the whole tree -- the runner spawns an agent of its own, and
            # was started in its own process group for exactly this.
            if sys.platform == "win32":
                subprocess.run(["taskkill", "/F", "/T", "/PID", str(pid)], capture_output=True)
            else:
                with contextlib.suppress(OSError):
                    os.killpg(pid, signal.SIGTERM)
    (d / "state").write_text("failed", encoding="utf-8")
    _reply(ok=True)


def cmd_job_cleanup(handle: str) -> None:
    """Delete one finished handle's retained state."""
    shutil.rmtree(_handle_dir(handle), ignore_errors=True)
    _reply(ok=True)


def _run_reply(request: dict[str, Any]) -> dict[str, Any]:
    """Run one `run` request and build its reply envelope fields.

    An empty `program` means `args[0]` is a shell command line the author wrote
    (a review check gate, an `auto_build` step) and must go through a shell; a
    named program gets its already-split argv, never re-joined into a string.
    """
    cwd = request.get("cwd") or "."
    program = request.get("program") or ""
    args = request.get("args") or []
    if not isinstance(args, list) or not all(isinstance(a, str) for a in args):
        return {"ok": False, "error": "'args' must be an array of strings"}
    foreign = _foreign_path(cwd)
    if foreign:
        return {"ok": False, "error": foreign}
    env = _environment(request.get("env"))
    try:
        if program:
            proc = subprocess.run(  # argv is structured, never a shell string
                [program, *args], cwd=cwd, env=env, capture_output=True, check=False
            )
        else:
            proc = subprocess.run(  # the author wrote shell syntax; run it as such
                " ".join(args), cwd=cwd, env=env, shell=True, capture_output=True, check=False
            )
    except OSError as exc:
        return {"ok": False, "error": f"could not run {program or 'shell command'}: {exc}"}
    if os.environ.get("RALPHUS_LOOPBACK_TRACE"):
        # Opt-in diagnostic: one line per command, for "what is the daemon
        # actually asking this machine to do" questions.
        with open(STATE_ROOT / "run-trace.log", "a", encoding="utf-8") as trace:
            trace.write(
                json.dumps({"cwd": cwd, "program": program, "args": args, "exit": proc.returncode})
                + "\n"
            )
    # stdout on success, stdout+stderr on failure -- what a caller needs to see
    # either way, without having to ask for both.
    text = proc.stdout if proc.returncode == 0 else proc.stdout + proc.stderr
    return {"ok": True, "exit_code": proc.returncode, "stdout": text.decode("utf-8", "replace")}


def cmd_run(uri: str, request: dict[str, Any]) -> None:
    """Run one command in a workspace and return its output.

    Called ~23 times during a one-branch review merge, so a real provider should
    care about per-call cost here more than anywhere else in the contract. The
    transport is entirely this script's choice -- a fresh SSH invocation, an
    `ssh -o ControlMaster` multiplexed one, or a message into a persistent
    channel it keeps open. Nothing in the contract dictates it, which is the
    point: a farm on a LAN and a machine across a slow link can make different
    choices without ralphus changing.

    Note `args` arrives already split for a named `program`. A provider must
    never re-join it into a shell string -- a branch name with a space or a
    path with a quote would then be mis-parsed on the far side.
    """
    _reply(**_run_reply(request))


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
    are strictly one-to-one and in order.
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


def cmd_cleanup(uri: str, request: dict[str, Any]) -> None:
    """Tear down one project's workspace, or one branch's worktree within it."""
    remote_root = request.get("remote_root")
    project = request.get("project") or ""
    clone_url = request.get("clone_url") or ""
    branch = request.get("branch")
    if remote_root and project:
        project_dir = _project_dir(remote_root, project, clone_url)
        if branch:
            target = _worktree_dir(project_dir, branch)
            repository = project_dir / "repository"
            if repository.exists():
                _git(repository, "worktree", "remove", "--force", "--force", str(target))
                _git(repository, "worktree", "prune")
        else:
            target = project_dir
    elif branch:
        target = STATE_ROOT / "workspaces" / _slug(uri) / _slug(branch)
    else:
        target = STATE_ROOT / "workspaces" / _slug(uri)
    shutil.rmtree(target, ignore_errors=True)
    _reply(ok=True, removed=str(target))


def cmd_retire(request: dict[str, Any]) -> None:
    """Retire one stale review worktree: remove it through its own repository."""
    path = Path(request.get("path") or "")
    if not str(path) or not path.exists():
        _reply(ok=True, outcome="removed")
        return
    common = _git(path, "rev-parse", "--git-common-dir")
    if common.returncode != 0:
        detail = common.stderr.decode("utf-8", "replace").strip()
        _fail(f"{str(path)!r} is not a git worktree: {detail}")
        return
    repository = (path / common.stdout.decode("utf-8", "replace").strip()).resolve().parent
    removed = _git(repository, "worktree", "remove", "--force", "--force", str(path))
    if removed.returncode != 0:
        _fail(f"git worktree remove failed: {removed.stderr.decode('utf-8', 'replace').strip()}")
        return
    _reply(ok=True, outcome="removed")


def cmd_materialize(request: dict[str, Any]) -> None:
    """Copy one file or directory from "the machine" to the daemon host.

    Staged next to the destination and renamed into place, so a reader never
    sees a half-copied artifact.
    """
    source = Path(request.get("source") or "")
    destination = Path(request.get("destination") or "")
    if not source.exists():
        _fail(f"materialize source {str(source)!r} does not exist")
        return
    staging = destination.with_name(destination.name + f".partial-{uuid.uuid4().hex[:8]}")
    try:
        destination.parent.mkdir(parents=True, exist_ok=True)
        if source.is_dir():
            shutil.copytree(source, staging)
        else:
            shutil.copy2(source, staging)
            if request.get("executable"):
                staging.chmod(staging.stat().st_mode | 0o111)
        if destination.is_dir():
            shutil.rmtree(destination)
        os.replace(staging, destination)
    except OSError as exc:
        if staging.is_dir():
            shutil.rmtree(staging, ignore_errors=True)
        else:
            staging.unlink(missing_ok=True)
        _fail(f"could not materialize {str(source)!r}: {exc}")
        return
    _reply(ok=True)


def cmd_terminal(command: str) -> int:
    """Run `command` with this process's stdio as the terminal byte stream.

    Not JSON: the daemon relays raw bytes, and success is the exit code. A real
    provider allocates a pty on the far side (`ssh -tt`); a local shell with
    inherited stdio is the loopback equivalent.
    """
    if not command:
        print("ralphus [loopback] terminal: --command is required", file=sys.stderr)
        return 2
    return subprocess.call(command, shell=True)


def cmd_read_file(request: dict[str, Any]) -> None:
    """Return a file's contents (RAL-355 Phase 2/4 remainder).

    A missing/unreadable file is *not* special-cased -- the daemon layer
    already treats any failure here as "absent" rather than distinguishing
    why, per `docs/machine-providers.md`.
    """
    path = request.get("path") or ""
    try:
        # newline="" keeps the bytes exact: text mode would rewrite CRLF/LF.
        with open(path, encoding="utf-8", newline="") as f:
            content = f.read()
    except OSError as exc:
        _fail(f"could not read {path!r}: {exc}")
        return
    _reply(ok=True, stdout=content)


def cmd_write_file(request: dict[str, Any]) -> None:
    """Write `content` to `path`, creating parent directories as needed."""
    path = request.get("path") or ""
    content = request.get("content")
    if content is None:
        _fail('write-file request is missing "content"')
        return
    try:
        p = Path(path)
        p.parent.mkdir(parents=True, exist_ok=True)
        # newline="" writes the content byte-for-byte. Text mode on Windows
        # turns every "\n" into "\r\n", which breaks the `#!/bin/sh` git hooks
        # the daemon installs through this verb.
        with open(p, "w", encoding="utf-8", newline="") as f:
            f.write(content)
        if request.get("executable"):
            p.chmod(p.stat().st_mode | 0o111)
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


SUPPORTED_OPS = [
    "provision",
    "exec",
    "status",
    "stream",
    "cancel",
    "job-cleanup",
    "channel",
    "run",
    "read-file",
    "write-file",
    "remove-path",
    "materialize",
    "ping",
    "cleanup",
    "retire",
    "terminal",
    "capabilities",
]


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
            "supported_ops": SUPPORTED_OPS,
            "async_exec": True,
            "terminal": True,
            "runner_version": None,
        },
    )


PAYLOAD_VERBS = {
    "provision",
    "exec",
    "run",
    "read-file",
    "write-file",
    "remove-path",
    "cleanup",
    "retire",
    "materialize",
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("verb")
    parser.add_argument("--uri", default="")
    parser.add_argument("--handle", default="")
    parser.add_argument("--since", type=int, default=0)
    parser.add_argument("--command", default="")
    parser.add_argument("--cols", default="")
    parser.add_argument("--lines", default="")
    args, _unknown = parser.parse_known_args(argv)

    if args.verb == "terminal":
        return cmd_terminal(args.command)

    # Payload-carrying verbs read stdin; handle-scoped/no-payload verbs don't.
    payload: dict[str, Any] = {}
    if args.verb in PAYLOAD_VERBS:
        raw = sys.stdin.read()
        if raw.strip():
            try:
                payload = json.loads(raw)
            except ValueError as exc:
                _fail(f"malformed {args.verb} payload: {exc}")
                return 0

    # `materialize`'s destination is on the daemon host by contract, so only
    # its source is checked.
    path_field = "source" if args.verb == "materialize" else "path"
    if args.verb in {"read-file", "write-file", "remove-path", "retire", "materialize"}:
        foreign = _foreign_path(payload.get(path_field) or "")
        if foreign:
            _fail(foreign)
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
    elif args.verb == "job-cleanup":
        cmd_job_cleanup(args.handle)
    elif args.verb == "channel":
        cmd_channel(args.uri)
    elif args.verb == "run":
        cmd_run(args.uri, payload)
    elif args.verb == "ping":
        cmd_ping(args.uri)
    elif args.verb == "cleanup":
        cmd_cleanup(args.uri, payload)
    elif args.verb == "retire":
        cmd_retire(payload)
    elif args.verb == "materialize":
        cmd_materialize(payload)
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
        _fail(f"loopback provider does not implement verb {args.verb!r}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
