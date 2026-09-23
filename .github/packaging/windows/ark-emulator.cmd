@echo off
rem Runs the GUI executable with inherited streams and returns its exit code.
setlocal EnableExtensions DisableDelayedExpansion
rem Batch execution waits for this process, without waiting for an emulator it starts
"%~dp0..\ark-emulator.exe" %*
exit /b %errorlevel%
