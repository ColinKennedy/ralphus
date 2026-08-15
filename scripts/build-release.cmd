@echo off
setlocal enabledelayedexpansion
rem Build the four ralphus executables into .\dist as copyable standalone binaries.
rem
rem   daemon    (Rust)   -> dist\ralphus-daemon.exe
rem   librarian (Rust)   -> dist\ralphus-librarian.exe
rem   CLI       (Python) -> dist\ralphus\ralphus.exe               (one-DIR, PyInstaller)
rem   runner    (Python) -> dist\ralphus-runner\ralphus-runner.exe (one-DIR, PyInstaller)
rem
rem The Rust binaries link SQLite in (rusqlite `bundled`) so they need no system
rem libraries. The Python CLI and runner are each bundled with their interpreter
rem by PyInstaller. Both are built with the `runner` extra (pulls in
rem pydantic-ai): the runner needs it to run native model-agent sessions, and
rem the CLI needs it too since `ralphus author` drives pydantic-ai in-process
rem (it is not delegated to the runner subprocess).
rem
rem ---------------------------------------------------------------------------
rem WHY --onedir AND NOT --onefile
rem ---------------------------------------------------------------------------
rem A --onefile exe is a self-extracting archive: at launch its bootloader
rem unpacks the interpreter + site-packages to %TEMP%\_MEIxxxxxx, sets
rem _MEIPASS2, re-executes itself as a child, and the child validates that
rem cache before running. That validation can fail with
rem
rem   [PYI-XXXXX:ERROR] Security validation failure: parent process has
rem   different executable!
rem
rem which was reproduced on two machines when ralphus.exe was launched from a
rem sandboxed/reparenting tool layer (see PERMISSIONS_ISSUE.local.md). The Rust
rem binaries in the same directory, from the same zip, never showed it -- they
rem have no bootloader, no extraction, and no parent-process check.
rem
rem --onedir removes that whole mechanism: the payload sits next to the exe in
rem _internal\, nothing is extracted to %TEMP%, no _MEIPASS2, no parent check.
rem It also starts faster and stops writing ~45 MB to disk on every cold run.
rem
rem COST: each app gets its OWN directory with its OWN copy of the interpreter,
rem so dist\ is larger than the two onefile exes were, and the exes are no
rem longer directly at dist\ralphus.exe / dist\ralphus-runner.exe.
rem
rem DEPLOYING: copy the whole dist\ tree. Each Python exe must stay next to its
rem sibling _internal\ directory -- moving the .exe out on its own breaks it.
rem   * put dist\ralphus on PATH to get the `ralphus` CLI
rem   * point RALPHUS_RUNNER_CMD at the full path to
rem     dist\ralphus-runner\ralphus-runner.exe (or put that dir on PATH too)
rem   * dist\ralphus-daemon.exe and dist\ralphus-librarian.exe are standalone

set "root=%~dp0.."
for %%I in ("%root%") do set "root=%%~fI"
set "dist=%root%\dist"
if not exist "%dist%" mkdir "%dist%"

rem Remove stale --onefile artifacts from a previous build. Without this, an
rem old dist\ralphus.exe survives beside the new dist\ralphus\ directory and
rem gets shipped in the zip -- and it is exactly the binary whose bootloader
rem check this build switched away from.
for %%B in (ralphus ralphus-runner) do (
  if exist "%dist%\%%B.exe" (
    echo == removing stale onefile artifact dist\%%B.exe ==
    del /f /q "%dist%\%%B.exe"
  )
)

echo == building Rust executables (release) ==
cargo build --release -p ralphus-daemon -p ralphus-librarian --manifest-path "%root%\Cargo.toml"
if errorlevel 1 exit /b 1

for %%B in (ralphus-daemon ralphus-librarian) do (
  if exist "%root%\target\release\%%B.exe" copy /y "%root%\target\release\%%B.exe" "%dist%\" >nul
)

echo == syncing the 'runner' extra (pydantic-ai; needed by both exes below) ==
pushd "%root%\cli"
uv sync --extra runner >nul
if errorlevel 1 (popd & exit /b 1)

rem pydantic-ai's dependency tree (genai-prices, pydantic-ai-slim, logfire,
rem the anthropic/openai/mcp clients, ...) reads its own package metadata via
rem importlib.metadata at import time; PyInstaller does not bundle that
rem metadata unless told to, so the bundled build crashes with
rem `PackageNotFoundError` the first time `ralphus author` (or the runner)
rem actually imports pydantic-ai. --copy-metadata pulls each package's
rem dist-info into the bundle so those lookups succeed. (This applies to
rem --onedir exactly as it did to --onefile; it is about what gets collected,
rem not about how it is packaged.)
set "PYD_METADATA=--copy-metadata genai-prices --copy-metadata pydantic-ai --copy-metadata pydantic-ai-slim --copy-metadata pydantic-graph --copy-metadata pydantic-evals --copy-metadata pydantic --copy-metadata pydantic_core --copy-metadata pydantic-settings --copy-metadata logfire --copy-metadata logfire-api --copy-metadata anthropic --copy-metadata openai --copy-metadata mcp --copy-metadata httpx --copy-metadata google-genai --copy-metadata opentelemetry-api --copy-metadata opentelemetry-sdk --copy-metadata tenacity"

echo == building Python CLI (one-dir) ==
rem Build from the project venv (editable install) so PyInstaller bundles the
rem current source; a fresh `uvx --with .` env can serve a cached wheel instead.
uv run --extra runner --with pyinstaller ^
  pyinstaller --onedir --clean --name ralphus ^
  --distpath "%dist%" --workpath "%root%\target\pyinstaller" --specpath "%root%\target\pyinstaller" ^
  --version-file "%root%\scripts\version_info_cli.txt" ^
  %PYD_METADATA% ^
  "%root%\scripts\ralphus_entry.py"
if errorlevel 1 (popd & exit /b 1)

echo == building Python runner (one-dir, with the 'runner' extra) ==
uv run --extra runner --with pyinstaller ^
  pyinstaller --onedir --clean --name ralphus-runner ^
  --distpath "%dist%" --workpath "%root%\target\pyinstaller" --specpath "%root%\target\pyinstaller" ^
  --version-file "%root%\scripts\version_info_runner.txt" ^
  %PYD_METADATA% ^
  "%root%\scripts\ralphus_runner_entry.py"
if errorlevel 1 (popd & exit /b 1)
popd

echo == done; artifacts in %dist% ==
dir "%dist%"
echo.
echo Layout (each Python exe must stay beside its own _internal\ directory):
echo   %dist%\ralphus-daemon.exe
echo   %dist%\ralphus-librarian.exe
echo   %dist%\ralphus\ralphus.exe
echo   %dist%\ralphus-runner\ralphus-runner.exe
