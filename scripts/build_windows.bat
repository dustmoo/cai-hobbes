@echo off
REM build_windows.bat - Wrapper for PowerShell build script
REM Run from project root: scripts\build_windows.bat [args]

echo Hobbes Windows Build
echo =====================
echo.

REM Forward all arguments to PowerShell script
powershell -ExecutionPolicy Bypass -File "%~dp0build_windows.ps1" %*

if %ERRORLEVEL% neq 0 (
    echo.
    echo Build failed with error code: %ERRORLEVEL%
    pause
    exit /b %ERRORLEVEL%
)
