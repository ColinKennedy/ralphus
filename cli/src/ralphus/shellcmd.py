"""Shell selection + command construction for `quick-start --command` (RAL-189).

The quick-start `--command` flag (`quick-start manager|reviewer
claude-code|codex --command ...`) accepts three shapes, and this module is
what makes all three behave the way they would if typed straight into a
terminal:

1. **A bare executable** (`claude`, `C:\\tools\\claude.exe`) -- resolved and
   exec'd directly, with ralphus's own arguments appended as real argv
   entries. No shell, no quoting risk.
2. **A single-name script** (`my-claude.ps1`, `launch-claude.sh`) -- resolved
   the same way, but since the OS cannot `exec` it directly it is handed to a
   shell *as an already-resolved absolute path*. That last part matters:
   PowerShell will not run `my-claude.ps1` found in the current directory
   unless it is spelled `.\\my-claude.ps1`, so resolving it here rather than
   re-deriving it inside the shell is what makes the bare name work.
3. **A raw shell command line** (`cd /foo/bar ; claude`, `python
   some_script.py -- super-claude`) -- passed through to a shell verbatim,
   with ralphus's arguments quoted for *that* shell and appended.

`is_compound_shell_command()` (`ralphus.health`, RAL-110) is what splits (1)/
(2) from (3); this module owns everything downstream of that decision.

**Which shell.** Shapes (2) and (3) need an actual shell, and the answer to
"which one" is *the shell that launched `ralphus`* -- so a `;` typed at a
PowerShell prompt keeps meaning what PowerShell says it means, rather than
being reinterpreted by a hardcoded `cmd /C`. `detect_parent_shell()` finds it
by walking the real process ancestry (Toolhelp32 on Windows, `/proc` on
Linux), falling back to environment heuristics (`$SHELL`, `%COMSPEC%`,
`POWERSHELL_DISTRIBUTION_CHANNEL`, `PSModulePath`) and finally to the
platform default. `$RALPHUS_SHELL` overrides the whole thing, and the CLI's
own `--shell` flag overrides that -- so a command authored for bash can be
launched from a PowerShell prompt.

**Lookup order for a bare name** (`find_program()`) deliberately mirrors what
the platform's shells themselves do, so nothing surprising is ever picked up:
Windows shells search the current directory *then* `PATH` (and try each
`%PATHEXT%` suffix); POSIX shells search `PATH` only -- a script in the
current directory must be spelled `./name` there, exactly as at a prompt.
"""

from __future__ import annotations

import ctypes
import os
import shlex
from pathlib import Path

from ralphus.hostos import is_windows

__all__ = [
    "SHELL_AUTO",
    "SHELL_CHOICES",
    "build_compound_command_line",
    "build_program_command_line",
    "detect_parent_shell",
    "find_program",
    "is_directly_executable",
    "quote_for_shell",
    "resolve_shell",
    "shell_spawn_args",
]

SHELL_AUTO = "auto"

#: Every value `--shell`/`$RALPHUS_SHELL` accepts. ``auto`` means "whatever
#: shell launched this `ralphus` process" -- see `detect_parent_shell`.
SHELL_CHOICES = ("auto", "powershell", "pwsh", "cmd", "bash", "sh", "zsh", "fish")

_REAL_SHELLS = frozenset(SHELL_CHOICES) - {SHELL_AUTO}
_POWERSHELLS = frozenset({"powershell", "pwsh"})

# The argv prefix that makes each shell run one command line passed as a
# single argument. PowerShell deliberately does NOT get `-NoProfile`: the
# whole point of this ticket is that `--command` behaves like the same text
# typed at the user's own prompt, and that prompt has their profile loaded
# (a `my-claude` defined as a profile function is a realistic thing to name).
_SHELL_PREFIXES: dict[str, tuple[str, ...]] = {
    "powershell": ("powershell", "-NoLogo", "-Command"),
    "pwsh": ("pwsh", "-NoLogo", "-Command"),
    "cmd": ("cmd.exe", "/C"),
    "bash": ("bash", "-c"),
    "sh": ("sh", "-c"),
    "zsh": ("zsh", "-c"),
    "fish": ("fish", "-c"),
}

# Executable-image suffixes Windows' CreateProcess can launch on its own
# (.bat/.cmd included -- it hands those to cmd.exe implicitly). Anything else
# resolved on PATH -- .ps1, .py, .sh, an extensionless file -- needs a shell.
_WINDOWS_DIRECT_EXEC_SUFFIXES = frozenset({".exe", ".com", ".bat", ".cmd"})

_DEFAULT_PATHEXT = ".COM;.EXE;.BAT;.CMD"

# Process/exe basenames that identify a shell, for ancestry + `$SHELL`/
# `%COMSPEC%` inspection. Keys are compared lowercased and stripped of a
# leading "-" (a login shell reports itself as e.g. "-bash").
_SHELL_EXE_NAMES: dict[str, str] = {
    "powershell": "powershell",
    "powershell.exe": "powershell",
    "pwsh": "pwsh",
    "pwsh.exe": "pwsh",
    "cmd": "cmd",
    "cmd.exe": "cmd",
    "bash": "bash",
    "bash.exe": "bash",
    "sh": "sh",
    "sh.exe": "sh",
    "dash": "sh",
    "zsh": "zsh",
    "zsh.exe": "zsh",
    "fish": "fish",
    "fish.exe": "fish",
}

_ANCESTRY_MAX_DEPTH = 12


# ---- shell selection --------------------------------------------------------


def resolve_shell(value: str | None) -> str:
    """Normalize a `--shell`/`$RALPHUS_SHELL` value to a concrete shell name.

    `None`, `""`, `"auto"`, or anything unrecognized all mean "detect the
    parent shell" -- there is no failure mode here, since argparse's own
    `choices=` already rejects a typo'd `--shell` value before this is
    reached, and an unrecognized `$RALPHUS_SHELL` is better ignored than
    fatal.
    """
    if value:
        normalized = value.strip().lower()
        if normalized in _REAL_SHELLS:
            return normalized
    return detect_parent_shell()


def detect_parent_shell() -> str:
    """Best-effort: which shell launched this `ralphus` process.

    Tried in order: `$RALPHUS_SHELL`; real process ancestry; environment
    heuristics; the platform default (`cmd` on Windows, `sh` elsewhere).
    Ancestry is first among the automatic sources because it is the only one
    that actually distinguishes "run from Windows PowerShell 5.1" from "run
    from cmd.exe" -- `%PSModulePath%` is a persisted machine-level variable
    that cmd.exe inherits too, so it cannot tell those apart on its own.
    """
    override = _shell_kind_from_exe(os.environ.get("RALPHUS_SHELL", ""))
    if override is not None:
        return override

    from_ancestry = _ancestor_shell()
    if from_ancestry is not None:
        return from_ancestry

    if is_windows():
        # pwsh (PowerShell 7+) stamps this into its own environment at
        # startup; Windows PowerShell 5.1 has no equivalent unique marker,
        # which is why %PSModulePath% is only a last-resort hint here.
        if os.environ.get("POWERSHELL_DISTRIBUTION_CHANNEL"):
            return "pwsh"
        from_comspec = _shell_kind_from_exe(Path(os.environ.get("COMSPEC", "")).name)
        if from_comspec is not None:
            return from_comspec
        # `os.environ` upper-cases every key on Windows, so this matches the
        # conventionally mixed-case `PSModulePath` too.
        if os.environ.get("PSMODULEPATH"):
            return "powershell"
        return "cmd"

    from_login = _shell_kind_from_exe(Path(os.environ.get("SHELL", "")).name)
    return from_login if from_login is not None else "sh"


def _shell_kind_from_exe(name: str) -> str | None:
    """Map a process/executable basename (e.g. `-bash`, `pwsh.exe`) to a shell."""
    cleaned = name.strip().lstrip("-").lower()
    return _SHELL_EXE_NAMES.get(cleaned)


def _ancestor_shell() -> str | None:
    """The nearest shell among this process's ancestors, or None if unknown."""
    if is_windows():
        return _windows_ancestor_shell()
    return _proc_ancestor_shell()


def _proc_ancestor_shell() -> str | None:
    """Walk `/proc` upward looking for a shell (Linux; None anywhere else)."""
    proc = Path("/proc")
    if not proc.is_dir():
        return None
    pid = os.getppid()
    for _ in range(_ANCESTRY_MAX_DEPTH):
        if pid <= 1:
            return None
        entry = proc / str(pid)
        try:
            comm = entry.joinpath("comm").read_text(encoding="utf-8", errors="replace")
            stat = entry.joinpath("stat").read_text(encoding="utf-8", errors="replace")
        except OSError:
            return None
        kind = _shell_kind_from_exe(comm)
        if kind is not None:
            return kind
        # `stat` is "<pid> (<comm>) <state> <ppid> ..." and <comm> may itself
        # contain spaces and parentheses -- split after the LAST ")" so the
        # fixed-position fields line up regardless.
        try:
            pid = int(stat.rsplit(")", 1)[1].split()[1])
        except (IndexError, ValueError):
            return None
    return None


class _ProcessEntry32(ctypes.Structure):
    """Windows `PROCESSENTRY32` (ANSI), spelled with plain `ctypes` scalars.

    Deliberately avoids `ctypes.wintypes`, which does not exist off Windows --
    this module is imported unconditionally, including by the Linux CI job.
    """

    _fields_ = (
        ("dwSize", ctypes.c_ulong),
        ("cntUsage", ctypes.c_ulong),
        ("th32ProcessID", ctypes.c_ulong),
        ("th32DefaultHeapID", ctypes.POINTER(ctypes.c_ulong)),
        ("th32ModuleID", ctypes.c_ulong),
        ("cntThreads", ctypes.c_ulong),
        ("th32ParentProcessID", ctypes.c_ulong),
        ("pcPriClassBase", ctypes.c_long),
        ("dwFlags", ctypes.c_ulong),
        ("szExeFile", ctypes.c_char * 260),
    )


def _windows_process_table() -> dict[int, tuple[int, str]]:
    """`{pid: (ppid, exe_basename)}` via Toolhelp32; empty dict on any failure.

    Every step is guarded: this is a best-effort refinement of an already-
    working environment-variable heuristic, never something worth failing a
    launch over.
    """
    table: dict[int, tuple[int, str]] = {}
    try:
        # `windll` only exists on Windows -- and only exists to *typeshed* on
        # Windows, hence `unused-ignore`: the ignore is load-bearing when this
        # is checked on Linux (the CI job) and redundant when checked here.
        kernel32 = ctypes.windll.kernel32  # type: ignore[attr-defined,unused-ignore]
        create_snapshot = kernel32.CreateToolhelp32Snapshot
        # Without this the HANDLE return value is truncated to a C int, which
        # silently breaks the >2GB handle values a 64-bit process can see.
        create_snapshot.restype = ctypes.c_void_p
        snapshot = create_snapshot(0x00000002, 0)  # TH32CS_SNAPPROCESS
        if not snapshot or snapshot == ctypes.c_void_p(-1).value:
            return {}
        try:
            entry = _ProcessEntry32()
            entry.dwSize = ctypes.sizeof(_ProcessEntry32)
            ok = kernel32.Process32First(ctypes.c_void_p(snapshot), ctypes.byref(entry))
            while ok:
                name = bytes(entry.szExeFile).split(b"\0", 1)[0].decode("utf-8", "replace")
                table[int(entry.th32ProcessID)] = (int(entry.th32ParentProcessID), name)
                ok = kernel32.Process32Next(ctypes.c_void_p(snapshot), ctypes.byref(entry))
        finally:
            kernel32.CloseHandle(ctypes.c_void_p(snapshot))
    except (AttributeError, OSError, ValueError):
        return {}
    return table


def _windows_ancestor_shell() -> str | None:
    """The nearest shell among this process's Windows ancestors, or None."""
    table = _windows_process_table()
    if not table:
        return None
    pid = os.getppid()
    for _ in range(_ANCESTRY_MAX_DEPTH):
        found = table.get(pid)
        if found is None:
            return None
        ppid, name = found
        kind = _shell_kind_from_exe(name)
        if kind is not None:
            return kind
        if ppid == pid:
            return None
        pid = ppid
    return None


# ---- program resolution -----------------------------------------------------


def find_program(name: str) -> str | None:
    """Resolve `name` to a file the way the platform's own shells would.

    A value that already contains a path separator is taken as a path and
    only checked for existence. A bare name is searched in the current
    directory *then* `PATH` on Windows, and in `PATH` only on POSIX -- POSIX
    shells do not search the working directory, and quietly doing so here
    would be exactly the "silently running the wrong script" hazard this
    lookup order is documented to avoid. On Windows each `%PATHEXT%` suffix
    is tried after the literal name, so both `claude` and `my-claude.ps1`
    resolve.

    Returns None when nothing matches -- the caller should still hand the
    value to a shell in that case, since a shell function, alias, or builtin
    is not a file and cannot be found here.
    """
    if not name:
        return None
    separators = [s for s in (os.sep, os.altsep) if s]
    if any(s in name for s in separators):
        candidate = Path(name)
        return str(candidate) if candidate.is_file() else None

    directories: list[str] = []
    if is_windows():
        directories.append(os.getcwd())
    directories.extend(d for d in os.environ.get("PATH", "").split(os.pathsep) if d)

    suffixes = [""]
    if is_windows():
        pathext = os.environ.get("PATHEXT") or _DEFAULT_PATHEXT
        suffixes.extend(e for e in pathext.split(os.pathsep) if e)

    for directory in directories:
        for suffix in suffixes:
            candidate = Path(directory) / f"{name}{suffix}"
            if candidate.is_file():
                return str(candidate)
    return None


def is_directly_executable(path: str) -> bool:
    """Can the OS `exec` `path` itself, without a shell interpreting it?

    Windows answers by extension (CreateProcess launches `.exe`/`.com` and
    hands `.bat`/`.cmd` to cmd.exe, but knows nothing about `.ps1`); POSIX
    answers by the execute bit.
    """
    if is_windows():
        return Path(path).suffix.lower() in _WINDOWS_DIRECT_EXEC_SUFFIXES
    return os.access(path, os.X_OK)


# ---- command-line construction ----------------------------------------------


def quote_for_shell(shell: str, value: str) -> str:
    """Quote `value` as exactly one literal token for `shell`.

    PowerShell gets single quotes (`''`-doubled), which are fully literal --
    no `$var`/backtick expansion, and no double quotes for `subprocess`'s own
    argv-joining to have to escape on the way in. cmd.exe keeps the
    long-standing `"`-wrapping best effort from RAL-110, including its known
    gap: cmd expands `%VAR%` even inside a double-quoted token. POSIX shells
    use `shlex.quote`; fish gets its own escaping, since fish single-quotes
    take `\\'` rather than POSIX's `'\\''` splice.
    """
    if shell in _POWERSHELLS:
        return "'" + value.replace("'", "''") + "'"
    if shell == "cmd":
        if not value or any(c in value for c in ' \t"'):
            return '"' + value.replace('"', '""') + '"'
        return value
    if shell == "fish":
        return "'" + value.replace("\\", "\\\\").replace("'", "\\'") + "'"
    return shlex.quote(value)


def build_program_command_line(shell: str, program: str, args: list[str]) -> str:
    """A `shell` command line that runs `program` with `args`, all quoted.

    PowerShell needs the `&` call operator: a quoted string in command
    position is just a string literal to it, not something to execute.
    """
    tokens = [quote_for_shell(shell, program), *(quote_for_shell(shell, a) for a in args)]
    line = " ".join(tokens)
    return f"& {line}" if shell in _POWERSHELLS else line


def build_compound_command_line(shell: str, raw_command: str, args: list[str]) -> str:
    """Append `args` (quoted for `shell`) to an opaque `raw_command` shell line."""
    if not args:
        return raw_command
    tail = " ".join(quote_for_shell(shell, a) for a in args)
    return f"{raw_command} {tail}"


def shell_spawn_args(shell: str, command_line: str) -> tuple[str | list[str], bool]:
    """The `(args, use_shell)` pair to hand `subprocess.run` for `command_line`.

    Everything gets an explicit argv (`bash -c ...`, `powershell -Command
    ...`) except cmd.exe on Windows, which gets `shell=True` instead. That
    exception is not cosmetic: cmd.exe does not parse its command line by the
    MSVCRT argv rules `subprocess.list2cmdline` implements, so a token
    containing an embedded `"` (a temp-file path with a space, say) comes out
    escaped as `\\"` -- which cmd does not understand. `shell=True` on Windows
    is literally `%COMSPEC% /c <string>` with no requoting at all, which is
    precisely what is wanted.
    """
    if shell == "cmd" and is_windows():
        return command_line, True
    prefix = _SHELL_PREFIXES.get(shell) or _SHELL_PREFIXES["sh"]
    return [*prefix, command_line], False
