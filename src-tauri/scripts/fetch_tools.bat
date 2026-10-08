@echo off
setlocal

set "RESOURCES_DIR=..\resources\tools"
if not exist "%RESOURCES_DIR%" mkdir "%RESOURCES_DIR%"

set "PATH=%USERPROFILE%\sdk\go1.26.1\go\bin;%PATH%"

echo [1/3] Building at-redis...
if exist "%RESOURCES_DIR%\at-redis-linux-amd64" if exist "%RESOURCES_DIR%\at-redis-linux-arm64" (
    echo Cached at-redis binaries, skipping build.
) else (
    cd ..\tools\at-redis
    call build.bat
    cd ..\..\scripts
)

echo [2/3] Building at-mysql...
if exist "%RESOURCES_DIR%\at-mysql-linux-amd64" if exist "%RESOURCES_DIR%\at-mysql-linux-arm64" (
    echo Cached at-mysql binaries, skipping build.
) else (
    cd ..\tools\at-mysql
    call build.bat
    cd ..\..\scripts
)

echo [3/3] Building spring-connection...
if exist "%RESOURCES_DIR%\spring-connection-linux-amd64" if exist "%RESOURCES_DIR%\spring-connection-linux-arm64" (
    echo Cached spring-connection binaries, skipping build.
) else (
    cd ..\tools\spring-connection
    call build.bat
    cd ..\..\scripts
)

echo Done.
endlocal
