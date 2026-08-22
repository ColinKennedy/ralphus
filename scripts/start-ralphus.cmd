@echo off
setlocal enabledelayedexpansion
rem Launch a prebuilt ralphus bundle from its extracted root directory.
rem
rem Usage:
rem   start-ralphus.cmd [--daemon-port N] [--librarian-port N] [--db-path PATH] [--daemon-only]

set "root=%~dp0"
if "%root:~-1%"=="\" set "root=%root:~0,-1%"

set "daemon_port=7890"
set "librarian_port=7474"
set "db_path="
set "daemon_only=0"

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
if /i "%~1"=="--daemon-only" (
    set "daemon_only=1"
    shift
    goto parse_args
)
echo unknown argument: %~1 1>&2
echo usage: start-ralphus.cmd [--daemon-port N] [--librarian-port N] [--db-path PATH] [--daemon-only] 1>&2
exit /b 1
:args_done

if "%db_path%"=="" if not "%daemon_port%"=="7890" (
    set "db_path=%USERPROFILE%\.ralphus\tasks-%daemon_port%.db"
)

set "RALPHUS_RUNNER_CMD=%root%\bin\ralphus-runner.exe"
set "RALPHUS_TMUX_CMD=%root%\tmux\tmux.exe"
set "RALPHUS_DAEMON_URL=http://127.0.0.1:%daemon_port%"
set "daemon_exe=%root%\bin\ralphus-daemon.exe"
set "librarian_exe=%root%\bin\ralphus-librarian.exe"
set "hello_template=%root%\examples\hello.toml"
set "log_dir=%root%\logs"
set "daemon_stdout_log=%log_dir%\daemon.stdout.log"
set "daemon_stderr_log=%log_dir%\daemon.stderr.log"
set "librarian_log=%log_dir%\librarian.log"
set "daemon_pid_file=%log_dir%\daemon.pid"

for %%F in ("%daemon_exe%" "%librarian_exe%" "%RALPHUS_RUNNER_CMD%" "%RALPHUS_TMUX_CMD%") do (
    if not exist "%%~fF" (
        echo required file missing: %%~fF 1>&2
        exit /b 1
    )
)
if exist "%hello_template%" (
    powershell -NoProfile -Command ^
      "$path = '%hello_template%'; " ^
      "$root = '%root%'.Replace('\','/'); " ^
      "$text = Get-Content $path -Raw; " ^
      "$text = $text -replace '__RALPHUS_BUNDLE_ROOT__', $root; " ^
      "[System.IO.File]::WriteAllText($path, $text, [System.Text.UTF8Encoding]::new($false))"
)
if not exist "%log_dir%" mkdir "%log_dir%"
type nul > "%daemon_stdout_log%"
type nul > "%daemon_stderr_log%"
type nul > "%librarian_log%"
if exist "%daemon_pid_file%" del "%daemon_pid_file%"

echo == starting ralphus bundle ==
echo    runner    -^> %RALPHUS_RUNNER_CMD%
echo    tmux      -^> %RALPHUS_TMUX_CMD%
echo    daemon    -^> %RALPHUS_DAEMON_URL%
if "%db_path%"=="" (
    echo    db        -^> ^<default: %%USERPROFILE%%\.ralphus\tasks.db^>
) else (
    echo    db        -^> %db_path%
)
if "%daemon_only%"=="0" echo    librarian -^> http://127.0.0.1:%librarian_port%
echo    daemon out -^> %daemon_stdout_log%
echo    daemon err -^> %daemon_stderr_log%
if "%daemon_only%"=="0" echo    librarian log -^> %librarian_log%

rem RAL-?: the daemon's PID is written to a file rather than captured through
rem `for /f`'s stdout pipe. `Start-Process` here launches a long-running
rem detached process, which inherits the pipe's write handle -- as long as
rem that process keeps running, the pipe never sees EOF, so `for /f` blocks
rem forever even though Start-Process itself already returned and the daemon
rem is up. Writing the PID to a file sidesteps the pipe entirely.
set "ps_arglist=serve','--port','%daemon_port%"
if not "%db_path%"=="" set "ps_arglist=%ps_arglist%','--db','%db_path%"
powershell -NoProfile -Command "(Start-Process -FilePath '%daemon_exe%' -ArgumentList '%ps_arglist%' -WorkingDirectory '%root%' -RedirectStandardOutput '%daemon_stdout_log%' -RedirectStandardError '%daemon_stderr_log%' -PassThru -WindowStyle Hidden).Id | Out-File -Encoding ascii '%daemon_pid_file%'"
set "daemon_pid="
if exist "%daemon_pid_file%" set /p daemon_pid=<"%daemon_pid_file%"
if "%daemon_pid%"=="" (
    echo failed to start daemon 1>&2
    exit /b 1
)

powershell -NoProfile -Command ^
  "$deadline = (Get-Date).AddSeconds(30); " ^
  "$stateHome = $env:USERPROFILE; if (-not $stateHome) { $stateHome = $env:HOME }; if (-not $stateHome) { $stateHome = '.' }; " ^
  "$tokenPath = Join-Path (Join-Path $stateHome '.ralphus') 'daemon.token'; " ^
  "while ((Get-Date) -lt $deadline) { " ^
  "  try { " ^
  "    $token = if ($env:RALPHUS_DAEMON_TOKEN) { $env:RALPHUS_DAEMON_TOKEN.Trim() } elseif (Test-Path $tokenPath) { (Get-Content $tokenPath -Raw).Trim() } else { '' }; " ^
  "    if ($token) { Invoke-RestMethod -Uri '%RALPHUS_DAEMON_URL%/api/daemon' -Headers @{ Authorization = ('Bearer ' + $token) } -TimeoutSec 2 | Out-Null; exit 0 } " ^
  "  } catch { } " ^
  "  Start-Sleep -Milliseconds 250 " ^
  "} " ^
  "exit 1"
if errorlevel 1 (
    echo daemon did not become ready within 30 seconds 1>&2
    echo daemon stderr log: %daemon_stderr_log% 1>&2
    echo daemon stdout log: %daemon_stdout_log% 1>&2
    powershell -NoProfile -Command ^
      "if (Test-Path '%daemon_stderr_log%') { " ^
      "  [Console]::Error.WriteLine('--- daemon stderr tail ---'); " ^
      "  Get-Content '%daemon_stderr_log%' -Tail 40 | ForEach-Object { [Console]::Error.WriteLine($_) } " ^
      "} elseif (Test-Path '%daemon_stdout_log%') { " ^
      "  [Console]::Error.WriteLine('--- daemon stdout tail ---'); " ^
      "  Get-Content '%daemon_stdout_log%' -Tail 40 | ForEach-Object { [Console]::Error.WriteLine($_) } " ^
      "}"
    taskkill /f /pid %daemon_pid% >nul 2>&1
    exit /b 1
)

if "%daemon_only%"=="1" (
    echo daemon is ready on %RALPHUS_DAEMON_URL%
    exit /b 0
)

"%librarian_exe%" serve --port %librarian_port% 1>>"%librarian_log%" 2>&1
taskkill /f /pid %daemon_pid% >nul 2>&1
endlocal
