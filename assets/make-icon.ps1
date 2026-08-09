# SPDX-License-Identifier: MIT
#
# Yamato - fan control software for ThinkPads
# Copyright (c) 2026 David Brustein
#
# Draws the Yamato mark: a red dome. A soft highlight up and to the left and a
# darker rim, so it still reads as round at 16 pixels.
#
# Plain by design: a red dome, no texture.
#
# Regenerate with:  powershell -ExecutionPolicy Bypass -File assets\make-icon.ps1

Add-Type -AssemblyName System.Drawing

$ErrorActionPreference = 'Stop'
$outDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$sizes  = @(16, 20, 24, 32, 48, 64, 128, 256)

function New-Dot {
    param([int]$Size, [switch]$Flat, [string]$Tint = 'red')

    $bmp = New-Object Drawing.Bitmap($Size, $Size, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g   = [Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode     = 'AntiAlias'
    $g.InterpolationMode = 'HighQualityBicubic'
    $g.PixelOffsetMode   = 'HighQuality'
    $g.Clear([Drawing.Color]::Transparent)

    # Leave a little air so the rim is not clipped at small sizes.
    $pad  = [Math]::Max(1.0, $Size * 0.06)
    $d    = $Size - (2 * $pad)
    $rect = New-Object Drawing.RectangleF($pad, $pad, $d, $d)

    # Palette. The tray icon tints by temperature band; the app icon is red.
    switch ($Tint) {
        'warm' { $hi = [Drawing.Color]::FromArgb(255,255,176, 64); $mid = [Drawing.Color]::FromArgb(255,224,124, 16); $rim = [Drawing.Color]::FromArgb(255,128, 64,  0) }
        'hot'  { $hi = [Drawing.Color]::FromArgb(255,255,128,112); $mid = [Drawing.Color]::FromArgb(255,214, 40, 32); $rim = [Drawing.Color]::FromArgb(255,110,  8,  8) }
        'idle' { $hi = [Drawing.Color]::FromArgb(255,150,150,158); $mid = [Drawing.Color]::FromArgb(255, 96, 96,104); $rim = [Drawing.Color]::FromArgb(255, 48, 48, 54) }
        'green'{ $hi = [Drawing.Color]::FromArgb(255,126,236,142); $mid = [Drawing.Color]::FromArgb(255, 34,168, 68); $rim = [Drawing.Color]::FromArgb(255,  8, 82, 32) }
        default{ $hi = [Drawing.Color]::FromArgb(255,255,110,110); $mid = [Drawing.Color]::FromArgb(255,206, 27, 34); $rim = [Drawing.Color]::FromArgb(255,104,  6, 10) }
    }

    # The dome. A path gradient puts the light source up and left, which is
    # what makes a flat circle look like a dome.
    $path = New-Object Drawing.Drawing2D.GraphicsPath
    $path.AddEllipse($rect)
    $brush = New-Object Drawing.Drawing2D.PathGradientBrush($path)
    $brush.CenterPoint    = New-Object Drawing.PointF(($pad + $d * 0.36), ($pad + $d * 0.32))
    $brush.CenterColor    = $hi
    $brush.SurroundColors = @($mid)
    $g.FillEllipse($brush, $rect)

    # Rim, so it holds its shape against a dark taskbar.
    $penW = [Math]::Max(1.0, $Size * 0.042)
    $pen  = New-Object Drawing.Pen($rim, $penW)
    $inset = $penW / 2.0
    $g.DrawEllipse($pen, ($rect.X + $inset), ($rect.Y + $inset), ($rect.Width - $penW), ($rect.Height - $penW))



    # Specular bloom, kept subtle so it does not read as a bubble.
    $glossRect = New-Object Drawing.RectangleF(
        ($pad + $d * 0.17), ($pad + $d * 0.12), ($d * 0.42), ($d * 0.30))
    $glossPath = New-Object Drawing.Drawing2D.GraphicsPath
    $glossPath.AddEllipse($glossRect)
    $gloss = New-Object Drawing.Drawing2D.PathGradientBrush($glossPath)
    $gloss.CenterColor    = [Drawing.Color]::FromArgb(120, 255, 255, 255)
    $gloss.SurroundColors = @([Drawing.Color]::FromArgb(0, 255, 255, 255))
    $g.FillEllipse($gloss, $glossRect)

    $gloss.Dispose(); $glossPath.Dispose(); $pen.Dispose(); $brush.Dispose(); $path.Dispose(); $g.Dispose()
    return $bmp
}

function Save-Ico {
    param([string]$Path, [int[]]$Sizes, [string]$Tint = 'red')

    $pngs = @()
    foreach ($s in $Sizes) {
        $bmp = New-Dot -Size $s -Tint $Tint
        $ms  = New-Object IO.MemoryStream
        $bmp.Save($ms, [Drawing.Imaging.ImageFormat]::Png)
        $pngs += ,@($s, $ms.ToArray())
        $ms.Dispose(); $bmp.Dispose()
    }

    $fs = [IO.File]::Create($Path)
    $bw = New-Object IO.BinaryWriter($fs)

    # ICONDIR
    $bw.Write([UInt16]0); $bw.Write([UInt16]1); $bw.Write([UInt16]$pngs.Count)

    # PNG-compressed entries. Windows Vista and later read these at any size.
    $offset = 6 + (16 * $pngs.Count)
    foreach ($p in $pngs) {
        $s = $p[0]; $bytes = $p[1]
        $bw.Write([Byte]$(if ($s -ge 256) { 0 } else { $s }))
        $bw.Write([Byte]$(if ($s -ge 256) { 0 } else { $s }))
        $bw.Write([Byte]0); $bw.Write([Byte]0)
        $bw.Write([UInt16]1); $bw.Write([UInt16]32)
        $bw.Write([UInt32]$bytes.Length); $bw.Write([UInt32]$offset)
        $offset += $bytes.Length
    }
    foreach ($p in $pngs) { $bw.Write($p[1]) }

    $bw.Flush(); $bw.Dispose(); $fs.Dispose()
    Write-Host ("  {0}  ({1} bytes, {2} sizes)" -f (Split-Path -Leaf $Path), (Get-Item $Path).Length, $pngs.Count)
}

Write-Host "Drawing the Yamato mark:"
# The application mark stays red. The tray tints by thermal state:
# green while everything is fine, amber warming, red genuinely hot, gray when
# Yamato is not driving the fan at all.
Save-Ico -Path (Join-Path $outDir 'yamato.ico')      -Sizes $sizes            -Tint 'red'
Save-Ico -Path (Join-Path $outDir 'tray-normal.ico') -Sizes @(16,20,24,32,48) -Tint 'green'
Save-Ico -Path (Join-Path $outDir 'tray-warm.ico')   -Sizes @(16,20,24,32,48) -Tint 'warm'
Save-Ico -Path (Join-Path $outDir 'tray-hot.ico')    -Sizes @(16,20,24,32,48) -Tint 'hot'
Save-Ico -Path (Join-Path $outDir 'tray-idle.ico')   -Sizes @(16,20,24,32,48) -Tint 'idle'

# A big PNG for the readme and the installer banner.
$big = New-Dot -Size 256 -Tint 'red'
$big.Save((Join-Path $outDir 'yamato-256.png'), [Drawing.Imaging.ImageFormat]::Png)
$big.Dispose()
Write-Host "  yamato-256.png"

$grn = New-Dot -Size 256 -Tint 'green'
$grn.Save((Join-Path $outDir 'tray-normal-256.png'), [Drawing.Imaging.ImageFormat]::Png)
$grn.Dispose()
Write-Host "  tray-normal-256.png"
