<#
.SYNOPSIS
  Render the Planor app icon (a rounded blue tile holding three white panes: one tall, two stacked) at
  16/24/32/48/256 px and pack the PNG frames into assets\planor.ico.
#>
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $root "assets\planor.ico"
New-Item -ItemType Directory -Force (Split-Path $out) | Out-Null

function New-RoundedPath([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
  $p = New-Object System.Drawing.Drawing2D.GraphicsPath
  $d = [Math]::Min($r * 2, [Math]::Min($w, $h))
  $p.AddArc($x, $y, $d, $d, 180, 90)
  $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
  $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
  $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
  $p.CloseFigure()
  return $p
}

function New-Frame([int]$s) {
  $bmp = New-Object System.Drawing.Bitmap $s, $s, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
  $g.Clear([System.Drawing.Color]::Transparent)
  $blue = [System.Drawing.Color]::FromArgb(255, 0x3B, 0x82, 0xF6)
  $bg = New-RoundedPath 0 0 $s $s ($s * 0.22)
  $g.FillPath((New-Object System.Drawing.SolidBrush $blue), $bg)
  $white = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::White)
  $pad = $s * 0.20; $gap = $s * 0.07
  $inner = $s - 2 * $pad
  $leftW = ($inner - $gap) * 0.52; $rightW = $inner - $gap - $leftW
  $rad = [Math]::Max(1, $s * 0.06)
  $g.FillPath($white, (New-RoundedPath $pad $pad $leftW $inner $rad))
  $halfH = ($inner - $gap) / 2
  $g.FillPath($white, (New-RoundedPath ($pad + $leftW + $gap) $pad $rightW $halfH $rad))
  $g.FillPath($white, (New-RoundedPath ($pad + $leftW + $gap) ($pad + $halfH + $gap) $rightW $halfH $rad))
  $g.Dispose()
  $ms = New-Object System.IO.MemoryStream
  $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
  return , $ms.ToArray()
}

$sizes = 16, 24, 32, 48, 256
$frames = $sizes | ForEach-Object { , (New-Frame $_) }
$fs = [System.IO.File]::Create($out)
$w = New-Object System.IO.BinaryWriter $fs
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
  $s = $sizes[$i]; $b = $frames[$i]
  $dim = if ($s -ge 256) { 0 } else { $s }
  $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
  $w.Write([uint16]1); $w.Write([uint16]32)
  $w.Write([uint32]$b.Length); $w.Write([uint32]$offset)
  $offset += $b.Length
}
foreach ($b in $frames) { $w.Write($b) }
$w.Close(); $fs.Close()
Write-Output "wrote $out ($((Get-Item $out).Length) bytes, $($sizes.Count) frames)"
