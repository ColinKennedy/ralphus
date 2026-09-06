@echo off
setlocal enabledelayedexpansion
rem docs-screenshots.cmd -- regenerate docs\site\pages\screenshots\*.png
rem (mirrors docs-screenshots.sh). Separate from docs-build.cmd on purpose:
rem this launches headless Chromium via Playwright and is the only expensive
rem part of the docs build. Only run it after a librarian\assets\board.html
rem UI change; review and commit the resulting PNG diffs.

set "root=%~dp0.."
for %%I in ("%root%") do set "root=%%~fI"

echo == building ralphus-librarian (debug) ==
cargo build --package ralphus-librarian --manifest-path "%root%\Cargo.toml"
if errorlevel 1 exit /b 1

echo == syncing docs venv (uv) ==
pushd "%root%\cli-py"
uv sync --extra docs >nul
if errorlevel 1 (popd & exit /b 1)

echo == installing headless Chromium (Playwright) ==
uv run playwright install chromium
if errorlevel 1 (popd & exit /b 1)

echo == generating screenshots ==
uv run ralphus-docs-shots
if errorlevel 1 (popd & exit /b 1)
popd

echo == done -^> %root%\docs\site\pages\screenshots\ ==
endlocal
