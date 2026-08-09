# SPDX-License-Identifier: MIT
#
# Yamato - fan control software for ThinkPads
# Copyright (c) 2026 David Brustein
#
# Checks a built binary for local paths and anything else identifying the
# machine it was built on. Called by build.cmd; exits non-zero on a hit so a
# release cannot be staged with them still in it.
#
#   scrub-check.ps1 target\release\yamato.exe

param([Parameter(Mandatory = $true)][string]$Binary)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $Binary)) {
    Write-Host "  $Binary does not exist" -ForegroundColor Red
    exit 1
}

$bytes = [IO.File]::ReadAllBytes($Binary)

# Strings land in the binary as both ASCII and UTF-16, and a path only present
# in the wide form would slip past an ASCII-only scan.
$ascii = [Text.Encoding]::ASCII.GetString($bytes)
$wide  = [Text.Encoding]::Unicode.GetString($bytes)
$hay   = $ascii + "`n" + $wide

# Things that identify this machine or this user. Deliberately broader than
# just the source directory: a stray temp path or the cargo registry gives the
# same information away.
$patterns = @(
    @{ Name = 'user profile';    Value = $env:USERPROFILE },
    @{ Name = 'user name';       Value = "\$($env:USERNAME)\" },
    @{ Name = 'source tree';     Value = (Get-Location).Path },
    @{ Name = 'cargo registry';  Value = '\.cargo\registry' },
    @{ Name = 'rustup toolchain'; Value = '\.rustup\toolchains' },
    @{ Name = 'computer name';   Value = $env:COMPUTERNAME }
)

$found = @()

foreach ($p in $patterns) {
    if ([string]::IsNullOrWhiteSpace($p.Value)) { continue }

    if ($hay.Contains($p.Value)) {
        $found += $p
    }
}

# Any drive-letter path at all, as a catch-all for something the named
# patterns missed. Reported separately because a handful of these are
# legitimate: Yamato names the PawnIO device path and the module file.
$paths = [Regex]::Matches($hay, '[A-Za-z]:\\[A-Za-z0-9_\-\\. ]{6,}') |
    ForEach-Object { $_.Value } |
    Where-Object { $_ -notmatch '^[A-Za-z]:\\Windows' } |
    Select-Object -Unique

if ($found.Count -gt 0) {
    Write-Host "  FAILED. The binary contains:" -ForegroundColor Red
    foreach ($f in $found) {
        Write-Host ("    {0}: {1}" -f $f.Name, $f.Value) -ForegroundColor Red
    }
    Write-Host "  Check the --remap-path-prefix flags in build.cmd." -ForegroundColor Yellow
    exit 1
}

Write-Host "  clean: no local paths, user name or machine name" -ForegroundColor Green

if ($paths.Count -gt 0) {
    Write-Host "  absolute paths present (expected, not identifying):" -ForegroundColor DarkGray
    foreach ($p in $paths | Select-Object -First 8) {
        Write-Host "    $p" -ForegroundColor DarkGray
    }
}

exit 0
