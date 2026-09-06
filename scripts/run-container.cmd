@echo off
setlocal enabledelayedexpansion
rem run-container.cmd -- Windows counterpart to run-container.sh. Selects the
rem container execution mode (RAL-225): the whole daemon+librarian+runner
rem stack, and every locally-executed agent subprocess, runs inside one
rem hardened container instead of as bare host subprocesses. See
rem docs\container-mode.md before relying on this for real isolation.
rem
rem Usage:
rem   set RALPHUS_WORKSPACE_ROOT=C:\path\to\your\checkouts
rem   run-container.cmd [--daemon-port N] [--librarian-port N]
rem
rem RALPHUS_WORKSPACE_ROOT is the ONLY host directory the container can read
rem or write -- point it at a parent directory of whatever project checkouts
rem your task files' `cwd`s live under, and use container-side paths (e.g.
rem /workspaces/my-project) in those task files when submitting against this
rem instance.

if "%RALPHUS_WORKSPACE_ROOT%"=="" (
    echo usage: set RALPHUS_WORKSPACE_ROOT=C:\path\to\your\checkouts ^&^& run-container.cmd 1>&2
    exit /b 1
)

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
echo usage: run-container.cmd [--daemon-port N] [--librarian-port N] 1>&2
exit /b 1
:args_done

set "RALPHUS_DAEMON_PORT=%daemon_port%"
set "RALPHUS_LIBRARIAN_PORT=%librarian_port%"

echo == ralphus container mode ==
echo    workspace root -^> %RALPHUS_WORKSPACE_ROOT% (mounted read-write at /workspaces in the container)
echo    daemon         -^> http://127.0.0.1:%daemon_port%
echo    librarian      -^> http://127.0.0.1:%librarian_port%

docker compose --file "%root%\docker\docker-compose.yml" up --build
endlocal
