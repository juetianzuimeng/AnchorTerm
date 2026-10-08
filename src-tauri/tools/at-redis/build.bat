@echo off
setlocal

echo Getting dependencies...
go mod tidy

echo Building at-redis for linux-amd64...
set GOOS=linux
set GOARCH=amd64
set CGO_ENABLED=0
go build -ldflags "-s -w" -o ..\..\resources\tools\at-redis-linux-amd64

echo Building at-redis for linux-arm64...
set GOOS=linux
set GOARCH=arm64
set CGO_ENABLED=0
go build -ldflags "-s -w" -o ..\..\resources\tools\at-redis-linux-arm64

echo Build complete.
endlocal
