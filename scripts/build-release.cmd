@echo off
setlocal enabledelayedexpansion
rem Build the five ralphus executables into .\dist as copyable standalone
rem binaries -- all five are Rust:
rem
rem   daemon    -> dist\ralphus-daemon.exe
rem   librarian -> dist\ralphus-librarian.exe
rem   CLI       -> dist\ralphus.exe
rem   runner    -> dist\ralphus-runner.exe
rem   SSH       -> dist\ralphus-ssh-provider.exe
rem
rem All five need no adjacent interpreter/runtime directory; SQLite is linked
rem into the binaries that use it (rusqlite's bundled feature). A plain
rem cargo build --release produces one executable per binary, no _internal\ directory to
rem keep each exe beside. `cli-py\` still exists for `docsgen\` (Playwright
rem screenshots, dev-only, never shipped) -- see AGENTS.md.
rem
rem By default this also builds the vendored psmux (RAL-347, vendor/psmux git
rem submodule) and links it into ralphus-daemon.exe via the `embedded-tmux`
rem feature, so a release build works out of the box without a separate tmux
rem install. Set RALPHUS_SKIP_VENDORED_TMUX=1 to skip this (e.g. no network
rem access to build the submodule, or you intentionally always point
rem RALPHUS_TMUX_CMD at your own binary) and build without embedded-tmux.

set "root=%~dp0.."
for %%I in ("%root%") do set "root=%%~fI"
set "dist=%root%\dist"
if not exist "%dist%" mkdir "%dist%"

rem Remove stale PyInstaller/onefile artifacts from a previous build so a
rem leftover dist\ralphus\ (dir) or dist\ralphus.exe (old onefile) never
rem gets shipped alongside the new plain exe. `ralphus-attach` is cleaned up
rem here too (RAL-288's relay design, superseded and removed) even though
rem nothing below builds it anymore, so a stale one never lingers.
if exist "%dist%\ralphus" rmdir /s /q "%dist%\ralphus"
if exist "%dist%\ralphus-runner" rmdir /s /q "%dist%\ralphus-runner"
for %%B in (ralphus ralphus-runner ralphus-daemon ralphus-librarian ralphus-ssh-provider ralphus-attach) do (
  if exist "%dist%\%%B.exe" del /f /q "%dist%\%%B.exe"
)

set "daemon_features="
if "%RALPHUS_SKIP_VENDORED_TMUX%"=="1" (
  echo == RALPHUS_SKIP_VENDORED_TMUX=1: skipping vendored psmux build ==
) else (
  echo == building vendored psmux ^(vendor/psmux submodule^) ==
  powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build-vendored-tmux.ps1"
  if errorlevel 1 exit /b 1
  set "daemon_features=--features ralphus-daemon/embedded-tmux"
)

echo == building Rust executables (release) ==
cargo build --release --package ralphus-daemon --package ralphus-librarian --package ralphus-cli --package ralphus-runner --package ralphus-ssh-provider --manifest-path "%root%\Cargo.toml" !daemon_features!
if errorlevel 1 exit /b 1

for %%B in (ralphus-daemon ralphus-librarian ralphus ralphus-runner ralphus-ssh-provider) do (
  if exist "%root%\target\release\%%B.exe" copy /y "%root%\target\release\%%B.exe" "%dist%\" >nul
)

echo == done; artifacts in %dist% ==
dir "%dist%"
echo.
echo Layout (each binary is a standalone exe -- no sibling directory needed):
echo   %dist%\ralphus-daemon.exe
echo   %dist%\ralphus-librarian.exe
echo   %dist%\ralphus.exe
echo   %dist%\ralphus-runner.exe
echo   %dist%\ralphus-ssh-provider.exe
echo.
echo Put %dist% on PATH to get the `ralphus` CLI and have RALPHUS_RUNNER_CMD
echo resolve `ralphus-runner` automatically; or point RALPHUS_RUNNER_CMD at
echo the full path to %dist%\ralphus-runner.exe explicitly.
