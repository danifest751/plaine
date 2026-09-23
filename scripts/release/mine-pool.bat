@echo off
rem Mines Plaine on a pool. Put your own address below first: the pool pays it.
rem plaine-miner speaks plain TCP; a pool's --tls flag does not apply to it.

set "ADDRESS=plne1PUT-YOUR-ADDRESS-HERE"
set "RIG=%COMPUTERNAME%"
set "POOL=eu.rplant.xyz:17190"

if "%ADDRESS%"=="plne1PUT-YOUR-ADDRESS-HERE" (
    echo Edit mine-pool.bat and put your plne1 address in ADDRESS first.
    pause
    exit /b 1
)
cd /d "%~dp0"
rem Add --cpu-priority 1 to keep the computer responsive while mining.
plaine-miner.exe "%ADDRESS%.%RIG%@%POOL%" --status 30 %*
echo.
echo plaine-miner exited with code %ERRORLEVEL%
pause
