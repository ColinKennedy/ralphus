@echo off
setlocal enabledelayedexpansion
rem Build the four ralphus executables into .\dist as copyable standalone binaries.
rem
rem   daemon    (Rust)   -> dist\ralphus-daemon.exe
rem   librarian (Rust)   -> dist\ralphus-librarian.exe
rem   CLI       (Python) -> dist\ralphus.exe          (one-file, via PyInstaller)
rem   runner    (Python) -> dist\ralphus-runner.exe   (one-file, via PyInstaller)
rem
rem The Rust binaries link SQLite in (rusqlite `bundled`) so they need no system
rem libraries. The Python CLI and runner are each bundled with their interpreter
rem by PyInstaller. Both are built with the `runner` extra (pulls in
rem pydantic-ai): the runner needs it to run native model-agent sessions, and
rem the CLI needs it too since `ralphus author` drives pydantic-ai in-process
rem (it is not delegated to the runner subprocess).

set "root=%~dp0.."
for %%I in ("%root%") do set "root=%%~fI"
set "dist=%root%\dist"
if not exist "%dist%" mkdir "%dist%"

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
rem metadata unless told to, so onefile builds crash with
rem `PackageNotFoundError` the first time `ralphus author` (or the runner)
rem actually imports pydantic-ai. --copy-metadata pulls each package's
rem dist-info into the bundle so those lookups succeed.
set "PYD_METADATA=--copy-metadata genai-prices --copy-metadata pydantic-ai --copy-metadata pydantic-ai-slim --copy-metadata pydantic-graph --copy-metadata pydantic-evals --copy-metadata pydantic --copy-metadata pydantic_core --copy-metadata pydantic-settings --copy-metadata logfire --copy-metadata logfire-api --copy-metadata anthropic --copy-metadata openai --copy-metadata mcp --copy-metadata httpx --copy-metadata google-genai --copy-metadata opentelemetry-api --copy-metadata opentelemetry-sdk --copy-metadata tenacity"

echo == building Python CLI (one-file) ==
rem Build from the project venv (editable install) so PyInstaller bundles the
rem current source; a fresh `uvx --with .` env can serve a cached wheel instead.
uv run --extra runner --with pyinstaller ^
  pyinstaller --onefile --clean --name ralphus ^
  --distpath "%dist%" --workpath "%root%\target\pyinstaller" --specpath "%root%\target\pyinstaller" ^
  --version-file "%root%\scripts\version_info_cli.txt" ^
  %PYD_METADATA% ^
  "%root%\scripts\ralphus_entry.py"
if errorlevel 1 (popd & exit /b 1)

echo == building Python runner (one-file, with the 'runner' extra) ==
uv run --extra runner --with pyinstaller ^
  pyinstaller --onefile --clean --name ralphus-runner ^
  --distpath "%dist%" --workpath "%root%\target\pyinstaller" --specpath "%root%\target\pyinstaller" ^
  --version-file "%root%\scripts\version_info_runner.txt" ^
  %PYD_METADATA% ^
  "%root%\scripts\ralphus_runner_entry.py"
if errorlevel 1 (popd & exit /b 1)
popd

echo == done; artifacts in %dist% ==
dir "%dist%"
