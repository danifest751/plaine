@echo off
rem Starts the Plaine node with the settings in noded.toml beside this file.
rem Leave this window open while you use the wallet or mine. Ctrl+C stops it.
cd /d "%~dp0"
plaine-noded.exe --config "%~dp0noded.toml"
echo.
echo plaine-noded exited with code %ERRORLEVEL%
pause
