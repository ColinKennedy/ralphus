#!/usr/bin/env python3
"""End-to-end check of remote machines through a provider service (FLEET).

Starts an isolated ralphus daemon in machine strict mode and a ralphus-fleet
service in front of the machines a fleet config describes, then runs one
squad whose two tasks land on two different machines and feed a review that
merges on the first, and checks what happened:

- each task ran on the machine its labels asked for (allocations);
- the review reached in_review on its machine;
- the daemon never ran remote work on its own host (no strict-mode refusal);
- a registered secret reached the cells but is not on the machine's disk
  (checked by a cell on the machine) or in the daemon log (squad env
  overrides are stored in the daemon's database by design);
- the work survived a ralphus-fleet restart (tokens re-issued);
- a request no machine can serve fails with the provider's reasons;
- the machines are handed back once the review is cancelled;
- outpost machines (transport = "outpost") connected to the fleet;
- with --live-actions, the review's live actions run on its machine, carry
  their commits onto the top branch, and put their results and uploaded
  exhibits into the right PR (fleet_e2e_live.py).

Outpost machines must be started separately (`ralphus-outpost connect`)
against the fleet's URL, which --fleet-listen fixes in advance. An outpost on
this same host proves the Machine API path, not that work ran elsewhere.

No LLM is used: every cell and proof is a raw command, and the live-action
agent is a stand-in script. The machines are yours:
point --fleet-config at a ralphus-fleet.toml whose machines carry the labels
given by --label-a/--label-b. Its `listen`, `token_env` and `state_file` are
replaced by the harness. See docs/machine-provider-service.md and
docs/fleet-e2e.md.

Exit status: 0 when every check passed, 1 otherwise, 2 for bad arguments.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import secrets
import socket
import subprocess
import sys
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

import fleet_e2e_live as live

EXE = ".exe" if os.name == "nt" else ""
FLEET_TOKEN_ENV = "RALPHUS_E2E_FLEET_TOKEN"
SECRET_NAME = "RALPHUS_E2E_SECRET"
STRICT_REFUSAL = "machine strict mode: refused"
USER = "e2e"


def log(message: str) -> None:
    print(f"fleet-e2e: {message}", file=sys.stderr, flush=True)


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


class Checks:
    """Named pass/fail results, printed at the end."""

    def __init__(self) -> None:
        self.results: list[tuple[str, bool, str]] = []

    def record(self, name: str, ok: bool, detail: str = "") -> bool:
        self.results.append((name, ok, detail))
        log(f"{'PASS' if ok else 'FAIL'} {name}{': ' + detail if detail else ''}")
        return ok

    def report(self) -> int:
        print("\nfleet e2e results")
        for name, ok, detail in self.results:
            print(f"  {'PASS' if ok else 'FAIL'}  {name}{'  -- ' + detail if detail else ''}")
        failed = [r for r in self.results if not r[1]]
        print(f"{len(self.results) - len(failed)} passed, {len(failed)} failed")
        return 1 if failed or not self.results else 0


class Daemon:
    """An isolated ralphus daemon and its HTTP API."""

    def __init__(self, root: Path, bin_dir: Path, fleet_url: str, fleet_token: str):
        self.root = root
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.home = root / "home"
        self.config = root / "config"
        self.log_path = root / "daemon.log"
        self.db = root / "daemon.db"
        self.home.mkdir(parents=True, exist_ok=True)
        self.config.mkdir(parents=True, exist_ok=True)
        (self.home / ".gitconfig").write_text(
            "[user]\n\tname = Fleet E2E\n\temail = fleet-e2e@example.invalid\n"
        )
        daemon_table = f'[daemon]\ndefault_user = "{USER}"\ndefault_user_is_admin = true\n'
        (self.config / "config.toml").write_text(
            "[machine]\nstrict = true\n\n"
            f'[machine.providers.fleet]\nurl = "{fleet_url}"\ntoken_env = "{FLEET_TOKEN_ENV}"\n\n'
            + daemon_table
        )
        (root / ".ralphus.toml").write_text(daemon_table)
        env = dict(os.environ)
        env.pop("RALPHUS_CONFIGURATION_PATH", None)
        env.update(
            {
                "HOME": str(self.home),
                "USERPROFILE": str(self.home),
                "RALPHUS_CONFIG_HOME": str(self.config),
                "RALPHUS_RUNNER_CMD": str(bin_dir / f"ralphus-runner{EXE}"),
                "RALPHUS_MACHINE_STRICT": "1",
                FLEET_TOKEN_ENV: fleet_token,
            }
        )
        self.process = subprocess.Popen(
            [
                str(bin_dir / f"ralphus-daemon{EXE}"),
                "serve",
                "--port",
                str(self.port),
                "--db",
                str(self.db),
                "--log-path",
                str(self.log_path),
            ],
            cwd=root,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.token = self._wait_for_token()
        self._wait_ready()

    def _wait_for_token(self) -> str:
        path = self.home / ".ralphus" / "daemon.token"
        deadline = time.time() + 60
        while time.time() < deadline:
            if path.is_file() and path.read_text().strip():
                return path.read_text().strip()
            if self.process.poll() is not None:
                raise RuntimeError(f"daemon exited early; see {self.log_path}")
            time.sleep(0.2)
        raise RuntimeError(f"daemon wrote no token; see {self.log_path}")

    def _wait_ready(self) -> None:
        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                self.get("/api/daemon")
                return
            except (urllib.error.URLError, ConnectionError):
                time.sleep(0.3)
        raise RuntimeError(f"daemon did not answer; see {self.log_path}")

    def request(self, method: str, path: str, body: object | None = None) -> object:
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.url + path, data=data, method=method)
        request.add_header("Authorization", f"Bearer {self.token}")
        if data is not None:
            request.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(request, timeout=120) as reply:
                text = reply.read().decode()
        except urllib.error.HTTPError as e:
            raise RuntimeError(f"{method} {path}: HTTP {e.code}: {e.read().decode()[:500]}") from e
        return json.loads(text) if text.strip() else None

    def get(self, path: str) -> object:
        return self.request("GET", path)

    def post(self, path: str, body: object | None = None) -> object:
        return self.request("POST", path, body if body is not None else {})

    def stop(self) -> None:
        try:
            self.post("/api/daemon/shutdown")
        except Exception:  # noqa: BLE001 -- best effort, then kill
            pass
        try:
            self.process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.process.kill()


class Fleet:
    """A ralphus-fleet process serving a copy of the user's fleet config."""

    def __init__(self, root: Path, bin_dir: Path, source: Path, token: str, listen: str | None = None,
                 external_url: str | None = None, agents: tuple[str, dict[str, str]] | None = None):
        self.root = root
        self.bin_dir = bin_dir
        self.process: subprocess.Popen[bytes] | None = None
        if external_url:
            # A fleet the user runs (somewhere outposts can reach): read its
            # machines, never start or stop it.
            self.external = True
            self.url = external_url.rstrip("/")
            self.listen = self.url
            self.token = token
            self.log_path = source
            self.config = tomllib.loads(source.read_text())
            return
        self.external = False
        if listen:
            self.listen = listen
            self.port = int(listen.rsplit(":", 1)[1])
        else:
            self.port = free_port()
            self.listen = f"127.0.0.1:{self.port}"
        self.url = f"http://127.0.0.1:{self.port}"
        self.token = token
        self.config_path = root / "ralphus-fleet.toml"
        self.log_path = root / "fleet.log"
        text = source.read_text()
        # Replace the top-level keys the harness owns; everything else
        # (machines, ssh settings) is the user's.
        head, sep, rest = text.partition("\n[")
        owned = ("listen", "token_env", "state_file", "public_url")
        kept = [
            line
            for line in head.splitlines()
            if not any(re.match(rf"\s*{key}\s*=", line) for key in owned)
        ]
        prefix = (
            f'listen = "{self.listen}"\n'
            f'token_env = "{FLEET_TOKEN_ENV}"\n'
            f"state_file = {json.dumps(str(root / 'fleet.state.json'))}\n"
        )
        text = prefix + "\n".join(kept) + (sep + rest if sep else "")
        if agents:
            text = with_machine_agents(text, *agents)
        self.config_path.write_text(text)
        self.config = tomllib.loads(self.config_path.read_text())
        self.start()

    def start(self) -> None:
        if self.external:
            return
        env = dict(os.environ)
        env[FLEET_TOKEN_ENV] = self.token
        log_file = open(self.log_path, "ab")  # noqa: SIM115 -- owned by the process
        self.process = subprocess.Popen(
            [str(self.bin_dir / f"ralphus-fleet{EXE}"), "serve", "--config", str(self.config_path)],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=log_file,
            stderr=log_file,
        )
        deadline = time.time() + 30
        while time.time() < deadline:
            try:
                request = urllib.request.Request(self.url + "/v1/health")
                request.add_header("Authorization", f"Bearer {self.token}")
                urllib.request.urlopen(request, timeout=5).read()
                return
            except (urllib.error.URLError, ConnectionError):
                if self.process.poll() is not None:
                    raise RuntimeError(f"ralphus-fleet exited; see {self.log_path}") from None
                time.sleep(0.3)
        raise RuntimeError(f"ralphus-fleet did not answer; see {self.log_path}")

    def stop(self) -> None:
        if self.external:
            return
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.process.kill()

    def outpost_names(self) -> list[str]:
        return [str(m.get("name")) for m in self.config.get("machine", [])
                if m.get("transport") == "outpost"]

    def connected_outposts(self) -> list[str]:
        request = urllib.request.Request(self.url + "/v1/outposts")
        request.add_header("Authorization", f"Bearer {self.token}")
        with urllib.request.urlopen(request, timeout=10) as reply:
            listing = json.loads(reply.read().decode())
        return sorted(o["name"] for o in listing.get("outposts", []) if o.get("connected"))

    def machine_with_label(self, label: str) -> dict:
        for machine in self.config.get("machine", []):
            if label in machine.get("labels", []):
                return machine
        raise RuntimeError(f"no machine in {self.config_path} has label {label!r}")


def prompt_task(project: str, run: str, label_b: str, base: str, machine_b: dict, review: str,
                prompt_cell: str) -> str:
    """A task on machine B whose cell is a real prompt for `prompt_cell`
    (`AGENT:MODEL`), run with the machine's own agent installation and login."""
    agent, _, model = prompt_cell.partition(":")
    file = f"fleet-e2e-agent-{run}.txt"
    prompt = (
        f"Run the `hostname` command. Then create a file named {file} in the current directory "
        "whose only line is `written by an agent on HOST`, with HOST replaced by that output. "
        "Then stage it and commit it with `git -c user.email=fleet-e2e@example.invalid "
        f"-c user.name=Fleet-E2E commit -m {file}`. Do nothing else."
    )
    model_line = f'model = "{model}"\n' if model else ""
    return f"""
[[task]]
name = "agent-on-b"
project = "{project}"
machine = {{ provider = "fleet", labels = ["{label_b}"] }}
agent = "{agent}"
{model_line}
[[task.cell]]
cwd = "<<ralphus:new-worktree/fleet-e2e-agent-{run}?upstream={base}>>"
prompt = {json.dumps(prompt)}
review = "{review}"

[[task.cell.proof]]
command = "git log -1 --format=%s"
mode = "raw"
shell = "{live.shell_for(machine_b)}"
"""


def with_machine_agents(text: str, label: str, agents: dict[str, str]) -> str:
    """`text` (a ralphus-fleet.toml) with `agents` added to the `[machine.agents]`
    of the machine labelled `label`: the fleet then hands each agent its path
    on that machine (an agent profile's own executable is a daemon-local path,
    which ralphus refuses for a remote machine)."""
    parts = re.split(r"(?m)^(?=\[\[machine\]\]\s*$)", text)
    for i, part in enumerate(parts):
        if not part.startswith("[[machine]]"):
            continue
        machine = tomllib.loads(part.replace("[[machine]]", "[machine]", 1)).get("machine", {})
        if label not in machine.get("labels", []):
            continue
        if machine.get("agents"):
            raise RuntimeError(f"the machine labelled {label!r} already has [machine.agents]; "
                               "add the stand-in agent there yourself")
        lines = "".join(f"{name} = {json.dumps(path)}\n" for name, path in agents.items())
        parts[i] = part.rstrip("\n") + "\n[machine.agents]\n" + lines + "\n"
        merged = "".join(parts)
        tomllib.loads(merged)
        return merged
    raise RuntimeError(f"no machine in the fleet config has label {label!r}")


def squad_toml(project: str, run: str, label_a: str, label_b: str, base: str, machine_a: dict,
               machine_b: dict, live_actions: bool = False, prompt_cell: str | None = None) -> str:
    """Two tasks on two machines feeding one review on the first. With
    `live_actions`, task A also hands off a live action the review runs; with
    `prompt_cell`, a third task on machine B runs a real agent."""
    cwd_a = f"<<ralphus:new-worktree/fleet-e2e-a-{run}?upstream={base}>>"
    review = "<<ralphus:new-review/fleet-e2e>>"
    if live.is_windows(machine_a):
        work_a = (live.commit_command(machine_a, f"fleet-e2e-a-{run}.txt") + "; "
                  + live.secret_check_command(machine_a, SECRET_NAME))
    else:
        work_a = (live.commit_command(machine_a, f"fleet-e2e-a-{run}.txt") + " && "
                  + live.secret_check_command(machine_a, SECRET_NAME))
    shell_a, shell_b = live.shell_for(machine_a), live.shell_for(machine_b)
    return f"""
[[task]]
name = "on-a"
project = "{project}"
machine = {{ provider = "fleet", labels = ["{label_a}"] }}

[[task.cell]]
{'id = "work"' if live_actions else ""}
cwd = "{cwd_a}"
command = {json.dumps(work_a)}
mode = "raw"
shell = "{shell_a}"
review = "{review}"

[[task.cell.proof]]
command = "git log -1 --format=%s"
mode = "raw"
shell = "{shell_a}"
{live.handoff_cells(machine_a, run, cwd_a, review) if live_actions else ""}
[[task]]
name = "on-b"
project = "{project}"
machine = {{ provider = "fleet", labels = ["{label_b}"] }}

[[task.cell]]
cwd = "<<ralphus:new-worktree/fleet-e2e-b-{run}?upstream={base}>>"
command = {json.dumps(live.commit_command(machine_b, f"fleet-e2e-b-{run}.txt"))}
mode = "raw"
shell = "{shell_b}"
review = "{review}"

[[task.cell.proof]]
command = "git log -1 --format=%s"
mode = "raw"
shell = "{shell_b}"
{prompt_task(project, run, label_b, base, machine_b, review, prompt_cell) if prompt_cell else ""}
[[review]]
id = "ralphus:new-review/fleet-e2e"
upstream = "{base}"
machine = {{ provider = "fleet", labels = ["{label_a}"] }}
proof_scope = "nothing"
skip_manual_checks = true
{live.review_keys() if live_actions else ""}"""


def wait_for(what: str, probe, timeout: float, every: float = 2.0):
    """Poll `probe` until it returns a truthy value or `timeout` passes."""
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = probe()
        if last:
            return last
        time.sleep(every)
    raise TimeoutError(f"timed out after {timeout:.0f}s waiting for {what} (last: {last!r})")


def squad_state(daemon: Daemon, squad: str) -> str:
    return str(daemon.get(f"/api/squads/{squad}").get("state", ""))


def allocations(daemon: Daemon, all_states: bool = False) -> list[dict]:
    suffix = "?all=true" if all_states else ""
    return list(daemon.get(f"/api/machine-allocations{suffix}").get("allocations", []))


def run(args: argparse.Namespace) -> int:
    checks = Checks()
    bin_dir = Path(args.bin_dir).resolve()
    for name in ("ralphus-daemon", "ralphus-runner", "ralphus-fleet", "ralphus-ssh-provider"):
        if not (bin_dir / f"{name}{EXE}").is_file():
            log(f"missing {bin_dir / (name + EXE)}; build it first")
            return 2
    root = Path(args.state_root).resolve()
    if root.exists() and any(root.iterdir()):
        log(f"{root} is not empty; pass an empty or new --state-root")
        return 2
    root.mkdir(parents=True, exist_ok=True)
    run_id = secrets.token_hex(3)
    forge_token = ""
    secret_value = f"fleet-e2e-value-{secrets.token_hex(8)}"

    if args.fleet_url:
        token = os.environ.get(FLEET_TOKEN_ENV, "").strip()
        if not token:
            log(f"--fleet-url needs the fleet's service token in {FLEET_TOKEN_ENV}")
            return 2
        if args.restart_fleet:
            log("--fleet-url: the harness does not restart a fleet it did not start")
            args.restart_fleet = False
    else:
        token = secrets.token_hex(32)
    agents = None
    if args.live_actions and not args.fleet_url:
        source = tomllib.loads(Path(args.fleet_config).read_text())
        machine = next((m for m in source.get("machine", []) if args.label_a in m.get("labels", [])), None)
        if machine:
            agents = (args.label_a, {"raw": live.agent_path(machine, run_id)})
    fleet = Fleet(root, bin_dir, Path(args.fleet_config), token, args.fleet_listen, args.fleet_url,
                  agents)
    daemon = None
    try:
        outposts = sorted(fleet.outpost_names())
        if outposts:
            log(f"waiting for outpost machines {outposts} to connect to {fleet.listen} "
                "(a same-host outpost proves the Machine API path, not remoteness)")
            try:
                wait_for("the outposts", lambda: fleet.connected_outposts() == outposts or None,
                         args.outpost_timeout)
                checks.record("outpost machines connected", True, ", ".join(outposts))
            except TimeoutError as e:
                checks.record("outpost machines connected", False, str(e))
                raise RuntimeError("outposts did not connect") from e
        machine_a = fleet.machine_with_label(args.label_a)
        machine_b = fleet.machine_with_label(args.label_b)
        github = None
        if args.live_actions:
            problem = live.remote_root_problem(machine_a)
            if problem:
                log(f"--live-actions: {problem} (the stand-in agent is installed under it)")
                return 2
            repo = live.github_repo(args.repo_url)
            if repo:
                github = live.GitHub(repo, os.environ.get(args.forge_token_env, "").strip())
        daemon = Daemon(root, bin_dir, fleet.url, fleet.token)
        log(f"daemon {daemon.url} (log {daemon.log_path}), fleet {fleet.url} (log {fleet.log_path})")

        project = f"fleet-e2e-{run_id}"
        registration = {"name": project, "description": "fleet e2e", "vcs": "git",
                        "clone_url": args.repo_url}
        if args.local_clone:
            project_dir = root / "project"
            subprocess.run(["git", "clone", "-q", args.repo_url, str(project_dir)], check=True)
            registration["path"] = str(project_dir)
        daemon.post("/api/projects", registration)
        checks.record(
            "the project needs no clone on the daemon host",
            args.local_clone or daemon.get(f"/api/projects/{project}").get("remote_only") is True,
            "registered with a clone URL only" if not args.local_clone else "--local-clone",
        )
        daemon.post("/api/users", {"name": USER})
        if args.forge_token_env:
            forge_token = os.environ.get(args.forge_token_env, "").strip()
            if not forge_token:
                log(f"{args.forge_token_env} is empty")
                return 2
            host = re.sub(r"^(?:https?://|ssh://)?(?:[^@/]+@)?([^/:]+).*$", r"\1", args.repo_url)
            daemon.post(f"/api/users/{USER}/forge-tokens", {"host": host, "token": forge_token})
        daemon.post("/api/secret-env-names", {"name": SECRET_NAME})
        if args.live_actions:
            # A `raw` profile: the stand-in agent a cell installs on machine A.
            # A raw profile needs an executable, and the daemon refuses a
            # path-shaped one for a remote machine; the fleet replaces this
            # bare name with the machine's [machine.agents] raw path (with
            # --fleet-url, add `raw = <agent path>` there yourself).
            daemon.post("/api/agent-profiles", {"name": live.AGENT_PROFILE, "backend": "raw",
                                                "executable": live.AGENT_PROFILE})

        # The squad env is where a registered secret normally comes from.
        text = squad_toml(project, run_id, args.label_a, args.label_b, args.base, machine_a,
                          machine_b, args.live_actions, args.prompt_cell)
        (root / "squad.toml").write_text(text)
        squad = str(daemon.post("/api/squads", {"toml": text, "hold": True})["squad_id"])
        daemon.post(f"/api/squads/{squad}/env", {"set": {SECRET_NAME: secret_value}})
        # Submit materializes in the background (granting machines and
        # making the review's worktrees on them); a held squad is queued after.
        held = wait_for(
            f"{squad} to finish materializing",
            lambda: (s := squad_state(daemon, squad)) not in ("", "materializing") and s,
            args.timeout,
        )
        if held != "queued":
            raise RuntimeError(f"{squad} is {held} after materializing; see {daemon.log_path}")
        daemon.post(f"/api/squads/{squad}/activate")
        log(f"submitted {squad}")

        wait_for(f"{squad} to start", lambda: squad_state(daemon, squad) not in ("", "materializing", "queued"), 600)
        if args.restart_fleet:
            log("restarting ralphus-fleet mid-run")
            fleet.stop()
            fleet.start()
        state = wait_for(
            f"{squad} to finish",
            lambda: (s := squad_state(daemon, squad)) in ("done", "failed", "cancelled") and s,
            args.timeout,
        )
        checks.record("squad finished done", state == "done", f"state {state}")

        # Released ones too: a task's machine is handed back once its branch
        # is published and the squad is done, which can beat this check.
        granted = allocations(daemon, all_states=True)
        mine = [a for a in granted if a.get("squad_id") == squad]
        names = sorted({str(a.get("description")) for a in mine})
        expected = sorted({str(machine_a.get("name") or machine_a["host"]),
                           str(machine_b.get("name") or machine_b["host"])})
        checks.record("each task ran on the machine its labels asked for", names == expected,
                      f"allocated {names}, expected {expected}")

        reviews = wait_for("the review", lambda: [g for g in daemon.get("/api/guardians")
                                                 if g.get("squad_id") == squad], 120)
        review = str(reviews[0]["id"])
        status = wait_for(
            f"review {review} to settle",
            lambda: (s := str(daemon.get(f"/api/guardians/{review}").get("status")))
            in ("in_review", "merge_failed", "cancelled") and s,
            args.timeout,
        )
        checks.record("review reached in_review on its machine", status == "in_review", f"status {status}")

        live_check = None
        if args.live_actions and status == "in_review":
            live_check = live.LiveActions(daemon, checks, wait_for, review, squad, run_id, machine_a,
                                          args.live_timeout)
            live_check.after_review()

        rows = []
        if args.pr:
            try:
                # One whole-stack request; submission runs in the background,
                # so wait for a row with a URL for every enabled branch.
                daemon.post(f"/api/guardians/{review}/pull-requests", {"prs": [{}]})
                branches = [b for b in daemon.get(f"/api/guardians/{review}").get("branches", [])
                            if b.get("enabled", True)]
                rows = wait_for(
                    f"{len(branches)} pull request(s)",
                    lambda: (r := daemon.get(f"/api/guardians/{review}/pull-requests"))
                    and len(r) >= len(branches) and all(p.get("pr_url") for p in r) and r,
                    600,
                    every=5,
                )
                checks.record("pull request opened from the machine", True,
                              ", ".join(str(p.get("pr_url")) for p in rows))
            except (RuntimeError, TimeoutError) as e:
                rows = []
                checks.record("pull request opened from the machine", False, str(e))
        if live_check and rows:
            try:
                live_check.after_pr(github, rows, args.live_timeout)
            except (urllib.error.URLError, KeyError, ValueError) as e:
                checks.record("the PRs show the live results (forge API)", False, str(e)[:300])
            live_check.audit(project, args.label_a, args.base, forge_token)

        windows_or_mac = "macos" if any(
            m.get("os") == "windows" for m in fleet.config.get("machine", [])) else "windows"
        bad = daemon.post("/api/squads", {"toml": (
            "no_review = true\n\n"
            f'[[task]]\nname = "nowhere"\nproject = "{project}"\n'
            f'machine = {{ provider = "fleet", os = "{windows_or_mac}" }}\n'
            '[[task.cell]]\n'
            f'cwd = "<<ralphus:new-worktree/fleet-e2e-nowhere-{run_id}?upstream={args.base}>>"\n'
            'command = "true"\nmode = "raw"\n')})
        bad_squad = str(bad["squad_id"])
        bad_state = wait_for(f"{bad_squad} to fail",
                             lambda: (s := squad_state(daemon, bad_squad)) in ("done", "failed") and s, 300)
        checks.record(
            "an unservable request fails with the provider's reasons",
            bad_state == "failed"
            and "no_matching_machine" in daemon.log_path.read_text(errors="replace"),
            f"state {bad_state}",
        )

        daemon.post(f"/api/guardians/{review}/cancel")
        try:
            remaining = wait_for(
                "the machines to be handed back",
                lambda: "released" if not [a for a in allocations(daemon) if a.get("squad_id") == squad
                                           or a.get("guardian_id") == review] else None,
                args.release_timeout,
                every=5,
            )
        except TimeoutError as e:
            remaining = str(e)
        checks.record("machines handed back after the review was cancelled", remaining == "released",
                      "" if remaining == "released" else remaining[:300])

        daemon_log = daemon.log_path.read_text(errors="replace")
        refusals = [line for line in daemon_log.splitlines() if STRICT_REFUSAL in line]
        checks.record("no remote work ran on the daemon host (strict mode)", not refusals,
                      refusals[0][:200] if refusals else "")
        checks.record("the secret is not in the daemon log", secret_value not in daemon_log)
        if forge_token:
            leaks = live.scan_for_token(root, forge_token)
            checks.record("the forge token is not in the daemon's or fleet's logs and state", not leaks,
                          ", ".join(leaks[:5]))
        task_a = next(t for t in daemon.get(f"/api/squads/{squad}")["tasks"] if t["name"] == "on-a")
        checks.record("the secret reached the cell and is not on the machine's disk",
                      task_a["cells"][0]["state"] == "done",
                      f"checking cell {task_a['cells'][0]['state']}")
        if args.prompt_cell:
            agent_task = next(t for t in daemon.get(f"/api/squads/{squad}")["tasks"]
                              if t["name"] == "agent-on-b")
            checks.record(f"a {args.prompt_cell} prompt cell ran on machine B",
                          agent_task["cells"][0]["state"] == "done",
                          f"agent cell {agent_task['cells'][0]['state']}")
    except (RuntimeError, TimeoutError, subprocess.CalledProcessError, KeyError) as e:
        checks.record("harness ran to the end", False, str(e))
    finally:
        if not args.keep:
            if daemon:
                daemon.stop()
            fleet.stop()
        else:
            log(f"left running: daemon {daemon.url if daemon else '-'}, fleet {fleet.url}")
    log(f"state, logs and configs are under {root}")
    return checks.report()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--fleet-config", required=True, help="ralphus-fleet.toml describing the machines")
    parser.add_argument("--repo-url", required=True, help="test repository both machines can clone and push to")
    parser.add_argument("--label-a", required=True, help="label of the machine for task A and the review")
    parser.add_argument("--label-b", required=True, help="label of the machine for task B")
    parser.add_argument("--base", default="main", help="the repository's base branch (default main)")
    parser.add_argument("--bin-dir", default="target/debug", help="directory holding the built binaries")
    parser.add_argument("--state-root", default=str(Path(os.environ.get("TEMP", "/tmp")) / f"fleet-e2e-{os.getpid()}"))
    parser.add_argument("--forge-token-env", help="variable holding a forge token for HTTPS pushes and --pr")
    parser.add_argument("--pr", action="store_true", help="also open the review's pull request (needs --forge-token-env)")
    parser.add_argument("--no-fleet-restart", dest="restart_fleet", action="store_false",
                        help="do not restart ralphus-fleet while the squad runs")
    parser.add_argument("--timeout", type=float, default=1800, help="seconds to wait for the squad and the review")
    parser.add_argument("--release-timeout", type=float, default=300, help="seconds to wait for machines to be handed back")
    parser.add_argument("--keep", action="store_true", help="leave the daemon and ralphus-fleet running")
    parser.add_argument("--fleet-listen", help="HOST:PORT for ralphus-fleet to listen on (default a free "
                        "loopback port); outposts connect to it, so fix it when any machine is an outpost")
    parser.add_argument("--fleet-url", help="use a ralphus-fleet you run yourself at this URL (its "
                        f"service token in ${FLEET_TOKEN_ENV}, --fleet-config describing its machines) "
                        "instead of starting one, e.g. where outposts can reach it; it is not restarted")
    parser.add_argument("--outpost-timeout", type=float, default=120,
                        help="seconds to wait for outpost machines to connect")
    parser.add_argument("--live-actions", action="store_true",
                        help="also check live actions and exhibits on the review's machine, model-free "
                             "(needs --pr; PR bodies are read through the GitHub API)")
    parser.add_argument("--live-timeout", type=float, default=900,
                        help="seconds to wait for each live run and for the PR to show its results")
    parser.add_argument("--local-clone", action="store_true",
                        help="also clone the repository on the daemon host and register that path "
                             "(default: a remote-only project with no clone here)")
    parser.add_argument("--prompt-cell", metavar="AGENT:MODEL",
                        help="also run a real prompt cell with this agent (e.g. claude-code:haiku) on "
                             "machine B, using the machine's own agent installation and login; "
                             "costs model usage")
    args = parser.parse_args()
    if args.pr and not args.forge_token_env:
        parser.error("--pr needs --forge-token-env")
    if args.live_actions and not args.pr:
        parser.error("--live-actions needs --pr (exhibits and live results are checked in the PR)")
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
