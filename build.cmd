@echo off
REM SPDX-License-Identifier: MIT
REM
REM Yamato - fan control software for ThinkPads
REM Copyright (c) 2026 David Brustein
REM
REM Builds Yamato. With no arguments it makes a release build and stages
REM everything needed to run into dist\.
REM
REM   build              release build, staged into dist\
REM   build debug        debug build, no staging
REM   build test         run the tests
REM   build installer    release build plus the Inno Setup installer
REM   build clean        remove target\ and dist\
REM
REM   build --version            print the current version
REM   build --version 0.2.0      set it, then build
REM   build --ver patch          bump the patch level, then build
REM   build --ver minor          0.1.3 -> 0.2.0
REM   build --ver major          0.2.1 -> 1.0.0

setlocal EnableDelayedExpansion
cd /d "%~dp0"

set MODE=%1

REM --- version ---------------------------------------------------------------
if /i "%MODE%"=="--version" goto :version
if /i "%MODE%"=="--ver"     goto :version
if /i "%MODE%"=="-v"        goto :version
goto :noversion

:version
if "%2"=="" (
    powershell -NoProfile -ExecutionPolicy Bypass -File version.ps1
    exit /b 0
)

if /i "%2"=="major" (
    powershell -NoProfile -ExecutionPolicy Bypass -File version.ps1 -Bump major
) else if /i "%2"=="minor" (
    powershell -NoProfile -ExecutionPolicy Bypass -File version.ps1 -Bump minor
) else if /i "%2"=="patch" (
    powershell -NoProfile -ExecutionPolicy Bypass -File version.ps1 -Bump patch
) else (
    powershell -NoProfile -ExecutionPolicy Bypass -File version.ps1 %2
)
if errorlevel 1 exit /b 1

REM Setting a version is almost always the prelude to cutting a build, so
REM carry straight on into one rather than making it two commands.
REM
REM A third argument says which kind. Without this, "build --ver 1.0.0
REM installer" threw the word installer away and quietly built a release
REM without one, which looks identical to success right up until you go
REM looking for the setup file.
set MODE=release
if not "%3"=="" set MODE=%3

:noversion
if "%MODE%"=="" set MODE=release

where cargo >nul 2>&1
if errorlevel 1 (
    echo cargo was not found. Run bootstrap.ps1 first:
    echo   powershell -ExecutionPolicy Bypass -File bootstrap.ps1
    exit /b 1
)

if /i "%MODE%"=="clean" (
    echo Cleaning...
    cargo clean
    if exist dist rmdir /s /q dist
    echo Done.
    exit /b 0
)

if /i "%MODE%"=="test" (
    cargo test --workspace
    exit /b !errorlevel!
)

if /i "%MODE%"=="debug" (
    cargo build --workspace
    if errorlevel 1 exit /b 1
    echo.
    echo Built target\debug\yamato.exe
    exit /b 0
)

REM --- release -------------------------------------------------------------

REM Tests first. This program drives cooling hardware; shipping a build whose
REM fan-curve tests fail is not a trade worth making for a faster build.
echo Running tests...
cargo test --workspace --quiet
if errorlevel 1 (
    echo.
    echo Tests failed. Not building a release.
    exit /b 1
)

REM Strip local paths out of the binary.
REM
REM rustc bakes absolute source paths into panic messages and debug info, so a
REM release built here would otherwise carry C:\Users\<name>\... around inside
REM it, along with the same for every dependency in the cargo registry and the
REM toolchain's own sources. Remapping rewrites them at compile time.
REM
REM Changing RUSTFLAGS invalidates the build cache, so a release build after a
REM debug one recompiles from scratch. That is the cost of not shipping paths.
REM One assignment, not appended in pieces: an empty variable in the middle
REM produces a malformed flag that rustc quietly ignores along with the rest,
REM which is how this silently shipped paths the first time.
REM
REM Order matters, and in the opposite direction to what you would expect:
REM rustc walks the mappings in reverse, so the LAST one that matches wins and
REM the most specific has to come last. Written the other way round, the broad
REM %USERPROFILE% rule swallowed everything and the binary shipped with
REM home\.cargo\registry\... paths in it. The user's name was gone, so the
REM point was half met, but the registry layout was still there and the check
REM below rightly refused the build.
set RUSTFLAGS=--remap-path-prefix=%USERPROFILE%=home --remap-path-prefix=%USERPROFILE%\.rustup=rust --remap-path-prefix=%USERPROFILE%\.cargo=cargo --remap-path-prefix=%USERPROFILE%\.cargo\registry\src=deps --remap-path-prefix=%CD%=yamato

echo.
echo Building release...
cargo build --workspace --release
if errorlevel 1 exit /b 1

REM Prove the remapping worked rather than trusting that it did. A typo in a
REM prefix above fails silently and ships the paths anyway.
echo.
echo Checking the binary for local paths...
powershell -NoProfile -ExecutionPolicy Bypass -File scrub-check.ps1 target\release\yamato.exe
if errorlevel 1 (
    echo.
    echo Local paths found in the binary. Not staging a release.
    exit /b 1
)

echo.
echo Staging dist\...
if not exist dist mkdir dist
copy /y target\release\yamato.exe dist\ >nul
REM The PawnIO module has to sit next to the exe; it is looked for there
REM rather than in the working directory, because a service and a Run-key
REM launch both start somewhere else entirely.
copy /y assets\LpcACPIEC.bin dist\ >nul
REM Its source, which the LGPL wants alongside the object it ships with.
copy /y assets\LpcACPIEC.p dist\ >nul
copy /y LICENSE dist\ >nul
copy /y LICENSE.LGPL-2.1.txt dist\ >nul
copy /y NOTICE.md dist\ >nul
copy /y THIRD-PARTY-LICENSES.txt dist\ >nul

for %%F in (dist\yamato.exe) do set SIZE=%%~zF
echo.
echo   dist\yamato.exe  (!SIZE! bytes)
echo   dist\LpcACPIEC.bin
echo   dist\LICENSE, LICENSE.LGPL-2.1.txt, NOTICE.md
echo.
echo PawnIO is not bundled by design; install it from https://pawnio.eu

if /i not "%MODE%"=="installer" goto :done

REM --- installer -----------------------------------------------------------

set ISCC=
if exist "%ProgramFiles(x86)%\Inno Setup 6\ISCC.exe" set ISCC=%ProgramFiles(x86)%\Inno Setup 6\ISCC.exe
if exist "%ProgramFiles%\Inno Setup 6\ISCC.exe" set ISCC=%ProgramFiles%\Inno Setup 6\ISCC.exe

if "!ISCC!"=="" (
    echo.
    echo Inno Setup 6 was not found, skipping the installer.
    exit /b 0
)

echo.
echo Building installer...
"!ISCC!" installer\yamato.iss
if errorlevel 1 exit /b 1

:done
echo.
echo Done.
exit /b 0
