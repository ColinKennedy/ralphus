@echo off
setlocal enabledelayedexpansion
rem Build the four ralphus executables into .\dist as copyable standalone
rem binaries -- all four are Rust:
rem
rem   daemon    -> dist\ralphus-daemon.exe
rem   librarian -> dist\ralphus-librarian.exe
rem   CLI       -> dist\ralphus.exe
rem   runner    -> dist\ralphus-runner.exe
rem
rem All four link SQLite in where needed (rusqlite `bundled`) and need no
rem system libraries or bundled interpreter -- a plain `cargo build --release`
rem produces one self-contained exe per binary, no _internal\ directory to
rem keep each exe beside. `cli\` still exists for `docsgen\` (Playwright
rem screenshots, dev-only, never shipped) -- see AGENTS.md.

set "root=%~dp0.."
for %%I in ("%root%") do set "root=%%~fI"
set "dist=%root%\dist"
if not exist "%dist%" mkdir "%dist%"

rem Remove stale PyInstaller/onefile artifacts from a previous build so a
rem leftover dist\ralphus\ (dir) or dist\ralphus.exe (old onefile) never
rem gets shipped alongside the new plain exe.
if exist "%dist%\ralphus" rmdir /s /q "%dist%\ralphus"
if exist "%dist%\ralphus-runner" rmdir /s /q "%dist%\ralphus-runner"
for %%B in (ralphus ralphus-runner ralphus-daemon ralphus-librarian) do (
  if exist "%dist%\%%B.exe" del /f /q "%dist%\%%B.exe"
)

echo == building Rust executables (release) ==
cargo build --release -p ralphus-daemon -p ralphus-librarian -p ralphus-cli -p ralphus-runner --manifest-path "%root%\Cargo.toml"
if errorlevel 1 exit /b 1

for %%B in (ralphus-daemon ralphus-librarian ralphus ralphus-runner) do (
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
echo.
echo Put %dist% on PATH to get the `ralphus` CLI and have RALPHUS_RUNNER_CMD
echo resolve `ralphus-runner` automatically; or point RALPHUS_RUNNER_CMD at
echo the full path to %dist%\ralphus-runner.exe explicitly.
