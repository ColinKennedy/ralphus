@echo off
setlocal enabledelayedexpansion
rem build-debug.cmd -- the FAST counterpart to build-release.cmd (mirrors build-debug.sh).
rem Fast local dev loop -- NO dist\. Runs the whole stack from source so
rem iterating on the GUI (librarian\assets's board) is quick:
rem
rem   * daemon + librarian + runner + cli   -> cargo debug builds (incremental;
rem                                            seconds each; all four are Rust)
rem
rem All four binaries are Rust -- there is no Python venv sync step. `cli-py\`
rem still exists for `docsgen\` (Playwright screenshots, dev-only, never
rem shipped).
rem
rem Loop: edit a board asset (librarian\assets\board\*.js, board.css,
rem board.html) and refresh the browser -- the librarian reads board assets from
rem disk in dev mode (RALPHUS_BOARD_ASSETS_DIR is set below), so no rebuild is
rem needed. Re-run this script only when Rust code changes.
rem Ctrl-C stops both processes. For a distributable standalone build (slow),
rem use build-release.cmd instead.
rem
rem Usage: build-debug.cmd [--daemon-port N] [--librarian-port N] [--db-path PATH] [--webhook-tunnel]
rem Defaults to 7890/7474. Pass different ports to run a second stack
rem alongside the regular one. RAL-164: --daemon-port + --db-path together
rem give FULL isolation (separate port AND separate SQLite DB) -- the right
rem way to keep your regular ralphus instance open while testing ralphus in
rem another git worktree. Without --db-path, a non-default --daemon-port
rem still gets its own DB automatically (derived as
rem %USERPROFILE%\.ralphus\tasks-<port>.db); the *default* port keeps using
rem the plain %USERPROFILE%\.ralphus\tasks.db it always has, so existing
rem setups are unaffected. Ports/paths are never silently invented beyond
rem this per-port default -- write down whatever you pass so a later
rem `ralphus-daemon stop --port N` targets the right instance.
rem
rem --webhook-tunnel starts an ngrok tunnel to --daemon-port (ngrok must
rem already be on PATH -- https://ngrok.com/download; it's a dev-only tool,
rem never a project dependency) and exports RALPHUS_DAEMON_PUBLIC_URL to
rem whatever public HTTPS URL ngrok hands back *this run* before the daemon
rem starts -- see scripts/webhook-tunnel.ps1 and config.rs's
rem load_daemon_config_with. Solves the chicken-and-egg of "the daemon
rem needs public_url at startup, but a free-tier ngrok URL is only known
rem once ngrok itself starts, and changes every run" without ever touching
rem .ralphus.toml by hand. The tunnel is torn down alongside the daemon when
rem this script exits.
rem
rem Example (regular stack, defaults):      build-debug.cmd
rem Example (second stack, fully isolated): build-debug.cmd --daemon-port 7891 --librarian-port 7475 --db-path %USERPROFILE%\.ralphus\tasks-worktree2.db
rem Example (webhook dev, real deliveries):  build-debug.cmd --webhook-tunnel

rem Root is whichever checkout this script lives in (main or a worktree).
rem MUST be resolved before any `shift` below -- plain `shift` shifts %0 too,
rem so %~dp0 would stop pointing at this script once argument parsing shifts.
set "root=%~dp0.."
for %%I in ("%root%") do set "root=%%~fI"

set "daemon_port=7890"
set "librarian_port=7474"
set "db_path="
set "webhook_tunnel=0"

:parse_args
if "%~1"=="" goto args_done
if /i "%~1"=="--daemon-port" (
    set "daemon_port=%~2"
    shift
    shift
    goto parse_args
)
if /i "%~1"=="--librarian-port" (
    set "librarian_port=%~2"
    shift
    shift
    goto parse_args
)
if /i "%~1"=="--db-path" (
    set "db_path=%~2"
    shift
    shift
    goto parse_args
)
if /i "%~1"=="--webhook-tunnel" (
    set "webhook_tunnel=1"
    shift
    goto parse_args
)
echo unknown argument: %~1 1>&2
echo usage: build-debug.cmd [--daemon-port N] [--librarian-port N] [--db-path PATH] [--webhook-tunnel] 1>&2
exit /b 1
:args_done

rem Only auto-derive a per-port DB when the daemon port was actually changed
rem from the default -- the default port keeps its existing DB path untouched.
if "%db_path%"=="" if not "%daemon_port%"=="7890" (
    set "db_path=%USERPROFILE%\.ralphus\tasks-%daemon_port%.db"
)

rem 1. Build all four Rust bins in debug (fast incremental rebuild picks up
rem    any CLI/runner/librarian source edit alike).
echo == cargo build (debug) daemon + librarian + runner + cli ==
cargo build --package ralphus-daemon --package ralphus-librarian --package ralphus-runner --package ralphus-cli --manifest-path "%root%\Cargo.toml"
if errorlevel 1 exit /b 1

rem 2. Point RALPHUS_RUNNER_CMD at the just-built debug runner exe.
set "RALPHUS_RUNNER_CMD=%root%\target\debug\ralphus-runner.exe"

rem 3. Daemon in the background, librarian in the foreground. Ctrl-C (or the
rem    librarian exiting) tears the daemon down too. The daemon is launched via
rem    PowerShell's Start-Process -PassThru so we capture its exact PID -- the
rem    cleanup below kills only THAT process, not every ralphus-daemon.exe on
rem    the box, so a second side-by-side instance (different ports) survives.
rem    -NoNewWindow (not -WindowStyle Hidden) is required here: a hidden
rem    window still gets its own console, which puts the daemon in a separate
rem    console process group that never sees a Ctrl-C typed into this window --
rem    only the foreground librarian.exe would die, and the taskkill below
rem    never runs because Ctrl-C also triggers cmd's own "Terminate batch job
rem    (Y/N)?" prompt, which aborts the rest of this script if answered Y.
rem    -NoNewWindow keeps the daemon in this window's console/process group so
rem    Ctrl-C kills it directly, independent of that prompt.
rem
rem    The PID comes back through a temp FILE, never through `for /f`. `for /f`
rem    reads its child command's stdout over a pipe and keeps reading until
rem    that pipe reports EOF -- and -NoNewWindow hands the daemon that very
rem    same stdout handle, so the daemon holds the write end open for its
rem    entire (unbounded) lifetime. The `for /f` would then never return, the
rem    librarian below would never start, and the board would never come up.
rem    -RedirectStandardOutput/-RedirectStandardError are absent for the same
rem    reason; the daemon already writes everything to its configured log_path
rem    (see ~/.config/ralphus/config.toml [daemon].log_path).
rem --webhook-tunnel: start ngrok and capture its (fresh-every-run) public
rem HTTPS URL into RALPHUS_DAEMON_PUBLIC_URL before the daemon starts, so
rem config.rs's load_daemon_config picks it up as this run's [daemon]
rem public_url override -- no .ralphus.toml edit, ever. Delegated to a real
rem .ps1 (not inlined here) because it needs a retry loop against ngrok's
rem local status API, which is unwieldy to express correctly in batch.
set "ngrok_pid="
if "%webhook_tunnel%"=="1" (
    echo == starting ngrok tunnel to port %daemon_port% ==
    set "ngrok_pid_file=%TEMP%\ralphus-debug-ngrok-%daemon_port%.pid"
    set "ngrok_url_file=%TEMP%\ralphus-debug-ngrok-%daemon_port%.url"
    del "!ngrok_pid_file!" >nul 2>&1
    del "!ngrok_url_file!" >nul 2>&1
    powershell -NoProfile -File "%root%\scripts\webhook-tunnel.ps1" -Port %daemon_port% -PidFile "!ngrok_pid_file!" -UrlFile "!ngrok_url_file!"
    if errorlevel 1 exit /b 1
    if exist "!ngrok_pid_file!" set /p ngrok_pid=<"!ngrok_pid_file!"
    del "!ngrok_pid_file!" >nul 2>&1
    set "RALPHUS_DAEMON_PUBLIC_URL="
    if exist "!ngrok_url_file!" set /p RALPHUS_DAEMON_PUBLIC_URL=<"!ngrok_url_file!"
    del "!ngrok_url_file!" >nul 2>&1
    if not defined RALPHUS_DAEMON_PUBLIC_URL (
        echo failed to establish an ngrok tunnel 1>&2
        exit /b 1
    )
    echo    webhook tunnel -^> !RALPHUS_DAEMON_PUBLIC_URL! ^(ngrok pid !ngrok_pid!^)
)

set "RALPHUS_DAEMON_URL=http://127.0.0.1:%daemon_port%"
echo == starting stack ==
echo    runner    -^> %RALPHUS_RUNNER_CMD%
echo    cli       -^> %root%\target\debug\ralphus.exe (not started; run it yourself, e.g. "ralphus status")
echo    daemon    -^> %RALPHUS_DAEMON_URL%
if "%db_path%"=="" (
    echo    db        -^> ^<default: %%USERPROFILE%%\.ralphus\tasks.db^>
) else (
    echo    db        -^> %db_path%
)
echo    librarian -^> http://127.0.0.1:%librarian_port%
echo    board dev mode -^> reading librarian\assets from disk (RALPHUS_BOARD_ASSETS_DIR); edits are live on browser refresh
set "ps_arglist=serve','--port','%daemon_port%"
if not "%db_path%"=="" set "ps_arglist=%ps_arglist%','--db','%db_path%"
set "daemon_pid_file=%TEMP%\ralphus-debug-daemon-%daemon_port%.pid"
del "%daemon_pid_file%" >nul 2>&1
powershell -NoProfile -Command "(Start-Process -FilePath '%root%\target\debug\ralphus-daemon.exe' -ArgumentList '%ps_arglist%' -PassThru -NoNewWindow).Id | Set-Content -Encoding ascii -Path '%daemon_pid_file%'"
set "daemon_pid="
if exist "%daemon_pid_file%" set /p daemon_pid=<"%daemon_pid_file%"
del "%daemon_pid_file%" >nul 2>&1
if not defined daemon_pid (
    echo failed to start the daemon: no pid captured 1>&2
    exit /b 1
)

set "RALPHUS_BOARD_ASSETS_DIR=%root%\librarian\assets"
"%root%\target\debug\ralphus-librarian.exe" serve --port %librarian_port%

rem Reaching here means the librarian exited -- the daemon is killed next, so
rem say so out loud: a silent teardown here has previously masqueraded as
rem "the daemon crashes after ~2 minutes".
echo == librarian exited (code %errorlevel%); stopping daemon pid %daemon_pid% ==
taskkill /f /pid %daemon_pid% >nul 2>&1
if defined ngrok_pid (
    echo == stopping ngrok tunnel pid %ngrok_pid% ==
    taskkill /f /pid %ngrok_pid% >nul 2>&1
)
endlocal
