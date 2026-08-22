@echo off
setlocal
set "script_dir=%~dp0"
set "pycmd=py -3"
where py >nul 2>&1 || set "pycmd=python"
%pycmd% "%script_dir%build-bundle.py" %*
