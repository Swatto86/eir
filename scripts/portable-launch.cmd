@echo off
setlocal

"%SystemRoot%\System32\expand.exe" "%~dp0Microsoft.WebView2.FixedVersionRuntime.*.x64.cab" -F:* "%~dp0." >nul
if errorlevel 1 exit /b 1

set "EIR_WEBVIEW2="
for /d %%D in ("%~dp0Microsoft.WebView2.FixedVersionRuntime.*.x64") do set "EIR_WEBVIEW2=%%~fD"
if not defined EIR_WEBVIEW2 exit /b 1

"%SystemRoot%\System32\icacls.exe" "%EIR_WEBVIEW2%" /grant "*S-1-15-2-2:(OI)(CI)(RX)" "*S-1-15-2-1:(OI)(CI)(RX)" /T /C >nul
if errorlevel 1 exit /b 1

rem Do not let the runner or its children hold the self-extraction directory
rem as their working directory while IExpress tears it down.
cd /d "%TEMP%"
if errorlevel 1 exit /b 1

"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0portable-run.ps1"
exit /b %errorlevel%
