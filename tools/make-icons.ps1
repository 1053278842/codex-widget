# Generates the app icon (1024px) and the tray glyph (64px) with System.Drawing.
# Output: tools/app-icon.png, tools/tray.png
Add-Type -AssemblyName System.Drawing

function New-RoundedPath([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
  $path = New-Object System.Drawing.Drawing2D.GraphicsPath
  $d = $r * 2
  $path.AddArc($x, $y, $d, $d, 180, 90)
  $path.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
  $path.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
  $path.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
  $path.CloseFigure()
  return $path
}

# ---------------------------------------------------------------- app icon
$size = 1024
$bmp = [System.Drawing.Bitmap]::new($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
$g.Clear([System.Drawing.Color]::Transparent)

$pad = 40
$side = $size - ($pad * 2)
$path = New-RoundedPath $pad $pad $side $side 210

$p1 = [System.Drawing.Point]::new($pad, $pad)
$p2 = [System.Drawing.Point]::new(($size - $pad), ($size - $pad))
$c1 = [System.Drawing.Color]::FromArgb(255, 34, 36, 42)
$c2 = [System.Drawing.Color]::FromArgb(255, 12, 13, 16)
$brush = [System.Drawing.Drawing2D.LinearGradientBrush]::new($p1, $p2, $c1, $c2)
$g.FillPath($brush, $path)

$hairline = [System.Drawing.Pen]::new([System.Drawing.Color]::FromArgb(30, 255, 255, 255), 4)
$g.DrawPath($hairline, $path)

# amber dot + soft halo
$cx = $size / 2.0
$cy = $size / 2.0
$dotR = 128.0
for ($i = 22; $i -ge 1; $i--) {
  $alpha = [int](2 + (30 * ($i / 22.0)))
  $hr = $dotR + ($i * 7)
  $halo = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb($alpha, 242, 171, 52))
  $g.FillEllipse($halo, ($cx - $hr), ($cy - $hr), ($hr * 2), ($hr * 2))
  $halo.Dispose()
}
$dot = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(255, 242, 171, 52))
$g.FillEllipse($dot, ($cx - $dotR), ($cy - $dotR), ($dotR * 2), ($dotR * 2))

$sheen = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(66, 255, 255, 255))
$g.FillEllipse($sheen, ($cx - 66), ($cy - 94), 118, 96)

$g.Dispose()
$iconOut = Join-Path $PSScriptRoot 'app-icon.png'
$bmp.Save($iconOut, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output "wrote $iconOut"

# ---------------------------------------------------------------- tray icon
$ts = 64
$tb = [System.Drawing.Bitmap]::new($ts, $ts, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$tg = [System.Drawing.Graphics]::FromImage($tb)
$tg.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
$tg.Clear([System.Drawing.Color]::Transparent)
for ($i = 10; $i -ge 1; $i--) {
  $alpha = [int](6 + (26 * ($i / 10.0)))
  $hr = 13 + ($i * 1.7)
  $h = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb($alpha, 242, 171, 52))
  $tg.FillEllipse($h, (32 - $hr), (32 - $hr), ($hr * 2), ($hr * 2))
  $h.Dispose()
}
$core = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(255, 242, 171, 52))
$tg.FillEllipse($core, 19, 19, 26, 26)
$tg.Dispose()
$trayOut = Join-Path $PSScriptRoot 'tray.png'
$tb.Save($trayOut, [System.Drawing.Imaging.ImageFormat]::Png)
$tb.Dispose()
Write-Output "wrote $trayOut"