@echo off
setlocal enabledelayedexpansion
rem live-stack.cmd -- a private ralphus stack for live-testing this checkout
rem (mirrors live-stack.sh; use this one on Windows).
rem
rem Builds the daemon, librarian, runner and CLI from the checkout this script
rem lives in, then starts a daemon and a board on free ports with their own
rem SQLite database, so nothing the host's regular stack (7890/7474,
rem %USERPROFILE%\.ralphus\tasks.db) is doing is touched. Safe to run from
rem inside a ralphus cell: the daemon's startup recovery only resets rows in its
rem own database, and it binds its port before doing so.
rem
rem Usage:
rem   live-stack.cmd start [--name NAME]   build, start, print the URLs
rem   live-stack.cmd stop  [--name NAME]   stop exactly the pids `start` recorded
rem   live-stack.cmd env   [--name NAME]   print the recorded settings (KEY=value)
rem   live-stack.cmd plan  [--name NAME]   print the ports/paths `start` would use
rem
rem NAME defaults to this checkout's directory name, so each worktree gets its
rem own stack and `stop` never has to guess which one is meant.
rem
rem State lives in %USERPROFILE%\.ralphus\live-stacks\<name>\ (stack.env,
rem tasks.db) in the same KEY=value format live-stack.sh writes, so either script
rem can stop a stack the other started. Drive the stack with
rem `target\debug\ralphus --daemon-url <DAEMON_URL> ...` and open <LIBRARIAN_URL>
rem in a browser. Stop it when finished.
rem
rem The daemon and board are launched with Start-Process -WindowStyle Hidden so
rem they outlive this script and do NOT inherit its stdout: a caller capturing
rem this script's output (an agent's shell tool, a `for /f`) would otherwise wait
rem on the daemon's lifetime and hang. PIDs come back through a temp FILE for the
rem same reason build-debug.cmd does it that way. Stopping is by recorded pid,
rem never by program name.

rem Root is whichever checkout this script lives in (main or a worktree).
rem MUST be resolved before any `shift` below -- plain `shift` shifts %0 too.
set "root=%~dp0.."
for %%I in ("%root%") do set "root=%%~fI"

if "%~1"=="" goto usage
set "action=%~1"
shift
for %%I in ("%root%") do set "name=%%~nxI"

:parse_args
if "%~1"=="" goto args_done
if /i "%~1"=="--name" (
    if "%~2"=="" goto usage
    set "name=%~2"
    shift
    shift
    goto parse_args
)
goto usage
:args_done

rem Keep the name path-safe (and identical to what live-stack.sh derives).
set "LIVE_STACK_NAME=%name%"
for /f "delims=" %%N in ('powershell -NoProfile -Command "$env:LIVE_STACK_NAME -replace '[^A-Za-z0-9_-]','_'"') do set "name=%%N"

set "stack_dir=%USERPROFILE%\.ralphus\live-stacks\%name%"
set "env_file=%stack_dir%\stack.env"
set "db_path=%stack_dir%\tasks.db"

if /i "%action%"=="plan" goto do_plan
if /i "%action%"=="env" goto do_env
if /i "%action%"=="start" goto do_start
if /i "%action%"=="stop" goto do_stop
goto usage

:usage
echo usage: live-stack.cmd {start^|stop^|env^|plan} [--name NAME] 1>&2
exit /b 1

:do_plan
call :free_port 17890 daemon_port
call :free_port 17474 librarian_port
echo NAME=%name%
echo STACK_DIR=%stack_dir%
echo DB=%db_path%
echo DAEMON_PORT=%daemon_port%
echo LIBRARIAN_PORT=%librarian_port%
exit /b 0

:do_env
if not exist "%env_file%" (
    echo live-stack: no stack recorded for '%name%' 1>&2
    exit /b 1
)
type "%env_file%"
exit /b 0

:do_start
rem A db path with a space would be split into two arguments by Start-Process.
if not "%db_path%"=="%db_path: =%" (
    echo live-stack: %db_path% contains a space; set USERPROFILE to a path without one 1>&2
    exit /b 1
)

call :recorded DAEMON_PID old_daemon_pid
if defined old_daemon_pid (
    call :pid_alive !old_daemon_pid!
    if not errorlevel 1 (
        echo live-stack: '%name%' is already running 1>&2
        type "%env_file%"
        exit /b 0
    )
)

echo == cargo build (debug) daemon + librarian + runner + cli == 1>&2
cargo build --package ralphus-daemon --package ralphus-librarian --package ralphus-runner --package ralphus-cli --features ralphus-daemon/shell-syntax --manifest-path "%root%\Cargo.toml" 1>&2
if errorlevel 1 exit /b 1

if not exist "%stack_dir%" mkdir "%stack_dir%"
call :free_port 17890 daemon_port
call :free_port 17474 librarian_port
set "daemon_url=http://127.0.0.1:%daemon_port%"
set "librarian_url=http://127.0.0.1:%librarian_port%"

set "RALPHUS_RUNNER_CMD=%root%\target\debug\ralphus-runner.exe"
set "RALPHUS_DAEMON_URL=%daemon_url%"

call :launch "%root%\target\debug\ralphus-daemon.exe" "serve','--port','%daemon_port%','--db','%db_path%" daemon_pid
if not defined daemon_pid (
    echo live-stack: failed to start the daemon: no pid captured 1>&2
    exit /b 1
)
call :wait_for_port %daemon_port%
if errorlevel 1 (
    echo live-stack: the daemon did not come up on port %daemon_port% within 60s 1>&2
    taskkill /f /pid %daemon_pid% >nul 2>&1
    exit /b 1
)

call :launch "%root%\target\debug\ralphus-librarian.exe" "serve','--port','%librarian_port%" librarian_pid
if not defined librarian_pid (
    echo live-stack: failed to start the librarian: no pid captured 1>&2
    taskkill /f /pid %daemon_pid% >nul 2>&1
    exit /b 1
)
call :wait_for_port %librarian_port%
if errorlevel 1 (
    echo live-stack: the librarian did not come up on port %librarian_port% within 60s 1>&2
    taskkill /f /pid %librarian_pid% >nul 2>&1
    taskkill /f /pid %daemon_pid% >nul 2>&1
    exit /b 1
)

rem Redirect first (`>>file echo ...`): `echo X=1>file` would read "1>" as a handle.
>"%env_file%" echo NAME=%name%
>>"%env_file%" echo DAEMON_URL=%daemon_url%
>>"%env_file%" echo LIBRARIAN_URL=%librarian_url%
>>"%env_file%" echo DAEMON_PORT=%daemon_port%
>>"%env_file%" echo LIBRARIAN_PORT=%librarian_port%
>>"%env_file%" echo DAEMON_PID=%daemon_pid%
>>"%env_file%" echo LIBRARIAN_PID=%librarian_pid%
>>"%env_file%" echo DB=%db_path%
>>"%env_file%" echo CLI=%root%\target\debug\ralphus.exe
type "%env_file%"
echo == live stack '%name%' is up; stop it with: scripts\live-stack.cmd stop --name %name% == 1>&2
exit /b 0

:do_stop
if not exist "%env_file%" (
    echo live-stack: no stack recorded for '%name%' 1>&2
    exit /b 0
)
call :recorded LIBRARIAN_PID librarian_pid
call :recorded DAEMON_PID daemon_pid
if defined librarian_pid taskkill /f /pid !librarian_pid! >nul 2>&1
if defined daemon_pid taskkill /f /pid !daemon_pid! >nul 2>&1
del "%env_file%" >nul 2>&1
echo live-stack: stopped '%name%' 1>&2
exit /b 0

rem :free_port <start> <var> -- first free port at or above <start>, into <var>.
rem The host stack's ports (7890/7474) are never offered.
:free_port
for /f "delims=" %%P in ('powershell -NoProfile -Command "$l=[Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners() | Select-Object -ExpandProperty Port; $p=%~1; while($l -contains $p -or $p -eq 7890 -or $p -eq 7474){$p++}; $p"') do set "%~2=%%P"
exit /b 0

rem :recorded <KEY> <var> -- the value `start` wrote for KEY, into <var> (unset when none).
:recorded
set "%~2="
if not exist "%env_file%" exit /b 0
for /f "usebackq tokens=1,* delims==" %%A in ("%env_file%") do if "%%A"=="%~1" set "%~2=%%B"
exit /b 0

rem :pid_alive <pid> -- errorlevel 0 when the process exists.
:pid_alive
powershell -NoProfile -Command "if (Get-Process -Id %~1 -ErrorAction SilentlyContinue) { exit 0 } else { exit 1 }" >nul 2>&1
exit /b %errorlevel%

rem :wait_for_port <port> -- errorlevel 0 once something listens, 1 after 60s.
:wait_for_port
powershell -NoProfile -Command "$e=(Get-Date).AddSeconds(60); while ((Get-Date) -lt $e) { try { $c=New-Object Net.Sockets.TcpClient; $c.Connect('127.0.0.1',%~1); $c.Close(); exit 0 } catch { Start-Sleep -Milliseconds 500 } }; exit 1" >nul 2>&1
exit /b %errorlevel%

rem :launch <exe> <arglist> <var> -- start detached, pid into <var>. <arglist> is the
rem body of a PowerShell array literal ("serve','--port','N").
:launch
set "%~3="
set "launch_pid_file=%TEMP%\ralphus-live-stack-%name%-%RANDOM%.pid"
del "%launch_pid_file%" >nul 2>&1
powershell -NoProfile -Command "(Start-Process -FilePath '%~1' -ArgumentList '%~2' -PassThru -WindowStyle Hidden).Id | Set-Content -Encoding ascii -Path '%launch_pid_file%'"
if exist "%launch_pid_file%" set /p %~3=<"%launch_pid_file%"
del "%launch_pid_file%" >nul 2>&1
exit /b 0
