@echo off
REM RUN-WINDOWS.bat - double-click to build + launch Cockatiel on Windows.
REM Builds the engine, the user database and the TUI from THIS folder, then
REM launches the TUI. Rust is required to build; if it is missing you are
REM offered the option to install it, and declining simply exits.
setlocal enabledelayedexpansion
cd /d "%~dp0"

echo Cockatiel one-click launcher
echo Repository: %CD%
echo.

if not exist "cockatiel.lock" goto :notrepo
if not exist "cockatiel_engine-rs" goto :notrepo
if not exist "modules" goto :notrepo

if not exist "cockatiel_engine-rs\Cargo.toml" goto :submodules
if not exist "modules\cockatiel_module-tui_v2-rs\Cargo.toml" goto :submodules

:rustcheck
where cargo >nul 2>nul
if errorlevel 1 goto :norust
goto :build

:build
echo Building Cockatiel (release). The first build takes several minutes.
echo.
cargo build --release --manifest-path "cockatiel_engine-rs\Cargo.toml" || goto :failed
cargo build --release --manifest-path "cockatiel_engine-rs\modules\cockatiel_user_database-rs\Cargo.toml" || goto :failed
cargo build --release --manifest-path "modules\cockatiel_module-tui_v2-rs\Cargo.toml" || goto :failed

set "TUI=modules\cockatiel_module-tui_v2-rs\target\release\cockatiel-tui-v2.exe"
if not exist "%TUI%" (
  echo.
  echo The TUI binary was not produced at %TUI%.
  goto :failed
)

echo.
echo Launching Cockatiel...
echo.
"%TUI%" %*
set "code=%errorlevel%"
if not "%code%"=="0" (
  echo.
  echo Cockatiel did not start ^(exit code %code%^).
  pause
)
exit /b %code%

:norust
echo Rust ^(cargo^) is needed to build Cockatiel and is not installed.
echo It is free and takes a couple of minutes: https://rustup.rs
echo.
set "answer="
set /p "answer=Install Rust now via rustup? [y/N] "
if /i not "%answer%"=="y" goto :norust_declined

echo.
echo Installing Rust (rustup)...
set "RUSTUP_ARCH=x86_64"
if /i "%PROCESSOR_ARCHITECTURE%"=="ARM64" set "RUSTUP_ARCH=aarch64"
powershell -NoProfile -ExecutionPolicy Bypass -Command "Invoke-WebRequest -UseBasicParsing 'https://win.rustup.rs/%RUSTUP_ARCH%' -OutFile \"$env:TEMP\rustup-init.exe\""
"%TEMP%\rustup-init.exe" -y
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
where cargo >nul 2>nul
if errorlevel 1 goto :rust_still_missing
goto :build

:norust_declined
echo.
echo No problem. Install Rust from https://rustup.rs, then run this launcher again.
pause
exit /b 0

:rust_still_missing
echo.
echo cargo is still not on PATH after installing Rust.
echo Close this window, open a new one, and run this launcher again.
pause
exit /b 1

:submodules
echo Fetching submodules (first run)...
where git >nul 2>nul
if errorlevel 1 (
  echo.
  echo The repo's submodules are missing and git is not installed.
  echo Install git, then run this launcher again.
  pause
  exit /b 1
)
git submodule update --init --recursive || goto :failed
goto :rustcheck

:notrepo
echo.
echo This launcher must sit in the Cockatiel repository root
echo (it expects cockatiel.lock, cockatiel_engine-rs\ and modules\ next to it).
pause
exit /b 1

:failed
echo.
echo Build failed.
pause
exit /b 1
