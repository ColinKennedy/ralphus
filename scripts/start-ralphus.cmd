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

set "RALPHUS_RUNNER_CMD=%root%\bin\ralphus-runner\ralphus-runner.exe"
set "RALPHUS_TMUX_CMD=%root%\tmux\tmux.exe"
set "RALPHUS_DAEMON_URL=http://127.0.0.1:%daemon_port%"
set "daemon_exe=%root%\bin\ralphus-daemon.exe"
set "librarian_exe=%root%\bin\ralphus-librarian.exe"

for %%F in ("%daemon_exe%" "%librarian_exe%" "%RALPHUS_RUNNER_CMD%" "%RALPHUS_TMUX_CMD%") do (
    if not exist "%%~fF" (
        echo required file missing: %%~fF 1>&2
        exit /b 1
    )
)

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

set "ps_arglist=serve','--port','%daemon_port%"
if not "%db_path%"=="" set "ps_arglist=%ps_arglist%','--db','%db_path%"
for /f "delims=" %%P in ('powershell -NoProfile -Command "(Start-Process -FilePath '%daemon_exe%' -ArgumentList '%ps_arglist%' -WorkingDirectory '%root%' -PassThru -WindowStyle Hidden).Id"') do set "daemon_pid=%%P"
if "%daemon_pid%"=="" (
    echo failed to start daemon 1>&2
    exit /b 1
)

powershell -NoProfile -Command ^
  "$deadline = (Get-Date).AddSeconds(30); " ^
  "while ((Get-Date) -lt $deadline) { " ^
  "  try { Invoke-RestMethod -Uri '%RALPHUS_DAEMON_URL%/api/daemon' -TimeoutSec 2 | Out-Null; exit 0 } catch { Start-Sleep -Milliseconds 250 } " ^
  "} " ^
  "exit 1"
if errorlevel 1 (
    echo daemon did not become ready within 30 seconds 1>&2
    taskkill /f /pid %daemon_pid% >nul 2>&1
    exit /b 1
)

if "%daemon_only%"=="1" (
    echo daemon is ready on %RALPHUS_DAEMON_URL%
    exit /b 0
)

"%librarian_exe%" serve --port %librarian_port%
taskkill /f /pid %daemon_pid% >nul 2>&1
endlocal
