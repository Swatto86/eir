#Requires -Version 5
# Generates the Eir app icons: a green pentagon "shield" badge with a bold "E".
# Produces 32x32.png, 128x128.png, 128x128@2x.png (256) and a multi-size icon.ico.
# Run with Windows PowerShell (GDI+): powershell.exe -NoProfile -File icons\gen-icon.ps1
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$here = Split-Path -Parent $MyInvocation.MyCommand.Path

# Brand colours. The shield border + letter are the bright "foreground" the app
# recolours per status; the fill stays dark (max channel <= 80) so it is never tinted.
$green = [System.Drawing.Color]::FromArgb(255, 61, 220, 132)  # #3DDC84
$fill  = [System.Drawing.Color]::FromArgb(255, 13, 17, 28)    # #0D111C (near-black navy)

# Render the artwork at a single high resolution, then downscale to each target
# size. Supersampling like this gives clean, crisp edges at small sizes — far
# better than antialiasing a thick stroke natively at e.g. 32px (which blurs).
$MASTER = 1024

function New-EirMaster {
    $S = $MASTER
    $bmp = New-Object System.Drawing.Bitmap($S, $S, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode     = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode   = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAlias
    $g.Clear([System.Drawing.Color]::Transparent)

    # Pentagon shield: flat top, near-vertical sides, point at the bottom.
    $pts = @(
        [System.Drawing.PointF]::new(0.50 * $S, 0.93 * $S),  # bottom point
        [System.Drawing.PointF]::new(0.11 * $S, 0.45 * $S),  # left mid
        [System.Drawing.PointF]::new(0.20 * $S, 0.13 * $S),  # top left
        [System.Drawing.PointF]::new(0.80 * $S, 0.13 * $S),  # top right
        [System.Drawing.PointF]::new(0.89 * $S, 0.45 * $S)   # right mid
    )

    $brush = New-Object System.Drawing.SolidBrush($fill)
    $g.FillPolygon($brush, $pts)

    $pen = New-Object System.Drawing.Pen($green, [float]($S * 0.075))
    $pen.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round
    $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $pen.EndCap   = [System.Drawing.Drawing2D.LineCap]::Round
    $g.DrawPolygon($pen, $pts)

    # Bold "E", optically centred (nudged up because the shield tapers downward).
    $fontName = if ([System.Drawing.FontFamily]::Families.Name -contains 'Segoe UI') { 'Segoe UI' } else { 'Arial' }
    $font = New-Object System.Drawing.Font($fontName, [float]($S * 0.40), [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)
    $sf = New-Object System.Drawing.StringFormat
    $sf.Alignment = [System.Drawing.StringAlignment]::Center
    $sf.LineAlignment = [System.Drawing.StringAlignment]::Center
    $textBrush = New-Object System.Drawing.SolidBrush($green)
    $rect = New-Object System.Drawing.RectangleF(0, [float](-$S * 0.04), [float]$S, [float]$S)
    $g.DrawString('E', $font, $textBrush, $rect, $sf)

    $g.Dispose(); $brush.Dispose(); $pen.Dispose(); $font.Dispose(); $textBrush.Dispose(); $sf.Dispose()
    return $bmp
}

function Resize-Bitmap {
    param([System.Drawing.Bitmap]$Src, [int]$S)
    $bmp = New-Object System.Drawing.Bitmap($S, $S, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.InterpolationMode  = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.SmoothingMode      = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
    $g.PixelOffsetMode    = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)
    $g.DrawImage($Src, (New-Object System.Drawing.Rectangle(0, 0, $S, $S)))
    $g.Dispose()
    return $bmp
}

$master = New-EirMaster

# PNG assets used by Tauri + embedded by the UI.
$sizes = @{ '32x32.png' = 32; '128x128.png' = 128; '128x128@2x.png' = 256 }
foreach ($name in $sizes.Keys) {
    $b = Resize-Bitmap -Src $master -S $sizes[$name]
    $b.Save((Join-Path $here $name), [System.Drawing.Imaging.ImageFormat]::Png)
    $b.Dispose()
}

# Multi-size icon.ico (PNG-compressed entries; supported on Windows Vista+).
$icoSizes = 16, 32, 48, 64, 128, 256
$pngs = foreach ($s in $icoSizes) {
    $b = Resize-Bitmap -Src $master -S $s
    $ms = New-Object System.IO.MemoryStream
    $b.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $b.Dispose()
    , $ms.ToArray()
}
$master.Dispose()

$icoPath = Join-Path $here 'icon.ico'
$fs = [System.IO.File]::Create($icoPath)
$bw = New-Object System.IO.BinaryWriter($fs)
$bw.Write([uint16]0)            # reserved
$bw.Write([uint16]1)            # type = icon
$bw.Write([uint16]$icoSizes.Count)
$offset = 6 + (16 * $icoSizes.Count)
for ($i = 0; $i -lt $icoSizes.Count; $i++) {
    $s = $icoSizes[$i]
    $bw.Write([byte]($(if ($s -ge 256) { 0 } else { $s })))  # width
    $bw.Write([byte]($(if ($s -ge 256) { 0 } else { $s })))  # height
    $bw.Write([byte]0)           # colour count
    $bw.Write([byte]0)           # reserved
    $bw.Write([uint16]1)         # planes
    $bw.Write([uint16]32)        # bits per pixel
    $bw.Write([uint32]$pngs[$i].Length)
    $bw.Write([uint32]$offset)
    $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $bw.Write($p) }
$bw.Flush(); $bw.Close(); $fs.Close()

Write-Host "Wrote icons to $here (32, 128, 256 PNG + icon.ico [$($icoSizes -join ', ')])"
