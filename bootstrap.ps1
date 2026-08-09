# SPDX-License-Identifier: MIT
#
# Yamato - fan control software for ThinkPads
# Copyright (c) 2026 David Brustein
#
# Gets a machine ready to build Yamato. Safe to re-run: everything here checks
# before it installs, so this is also a way to find out what is missing.
#
#   powershell -ExecutionPolicy Bypass -File bootstrap.ps1

$ErrorActionPreference = 'Stop'

function Test-Command($name) {
    $null -ne (Get-Command $name -ErrorAction SilentlyContinue)
}

function Say($ok, $text) {
    if ($ok) { Write-Host "  [ok]   $text" -ForegroundColor Green }
    else     { Write-Host "  [need] $text" -ForegroundColor Yellow }
}

Write-Host "`nYamato build environment`n" -ForegroundColor Cyan

$missing = @()

# --- Rust -----------------------------------------------------------------
$hasRust = Test-Command 'cargo'
Say $hasRust "Rust toolchain$(if ($hasRust) { ' (' + (rustc --version) + ')' })"
if (-not $hasRust) { $missing += 'rust' }

# Yamato is x64. The MSVC toolchain is required; gnu will not link against the
# Windows service and Direct2D imports the same way.
if ($hasRust) {
    $target = (rustc -vV | Select-String '^host:').ToString().Split(' ')[1]
    $isMsvc = $target -like '*windows-msvc'
    Say $isMsvc "MSVC target ($target)"
    if (-not $isMsvc) { $missing += 'rust-msvc' }
}

# --- MSVC build tools ------------------------------------------------------
# rustc shells out to link.exe, so the Visual Studio C++ build tools have to be
# present even though no C++ is compiled here.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$hasMsvc = $false
if (Test-Path $vswhere) {
    $vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
    $hasMsvc = -not [string]::IsNullOrWhiteSpace($vs)
}
Say $hasMsvc "MSVC build tools (link.exe)"
if (-not $hasMsvc) { $missing += 'msvc' }

# --- Inno Setup ------------------------------------------------------------
# Only needed to build the installer, not to build the program.
$iscc = @(
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
Say ($null -ne $iscc) "Inno Setup 6 (optional, for the installer)"

# --- PawnIO ----------------------------------------------------------------
# Not a build dependency at all, but you cannot run what you build without it.
$pawn = (Get-Service PawnIO -ErrorAction SilentlyContinue) -ne $null
Say $pawn "PawnIO driver (optional, needed to run)"
if (-not $pawn) {
    Write-Host "         install from https://pawnio.eu" -ForegroundColor DarkGray
}

# --- Fix what we can -------------------------------------------------------
if ($missing.Count -eq 0) {
    Write-Host "`nEverything needed is present. Build with: .\build.cmd`n" -ForegroundColor Green
    exit 0
}

Write-Host ""
if (Test-Command 'winget') {
    Write-Host "Installing what is missing with winget..." -ForegroundColor Cyan

    if ($missing -contains 'rust' -or $missing -contains 'rust-msvc') {
        winget install --id Rustlang.Rustup -e --accept-source-agreements --accept-package-agreements
        if (Test-Command 'rustup') { rustup default stable-x86_64-pc-windows-msvc }
    }

    if ($missing -contains 'msvc') {
        # The build tools alone, not the whole IDE.
        winget install --id Microsoft.VisualStudio.2022.BuildTools -e `
            --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
    }

    Write-Host "`nDone. Open a new shell so PATH changes take effect, then: .\build.cmd`n" -ForegroundColor Green
} else {
    Write-Host "winget is not available. Install these by hand:" -ForegroundColor Yellow
    if ($missing -contains 'rust' -or $missing -contains 'rust-msvc') {
        Write-Host "  Rust (MSVC toolchain)  https://rustup.rs"
    }
    if ($missing -contains 'msvc') {
        Write-Host "  VS 2022 Build Tools, Desktop development with C++"
        Write-Host "  https://visualstudio.microsoft.com/downloads/"
    }
    Write-Host ""
}
