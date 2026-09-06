@echo off
setlocal enabledelayedexpansion
rem docs-build.cmd -- FAST docs build: Markdown -> HTML only, no screenshots
rem (mirrors docs-build.sh). Renders docs\site\pages\*.md (MkDocs + Material)
rem into docs\site\_site\ using whatever PNGs are already committed under
rem docs\site\pages\screenshots\. Never touches Playwright, so this is cheap
rem to re-run on every doc edit.
rem
rem To regenerate the screenshots themselves (only needed after a board.html
rem UI change), run docs-screenshots.cmd instead -- a separate, heavier step.

set "root=%~dp0.."
for %%I in ("%root%") do set "root=%%~fI"

echo == syncing docs venv (uv) ==
pushd "%root%\cli-py"
uv sync --extra docs >nul
if errorlevel 1 (popd & exit /b 1)

echo == screenshot coverage lint ==
uv run ralphus-docs-lint
if errorlevel 1 (popd & exit /b 1)

echo == mkdocs build ==
uv run mkdocs build --config-file "%root%\docs\site\mkdocs.yml"
if errorlevel 1 (popd & exit /b 1)
popd

echo == done -^> %root%\docs\site\_site\index.html ==
endlocal
