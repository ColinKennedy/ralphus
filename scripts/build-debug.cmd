@echo off
setlocal enabledelayedexpansion
rem build-debug.cmd -- the FAST counterpart to build-release.cmd (mirrors build-debug.sh).
rem Fast local dev loop -- NO PyInstaller, NO dist\. Runs the whole stack from
rem source so iterating on the GUI (librarian\assets\board.html) is quick:
rem
rem   * daemon + librarian   -> cargo debug builds (incremental; seconds)
rem   * runner               -> the venv script via uv (RALPHUS_RUNNER_CMD),
rem                             so a GUI/daemon edit NEVER rebuilds the heavy
rem                             standalone runner exe.
rem
rem Loop: edit board.html -> re-run this script -> refresh the browser.
rem Ctrl-C stops both processes. For a distributable standalone build (slow),
rem use build-release.cmd instead.
rem
rem Usage: build-debug.cmd [--daemon-port N] [--librarian-port N]
rem Defaults to 7890/7474. Pass different ports to run a second stack
rem alongside the regular one -- but both instances still share the same
rem SQLite DB (%USERPROFILE%\.ralphus\tasks.db) unless USERPROFILE is also
rem overridden, so this is for a second UI/API endpoint onto the same data,
rem not full isolation.
rem
rem Example (regular stack, defaults):     build-debug.cmd
rem Example (second stack, side-by-side):  build-debug.cmd --daemon-port 7891 --librarian-port 7475

rem Root is whichever checkout this script lives in (main or a worktree).
rem MUST be resolved before any `shift` below -- plain `shift` shifts %0 too,
rem so %~dp0 would stop pointing at this script once argument parsing shifts.
set "root=%~dp0.."
for %%I in ("%root%") do set "root=%%~fI"

set "daemon_port=7890"
set "librarian_port=7474"

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
echo unknown argument: %~1 1>&2
echo usage: build-debug.cmd [--daemon-port N] [--librarian-port N] 1>&2
exit /b 1
:args_done

rem 1. Runner: use the venv script (fast; no bundling). Sync the runner extra so
rem    native model agents work; this is a near-no-op once the venv is warm.
echo == syncing runner venv (uv) ==
pushd "%root%\cli"
uv sync --extra runner >nul
if errorlevel 1 (popd & exit /b 1)
popd
set "RALPHUS_RUNNER_CMD=%root%\cli\.venv\Scripts\ralphus-runner.exe"

rem 2. Build the Rust bins in debug (fast incremental rebuild picks up board.html).
echo == cargo build (debug) daemon + librarian ==
cargo build -p ralphus-daemon -p ralphus-librarian --manifest-path "%root%\Cargo.toml"
if errorlevel 1 exit /b 1

rem 3. Daemon in the background, librarian in the foreground. Ctrl-C (or the
rem    librarian exiting) tears the daemon down too. The daemon is launched via
rem    PowerShell's Start-Process -PassThru so we capture its exact PID -- the
rem    cleanup below kills only THAT process, not every ralphus-daemon.exe on
rem    the box, so a second side-by-side instance (different ports) survives.
set "RALPHUS_DAEMON_URL=http://127.0.0.1:%daemon_port%"
echo == starting stack ==
echo    runner    -^> %RALPHUS_RUNNER_CMD%
echo    daemon    -^> %RALPHUS_DAEMON_URL%
echo    librarian -^> http://127.0.0.1:%librarian_port%
for /f "delims=" %%P in ('powershell -NoProfile -Command "(Start-Process -FilePath '%root%\target\debug\ralphus-daemon.exe' -ArgumentList 'serve','--port','%daemon_port%' -PassThru -WindowStyle Hidden).Id"') do set "daemon_pid=%%P"

"%root%\target\debug\ralphus-librarian.exe" serve --port %librarian_port%

taskkill /f /pid %daemon_pid% >nul 2>&1
endlocal
