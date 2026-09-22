@echo off
rem Runs the GUI executable with inherited streams and returns its exit code.
setlocal EnableExtensions DisableDelayedExpansion
set "executable=%~dp0..\ark-emulator.exe"
if not exist "%executable%" set "executable=%~dp0..\Ark Emulator.exe"
rem Batch execution waits for this process, without waiting for an emulator it starts
"%executable%" %*
exit /b %errorlevel%
