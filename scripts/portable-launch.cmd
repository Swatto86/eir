@echo off
setlocal

rem Portable Eir uses the machine Evergreen WebView2 runtime (Edge / WebView2
rem Runtime). No fixed runtime CAB is shipped.

rem Do not let the runner or its children hold the self-extraction directory
rem as their working directory while IExpress tears it down.
cd /d "%TEMP%"
if errorlevel 1 exit /b 1

"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0portable-run.ps1"
exit /b %errorlevel%
