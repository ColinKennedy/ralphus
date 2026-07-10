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

rem Root is whichever checkout this script lives in (main or a worktree).
set "root=%~dp0.."
for %%I in ("%root%") do set "root=%%~fI"

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
rem    librarian exiting) tears the daemon down too. The daemon shares this
rem    console via `start /b`, so its logs interleave and Ctrl-C hits both; the
rem    taskkill afterward is a cleanup safety net (only run one daemon at a time
rem    -- port 7890 clashes otherwise).
set "RALPHUS_DAEMON_URL=http://127.0.0.1:7890"
echo == starting stack ==
echo    runner -^> %RALPHUS_RUNNER_CMD%
echo    daemon -^> %RALPHUS_DAEMON_URL%
start "" /b "%root%\target\debug\ralphus-daemon.exe" serve

"%root%\target\debug\ralphus-librarian.exe" serve

taskkill /f /im ralphus-daemon.exe >nul 2>&1
endlocal
