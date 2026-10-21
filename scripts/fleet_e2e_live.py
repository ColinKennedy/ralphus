"""Live actions and exhibits on a remote machine, for `fleet_e2e.py --live-actions`.

No model is called. A stand-in agent -- a POSIX `sh` script on a Linux/macOS
machine, a small console program compiled on the machine by Windows
PowerShell's `Add-Type` on a Windows one -- is registered as a `raw` agent
profile (the prompt arrives as its last argument, its stdout is the reply)
and is installed on the review's machine by a command cell. It is used twice:

- by a prompt cell, whose reply carries a `RALPHUS_LIVE_ACTION:` handoff with
  two `EXHIBIT:` requests (a screenshot and a GIF), so the review gets one
  live action with two exhibit slots;
- as the review's resolver agent, which is what runs live actions: given a
  live run's prompt it records the machine's hostname and working directory
  in a new file, commits it, writes a PNG and a GIF into the run's exhibit
  directory and reports them with `RALPHUS_EXHIBIT:` lines, then ends with a
  `RALPHUS_LIVE_RESULT:` line.

Any other prompt (the PR writer, a conflict resolution) gets a fixed reply
that ralphus cannot use, so those fall back to their deterministic paths.
"""

from __future__ import annotations

import base64
import json
import re
import struct
import urllib.error
import urllib.parse
import urllib.request
import zlib
from collections.abc import Callable
from pathlib import Path

AGENT_PROFILE = "fleet-e2e-agent"
FORGE_SECRET_NAME = "RALPHUS_E2E_FORGE_TOKEN"
RUN_SETTLED = ("finished", "failed", "cancelled", "over_budget", "interrupted")
INTEGRATION_BUSY = ("pending", "integrating")

LIVE_LINE = re.compile(
    r"fleet-e2e-live host=(?P<host>\S+) cwd=(?P<cwd>.+?) exhibit_dir=(?P<dir>.*?) file=(?P<file>\S+)"
)


def label(run: str) -> str:
    """The live action's label: the first line of the stand-in agent's handoff."""
    return f"Fleet e2e live check {run}"


# ---- exhibit files -----------------------------------------------------------


def png_bytes(width: int = 64, height: int = 32) -> bytes:
    """A small RGB PNG of coloured bands, with no black border (a black
    border would be cropped). Kept tiny: it travels inside a cell command."""
    rows = bytearray()
    for y in range(height):
        rows.append(0)  # filter: none
        for x in range(width):
            rows += bytes((40 + 50 * (x // 16), 90 + 40 * (y // 8), 200))

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(bytes(rows), 9))
        + chunk(b"IEND", b"")
    )


def _gif_lzw(indices: list[int]) -> bytes:
    """GIF image data that never grows the code table: 8-bit codes, one per
    pixel, with a clear code often enough that the decoder's table stays below
    256 entries."""
    min_code_size = 7
    clear, end = 1 << min_code_size, (1 << min_code_size) + 1
    codes: list[int] = []
    for start in range(0, len(indices), 100):
        codes.append(clear)
        codes.extend(indices[start : start + 100])
    codes.append(end)
    stream = bytearray()
    bits = nbits = 0
    for code in codes:
        bits |= code << nbits
        nbits += min_code_size + 1
        while nbits >= 8:
            stream.append(bits & 0xFF)
            bits >>= 8
            nbits -= 8
    if nbits:
        stream.append(bits & 0xFF)
    out = bytearray([min_code_size])
    for start in range(0, len(stream), 255):
        block = stream[start : start + 255]
        out.append(len(block))
        out += block
    out.append(0)
    return bytes(out)


def gif_bytes(width: int = 32, height: int = 16, frames: int = 4) -> bytes:
    """A small looping GIF: a bar moving across a coloured background."""
    palette = bytes((30, 110, 200, 240, 200, 40, 60, 170, 90, 250, 250, 250))
    out = bytearray(b"GIF89a")
    out += struct.pack("<HHBBB", width, height, 0x91, 0, 0)
    out += palette
    out += b"\x21\xff\x0bNETSCAPE2.0\x03\x01\x00\x00\x00"
    bar = max(1, width // frames)
    for frame in range(frames):
        pixels = [
            1 if frame * bar <= x < (frame + 1) * bar else (2 if y < height // 2 else 0)
            for y in range(height)
            for x in range(width)
        ]
        out += b"\x21\xf9\x04\x00" + struct.pack("<H", 40) + b"\x00\x00"
        out += b"\x2c" + struct.pack("<HHHH", 0, 0, width, height) + b"\x00"
        out += _gif_lzw(pixels)
    out += b"\x3b"
    return bytes(out)


# ---- the stand-in agent --------------------------------------------------------

SH_AGENT = r"""#!/bin/sh
# Stand-in agent for scripts/fleet_e2e.py (agent profile backend "raw"); no
# model is called. The prompt is the last argument; stdout is the reply.
run='@RUN@'
for arg in "$@"; do prompt=$arg; done
case $prompt in
"Live action: Fleet e2e live check $run"*) ;;
*"FLEET_E2E_HANDOFF $run"*)
    printf '%s\n' \
        "Nothing to change; this cell only hands off a live check." \
        "RALPHUS_LIVE_ACTION: Fleet e2e live check $run" \
        "Record where this live test runs and commit it. FLEET_E2E_LIVE $run" \
        "EXHIBIT: screenshot | a still image made on the review's machine" \
        "EXHIBIT: gif | an animation made on the review's machine" \
        "RALPHUS_GHOST: (nothing to report)"
    exit 0 ;;
*)
    echo "fleet-e2e agent: no scripted answer for this prompt"
    exit 0 ;;
esac
dir=${RALPHUS_EXHIBIT_DIR:-}
if [ -z "$dir" ]; then
    dir=$(printf '%s\n' "$prompt" | sed -n 's/^.*saved in \(.*\)\. Capture the final.*$/\1/p' | head -n 1)
fi
host=$(hostname)
here=$(pwd)
file="fleet-e2e-live-$run-$(date +%s)-$$.txt"
{ echo "host=$host"; echo "cwd=$here"; echo "exhibit_dir=$dir"; uname -a; } > "$file"
if ! git add "$file" || ! git -c user.email=fleet-e2e@example.invalid -c user.name=Fleet-E2E \
        commit -q -m "fleet e2e live run $run"; then
    echo "fleet-e2e agent: commit failed" >&2
    exit 1
fi
echo "fleet-e2e-live host=$host cwd=$here exhibit_dir=$dir file=$file"
printf '%s\n' "$prompt" | sed -n 's/^- \([0-9][0-9]*\.[0-9][0-9]*\) (\([a-z-]*\)):.*$/\1 \2/p' |
while read -r slot kind; do
    case $kind in
    gif) name="slot-$slot.gif"; data='@GIF@' ;;
    screenshot) name="slot-$slot.png"; data='@PNG@' ;;
    *) echo "RALPHUS_EXHIBIT: {\"slot\": \"$slot\", \"missing\": \"the stand-in agent makes only gif and screenshot exhibits\"}"; continue ;;
    esac
    if [ -n "$dir" ] && [ -d "$dir" ] && printf '%s' "$data" | base64 -d > "$dir/$name"; then
        echo "RALPHUS_EXHIBIT: {\"slot\": \"$slot\", \"file\": \"$name\", \"caption\": \"fleet e2e $kind made on $host\"}"
    else
        echo "RALPHUS_EXHIBIT: {\"slot\": \"$slot\", \"missing\": \"no exhibit directory\"}"
    fi
done
echo "RALPHUS_LIVE_RESULT: {\"verdict\": \"passed\", \"summary\": \"ran on $host\", \"fix_attempted\": false, \"fix_confirmed\": false, \"recipe\": [\"hostname\"]}"
"""

# C# 5 (what Windows PowerShell 5.1's Add-Type compiles): no string
# interpolation, no expression-bodied members.
CS_AGENT = r"""using System;
using System.Diagnostics;
using System.IO;
using System.Text.RegularExpressions;

public static class FleetE2eAgent
{
    const string RunId = "@RUN@";
    const string PngData = "@PNG@";
    const string GifData = "@GIF@";

    public static int Main(string[] args)
    {
        string prompt = args.Length > 0 ? args[args.Length - 1] : "";
        if (prompt.StartsWith("Live action: Fleet e2e live check " + RunId, StringComparison.Ordinal))
        {
            return Live(prompt);
        }
        if (prompt.IndexOf("FLEET_E2E_HANDOFF " + RunId, StringComparison.Ordinal) >= 0)
        {
            Console.WriteLine("Nothing to change; this cell only hands off a live check.");
            Console.WriteLine("RALPHUS_LIVE_ACTION: Fleet e2e live check " + RunId);
            Console.WriteLine("Record where this live test runs and commit it. FLEET_E2E_LIVE " + RunId);
            Console.WriteLine("EXHIBIT: screenshot | a still image made on the review's machine");
            Console.WriteLine("EXHIBIT: gif | an animation made on the review's machine");
            Console.WriteLine("RALPHUS_GHOST: (nothing to report)");
            return 0;
        }
        Console.WriteLine("fleet-e2e agent: no scripted answer for this prompt");
        return 0;
    }

    static int Git(string arguments)
    {
        ProcessStartInfo info = new ProcessStartInfo("git", arguments);
        info.UseShellExecute = false;
        using (Process process = Process.Start(info))
        {
            process.WaitForExit();
            return process.ExitCode;
        }
    }

    static int Live(string prompt)
    {
        string dir = Environment.GetEnvironmentVariable("RALPHUS_EXHIBIT_DIR");
        if (String.IsNullOrEmpty(dir))
        {
            Match m = Regex.Match(prompt, @"saved in (.+?)(?: \(also in \$RALPHUS_EXHIBIT_DIR\))?\. Capture the final");
            dir = m.Success ? m.Groups[1].Value : "";
        }
        string host = Environment.MachineName;
        string here = Directory.GetCurrentDirectory();
        string file = "fleet-e2e-live-" + RunId + "-" + DateTime.UtcNow.Ticks + ".txt";
        File.WriteAllText(file, "host=" + host + "\ncwd=" + here + "\nexhibit_dir=" + dir + "\nos=" + Environment.OSVersion + "\n");
        if (Git("add " + file) != 0
            || Git("-c user.email=fleet-e2e@example.invalid -c user.name=Fleet-E2E commit -q -m \"fleet e2e live run " + RunId + "\"") != 0)
        {
            Console.Error.WriteLine("fleet-e2e agent: commit failed");
            return 1;
        }
        Console.WriteLine("fleet-e2e-live host=" + host + " cwd=" + here + " exhibit_dir=" + dir + " file=" + file);
        foreach (Match slot in Regex.Matches(prompt, @"^- (\d+\.\d+) \(([a-z-]+)\):", RegexOptions.Multiline))
        {
            string id = slot.Groups[1].Value;
            string kind = slot.Groups[2].Value;
            string name;
            string data;
            if (kind == "gif") { name = "slot-" + id + ".gif"; data = GifData; }
            else if (kind == "screenshot") { name = "slot-" + id + ".png"; data = PngData; }
            else
            {
                Console.WriteLine("RALPHUS_EXHIBIT: {\"slot\": \"" + id + "\", \"missing\": \"the stand-in agent makes only gif and screenshot exhibits\"}");
                continue;
            }
            if (dir.Length > 0 && Directory.Exists(dir))
            {
                File.WriteAllBytes(Path.Combine(dir, name), Convert.FromBase64String(data));
                Console.WriteLine("RALPHUS_EXHIBIT: {\"slot\": \"" + id + "\", \"file\": \"" + name + "\", \"caption\": \"fleet e2e " + kind + " made on " + host + "\"}");
            }
            else
            {
                Console.WriteLine("RALPHUS_EXHIBIT: {\"slot\": \"" + id + "\", \"missing\": \"no exhibit directory\"}");
            }
        }
        Console.WriteLine("RALPHUS_LIVE_RESULT: {\"verdict\": \"passed\", \"summary\": \"ran on " + host + "\", \"fix_attempted\": false, \"fix_confirmed\": false, \"recipe\": [\"hostname\"]}");
        return 0;
    }
}
"""


def _fill(template: str, run: str) -> str:
    return (
        template.replace("@RUN@", run)
        .replace("@PNG@", base64.b64encode(png_bytes()).decode())
        .replace("@GIF@", base64.b64encode(gif_bytes()).decode())
    )


def is_windows(machine: dict) -> bool:
    return str(machine.get("os", "")).lower() == "windows"


def shell_for(machine: dict) -> str:
    # Windows PowerShell 5.1, not pwsh: its Add-Type can compile a console
    # program, which the stand-in agent on Windows is.
    return "powershell" if is_windows(machine) else "sh"


def agent_path(machine: dict, run: str) -> str:
    """Where the stand-in agent is installed on `machine` (an absolute path:
    the agent profile names it before anything runs there)."""
    root = str(machine["remote_root"]).rstrip("/\\")
    if is_windows(machine):
        return root.replace("/", "\\") + f"\\fleet-e2e-agent-{run}\\agent.exe"
    return f"{root}/fleet-e2e-agent-{run}/agent"


def remote_root_problem(machine: dict) -> str | None:
    root = str(machine.get("remote_root", ""))
    if is_windows(machine):
        ok = re.match(r"^[A-Za-z]:[/\\]", root) is not None
    else:
        ok = root.startswith("/")
    return None if ok else f"machine {machine.get('name')!r} needs an absolute remote_root, not {root!r}"


def install_command(machine: dict, run: str) -> str:
    """A cell command that installs the stand-in agent at `agent_path`. Only
    single quotes on Windows: the command reaches powershell.exe through a
    Windows command line."""
    path = agent_path(machine, run)
    if is_windows(machine):
        source = base64.b64encode(_fill(CS_AGENT, run).encode()).decode()
        folder = path.rsplit("\\", 1)[0]
        return (
            f"$d='{folder}'; New-Item -ItemType Directory -Force -Path $d | Out-Null; "
            f"$src=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{source}')); "
            "Add-Type -TypeDefinition $src -Language CSharp -OutputAssembly (Join-Path $d 'agent.exe') "
            "-OutputType ConsoleApplication -ErrorAction Stop; 'fleet-e2e agent installed'"
        )
    source = base64.b64encode(_fill(SH_AGENT, run).encode()).decode()
    folder = path.rsplit("/", 1)[0]
    return (
        f"mkdir -p '{folder}' && printf '%s' '{source}' | base64 -d > '{path}' "
        f"&& chmod +x '{path}' && echo 'fleet-e2e agent installed'"
    )


def commit_command(machine: dict, file: str) -> str:
    """Commit a file naming the machine's OS and, on its last line, hostname."""
    if is_windows(machine):
        return (
            f"$f='{file}'; [Environment]::OSVersion.VersionString | Set-Content -Encoding ascii $f; "
            "[Environment]::MachineName | Add-Content -Encoding ascii $f; git add $f; "
            "if ($LASTEXITCODE) { exit 1 }; "
            "git -c user.email=fleet-e2e@example.invalid -c user.name=Fleet-E2E commit -q -m $f; "
            "if ($LASTEXITCODE) { exit 1 }"
        )
    return (
        f"uname -a > {file} && hostname >> {file} && git add {file} && "
        f"git -c user.email=fleet-e2e@example.invalid -c user.name=Fleet-E2E commit -q -m {file}"
    )


def secret_check_command(machine: dict, variable: str, with_cache: bool = False) -> str:
    """Fail when the value of environment variable `variable` is missing, or is
    found in a file under the machine's remote_root (and, with `with_cache`,
    ralphus's cache directory there, where exhibits are kept). Reads the value
    only from the environment: it must not appear in anything the machine
    stores, this command text included."""
    root = str(machine["remote_root"]).rstrip("/\\")
    if is_windows(machine):
        roots = f"@('{root}'" + (", (Join-Path $env:LOCALAPPDATA 'ralphus'))" if with_cache else ")")
        return (
            f"$v=$env:{variable}; if (-not $v) {{ exit 3 }}; "
            f"foreach ($r in {roots}) {{ if (Test-Path -LiteralPath $r) {{ "
            "$hit = Get-ChildItem -LiteralPath $r -Recurse -File -Force -ErrorAction SilentlyContinue "
            "| Select-String -SimpleMatch -Pattern $v -List -ErrorAction SilentlyContinue "
            "| Select-Object -First 1; if ($hit) { 'found under ' + $r; exit 1 } } }; "
            "'secret-seen-and-not-on-disk'"
        )
    roots = f"'{root}'" + (' "${XDG_CACHE_HOME:-$HOME/.cache}/ralphus"' if with_cache else "")
    return (
        f'test -n "${variable}" && for d in {roots}; do '
        f'if [ -d "$d" ] && grep -rqsF -- "${variable}" "$d"; then echo "found under $d"; exit 1; fi; '
        "done && echo secret-seen-and-not-on-disk"
    )


# ---- task-file pieces -----------------------------------------------------------


def handoff_cells(machine: dict, run: str, cwd: str, review: str) -> str:
    """The cells added to task A: install the stand-in agent, then a prompt
    cell (run by it) whose reply is the live handoff."""
    return f"""
[[task.cell]]
id = "install-agent"
cwd = "{cwd}"
command = {json.dumps(install_command(machine, run))}
mode = "raw"
shell = "{shell_for(machine)}"
review = "{review}"

[[task.cell]]
id = "handoff"
cwd = "{cwd}"
depends_on = ["install-agent"]
agent = "{AGENT_PROFILE}"
prompt = "Hand off a live check for this change. FLEET_E2E_HANDOFF {run}"
review = "{review}"
"""


def review_keys() -> str:
    """Extra `[[review]]` keys: live actions on, run by the stand-in agent."""
    return f'agent = "{AGENT_PROFILE}"\nlive_actions = true\n'


def audit_squad_toml(project: str, run: str, label_a: str, base: str, machine: dict) -> str:
    """A squad whose one cell checks, on the review's machine, that the forge
    token is in none of its files (remote_root and ralphus's cache)."""
    command = secret_check_command(machine, FORGE_SECRET_NAME, with_cache=True)
    return f"""no_review = true

[[task]]
name = "forge-token-audit"
project = "{project}"
machine = {{ provider = "fleet", labels = ["{label_a}"] }}
no_commit_required = true

[[task.cell]]
cwd = "<<ralphus:new-worktree/fleet-e2e-audit-{run}?upstream={base}>>"
command = {json.dumps(command)}
mode = "raw"
shell = "{shell_for(machine)}"
"""


# ---- forge -------------------------------------------------------------------------


def github_repo(repo_url: str) -> str | None:
    """`owner/name` of a github.com repository URL, else None."""
    match = re.match(
        r"^(?:https?://|ssh://)?(?:[^@/]+@)?github\.com[:/]([^/]+)/([^/]+?)(?:\.git)?/?$", repo_url
    )
    return f"{match.group(1)}/{match.group(2)}" if match else None


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


class GitHub:
    """The few GitHub REST reads the checks need. The token is sent only to
    github.com hosts, never to the storage host an asset redirects to."""

    def __init__(self, repo: str, token: str):
        self.repo = repo
        self.token = token

    def _request(self, url: str) -> urllib.request.Request:
        request = urllib.request.Request(url)
        request.add_header("User-Agent", "ralphus-fleet-e2e")
        host = urllib.parse.urlparse(url).hostname or ""
        if host == "github.com" or host.endswith(".github.com"):
            request.add_header("Authorization", f"Bearer {self.token}")
        return request

    def get(self, path: str) -> object:
        request = self._request(f"https://api.github.com/repos/{self.repo}{path}")
        request.add_header("Accept", "application/vnd.github+json")
        with urllib.request.urlopen(request, timeout=60) as reply:
            return json.loads(reply.read().decode())

    def file_text(self, path: str, ref: str) -> str | None:
        quoted = urllib.parse.quote(path)
        try:
            reply = self.get(f"/contents/{quoted}?ref={urllib.parse.quote(ref, safe='')}")
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            raise
        return base64.b64decode(reply.get("content", "")).decode(errors="replace")

    def fetch_head(self, url: str) -> tuple[int, bytes]:
        """Status and first bytes of an uploaded asset, following redirects by
        hand so the token never leaves GitHub."""
        opener = urllib.request.build_opener(_NoRedirect)
        for _ in range(5):
            try:
                with opener.open(self._request(url), timeout=60) as reply:
                    return reply.status, reply.read(16)
            except urllib.error.HTTPError as e:
                if e.code in (301, 302, 303, 307, 308) and e.headers.get("Location"):
                    url = urllib.parse.urljoin(url, e.headers["Location"])
                    continue
                return e.code, b""
        return 0, b""


def magic_kind(head: bytes) -> str:
    if head.startswith(b"\x89PNG"):
        return "png"
    if head.startswith(b"GIF8"):
        return "gif"
    return repr(head[:8])


# ---- the checks ----------------------------------------------------------------------


def _norm(path: str, windows: bool) -> str:
    path = path.replace("\\", "/").rstrip("/")
    return path.lower() if windows else path


def under(path: str, root: str, windows: bool) -> bool:
    path, root = _norm(path, windows), _norm(root, windows)
    return path == root or path.startswith(root + "/")


class LiveActions:
    """Drives and checks one review's live actions and exhibits."""

    def __init__(self, daemon, checks, wait_for: Callable, review: str, squad: str, run: str,
                 machine: dict, timeout: float):
        self.daemon = daemon
        self.checks = checks
        self.wait_for = wait_for
        self.review = review
        self.squad = squad
        self.run = run
        self.machine = machine
        self.windows = is_windows(machine)
        self.timeout = timeout
        self.action_idx: int | None = None
        self.live_files: list[str] = []
        self.hosts: list[str] = []

    def view(self) -> dict:
        return dict(self.daemon.get(f"/api/guardians/{self.review}/live-actions"))

    def _settled(self, run: dict) -> bool:
        return run.get("state") in RUN_SETTLED and run.get("integration") not in INTEGRATION_BUSY

    def _check_run(self, run: dict, what: str) -> None:
        """A finished run happened on the review's machine and its commits
        were carried onto the top branch."""
        root = str(self.machine["remote_root"])
        report = str(run.get("report") or "")
        line = LIVE_LINE.search(report)
        on_machine = (
            run.get("state") == "finished"
            and line is not None
            and under(str(run.get("cwd") or ""), root, self.windows)
            and under(line.group("cwd"), root, self.windows)
            and bool(line.group("dir"))
        )
        detail = (
            f"state {run.get('state')}, cwd {run.get('cwd')}"
            + (f", host {line.group('host')}, exhibit dir {line.group('dir')}" if line else
               f", report without the agent's line: {report[:200]!r}, error {run.get('error')!r}")
        )
        self.checks.record(f"the {what} live run ran on the review's machine", on_machine, detail)
        if line:
            self.live_files.append(line.group("file"))
            self.hosts.append(line.group("host"))
        self.checks.record(
            f"the {what} live run's commits were carried onto the top branch",
            bool(run.get("commits")) and run.get("integration") == "integrated",
            f"{len(run.get('commits') or [])} commit(s), integration {run.get('integration')}: "
            f"{run.get('integration_detail')}",
        )

    def after_review(self) -> None:
        """Wait for the automatic run, check it, then run the action by hand
        once and check that run too."""
        want = label(self.run)
        try:
            view = self.wait_for(
                "the automatic live-action run",
                lambda: (v := self.view()).get("auto_ran_at_ms")
                and [r for r in v.get("runs", []) if r.get("trigger") == "auto"]
                and all(self._settled(r) for r in v.get("runs", []))
                and v,
                self.timeout,
                5,
            )
        except TimeoutError as e:
            self.checks.record("the cell's live handoff became a live action", False, str(e))
            return
        actions = [a for a in view.get("actions", []) if a.get("label") == want]
        self.checks.record(
            "the cell's live handoff became a live action",
            len(actions) == 1 and any(self.squad in str(s) for s in actions[0].get("source_cells", [])),
            f"actions {[a.get('label') for a in view.get('actions', [])]}",
        )
        if not actions:
            return
        self.action_idx = int(actions[0]["idx"])
        auto = [r for r in view["runs"] if r.get("trigger") == "auto" and r.get("action_idx") == self.action_idx]
        if auto:
            self._check_run(auto[0], "automatic")

        queued = self.daemon.post(f"/api/guardians/{self.review}/live-actions/run", {"index": self.action_idx})
        ids = set((queued or {}).get("queued") or [])
        try:
            view = self.wait_for(
                "the manual live run",
                lambda: (v := self.view())
                and [r for r in v.get("runs", []) if r.get("run_id") in ids and self._settled(r)]
                and v,
                self.timeout,
                5,
            )
            manual = [r for r in view["runs"] if r.get("run_id") in ids]
            self.checks.record("a manual run was queued", bool(manual), f"queued {sorted(ids)}")
            if manual:
                self._check_run(manual[0], "manual")
        except TimeoutError as e:
            self.checks.record("the manual live run ran on the review's machine", False, str(e))

        top = self.top_branch()
        messages = (
            self.daemon.get(f"/api/guardians/{self.review}/branches/{top['id']}/messages").get("messages", [])
            if top else []
        )
        posted = [m for m in messages if f'Live action "{want}" finished' in str(m.get("text"))
                  and "fleet-e2e-live host=" in str(m.get("text"))]
        self.checks.record("the live runs' reports were posted into the review", len(posted) >= 2,
                           f"{len(posted)} report message(s) on the top branch")

        slots = self.slots()
        self.checks.record(
            "the exhibits were uploaded from the machine",
            len(slots) == 2 and all(s.get("url") and not s.get("has_file") for s in slots),
            "; ".join(f"{s.get('kind')}: {s.get('delivered_kind')} {s.get('url') or s.get('missing_reason')}"
                      for s in slots) or "no exhibit slots",
        )

    def top_branch(self) -> dict | None:
        branches = [b for b in self.daemon.get(f"/api/guardians/{self.review}").get("branches", [])
                    if b.get("enabled", True) and b.get("worktree")]
        return max(branches, key=lambda b: b.get("position", 0)) if branches else None

    def slots(self) -> list[dict]:
        listing = self.daemon.get(f"/api/guardians/{self.review}/exhibits")
        return [s for s in listing.get("exhibits", []) if s.get("action_idx") == self.action_idx]

    def after_pr(self, github: GitHub | None, rows: list[dict], timeout: float) -> None:
        """The PR of the branch the action came from shows its results and
        exhibits; the other PR shows neither; the commits are on the top
        branch's PR head; the uploaded files are reachable."""
        if self.action_idx is None:
            return
        if github is None:
            self.checks.record("PR descriptions checked through the forge API", False,
                               "only github.com repositories are checked")
            return
        want = label(self.run)
        pulls: dict[str, dict] = {}
        for row in rows:
            if row.get("pr_number") and row.get("state", "open") == "open":
                pulls[str(row.get("branch_id"))] = dict(github.get(f"/pulls/{row['pr_number']}"))
        own = [p for p in pulls.values() if f"fleet-e2e-a-{self.run}" in p["head"]["ref"]]
        other = [p for p in pulls.values() if f"fleet-e2e-b-{self.run}" in p["head"]["ref"]]
        if not own:
            self.checks.record("the live results and exhibits are in its PR", False,
                               f"no PR for task A's branch among {[p['head']['ref'] for p in pulls.values()]}")
            return
        number = own[0]["number"]

        def body_ready() -> str | None:
            body = str(github.get(f"/pulls/{number}").get("body") or "")
            urls = [s.get("url") for s in self.slots()]
            ready = (
                "<!-- ralphus:live-test-results:start -->" in body
                and want in body
                and "<!-- ralphus:exhibits:start -->" in body
                and urls and all(u and u in body for u in urls)
            )
            return body if ready else None

        try:
            body = self.wait_for(f"PR #{number} to show the live results", body_ready, timeout, 10)
            self.checks.record("the live results and exhibits are in its PR", True, own[0]["html_url"])
        except TimeoutError as e:
            self.checks.record("the live results and exhibits are in its PR", False, str(e)[:300])
            body = None
        for pr in other:
            text = str(github.get(f"/pulls/{pr['number']}").get("body") or "")
            self.checks.record(
                "another branch's PR shows none of them (per-PR scope)",
                want not in text and f"ralphus:exhibit:start action={self.action_idx} " not in text,
                pr["html_url"],
            )
        if body:
            for slot in self.slots():
                status, head = github.fetch_head(str(slot["url"]))
                expected = "gif" if slot.get("delivered_kind") == "gif" else "png"
                self.checks.record(
                    f"the uploaded {slot.get('delivered_kind')} is reachable",
                    status == 200 and magic_kind(head) == expected,
                    f"HTTP {status}, {magic_kind(head)}",
                )

        top = self.top_branch()
        top_pr = pulls.get(str(top["id"])) if top else None
        if not top_pr:
            self.checks.record("the live commits are on the top branch's PR", False, "no PR for the top branch")
            return
        ref = top_pr["head"]["ref"]
        texts = [github.file_text(f, ref) for f in self.live_files]
        self.checks.record(
            "the live commits are on the top branch's PR",
            bool(texts) and all(t is not None for t in texts),
            f"{len([t for t in texts if t])}/{len(texts)} live file(s) on {ref}",
        )
        cell_file = github.file_text(f"fleet-e2e-a-{self.run}.txt", own[0]["head"]["ref"])
        cell_host = [line.strip() for line in (cell_file or "").splitlines() if line.strip()][-1:] or [""]
        live_hosts = {line.split("=", 1)[1].strip() for t in texts if t
                      for line in t.splitlines() if line.startswith("host=")}
        self.checks.record(
            "the live runs ran on the same host as task A's cell",
            bool(live_hosts) and all(h.lower() == cell_host[0].lower() for h in live_hosts),
            f"live {sorted(live_hosts)}, cell {cell_host[0]!r}",
        )

    def audit(self, project: str, label_a: str, base: str, forge_token: str) -> None:
        """Check on the review's machine that the forge token is in none of its files."""
        self.daemon.post("/api/secret-env-names", {"name": FORGE_SECRET_NAME})
        text = audit_squad_toml(project, self.run, label_a, base, self.machine)
        squad = str(self.daemon.post("/api/squads", {"toml": text, "hold": True})["squad_id"])
        self.daemon.post(f"/api/squads/{squad}/env", {"set": {FORGE_SECRET_NAME: forge_token}})

        def state() -> str:
            return str(self.daemon.get(f"/api/squads/{squad}").get("state", ""))

        try:
            held = self.wait_for(f"{squad} to materialize",
                                 lambda: (s := state()) not in ("", "materializing") and s, self.timeout)
            if held == "queued":
                self.daemon.post(f"/api/squads/{squad}/activate")
            final = self.wait_for(f"{squad} to finish",
                                  lambda: (s := state()) in ("done", "failed", "cancelled") and s, self.timeout)
        except TimeoutError as e:
            final = str(e)
        self.checks.record("the forge token is not on the machine's disk", final == "done",
                           f"audit squad {squad}: {final}")


def scan_for_token(root: Path, token: str) -> list[str]:
    """Files under the harness's state root holding `token`, except the
    daemon's database (where a stored forge token belongs) and git objects."""
    needle = token.encode()
    hits = []
    for path in sorted(root.rglob("*")):
        if not path.is_file() or path.name.startswith("daemon.db") or "objects" in path.parts:
            continue
        try:
            if needle in path.read_bytes():
                hits.append(str(path.relative_to(root)))
        except OSError:
            continue
    return hits

