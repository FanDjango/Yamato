# SPDX-License-Identifier: MIT
#
# Yamato - fan control software for ThinkPads
# Copyright (c) 2026 David Brustein
#
# Reads or sets the version, in the one place it is defined and the one place
# it is duplicated. Called by build.cmd; usable on its own.
#
#   version.ps1              print the current version
#   version.ps1 0.2.0        set it
#   version.ps1 -Bump patch  0.1.0 -> 0.1.1
#   version.ps1 -Bump minor  0.1.3 -> 0.2.0
#   version.ps1 -Bump major  0.2.1 -> 1.0.0

param(
    [Parameter(Position = 0)][string]$Version,
    [ValidateSet('major', 'minor', 'patch')][string]$Bump
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path

$cargo = Join-Path $root 'Cargo.toml'
$iss   = Join-Path $root 'installer\yamato.iss'

function Get-Version {
    $line = Select-String -Path $cargo -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $line) { throw "no version found in $cargo" }

    return $line.Matches[0].Groups[1].Value
}

$current = Get-Version

if ($Bump) {
    if ($current -notmatch '^(\d+)\.(\d+)\.(\d+)') {
        throw "current version '$current' is not major.minor.patch, cannot bump"
    }

    $maj = [int]$Matches[1]; $min = [int]$Matches[2]; $pat = [int]$Matches[3]

    switch ($Bump) {
        'major' { $maj++; $min = 0; $pat = 0 }
        'minor' { $min++; $pat = 0 }
        'patch' { $pat++ }
    }

    $Version = "$maj.$min.$pat"
}

if (-not $Version) {
    Write-Host $current
    exit 0
}

if ($Version -notmatch '^\d+\.\d+\.\d+') {
    throw "'$Version' is not major.minor.patch"
}

# The workspace is the source of truth; every crate inherits from it.
$text = [IO.File]::ReadAllText($cargo)
$text = [Regex]::Replace($text, '(?m)^version\s*=\s*"[^"]+"', "version = `"$Version`"", 1)
[IO.File]::WriteAllText($cargo, $text)

# Inno Setup cannot read Cargo.toml, so this one copy has to be kept in step.
if (Test-Path $iss) {
    $text = [IO.File]::ReadAllText($iss)
    $text = [Regex]::Replace($text, '#define AppVersion\s+"[^"]+"', "#define AppVersion  `"$Version`"")
    [IO.File]::WriteAllText($iss, $text)
}

Write-Host "$current -> $Version"
