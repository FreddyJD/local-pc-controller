# Regenerate the checked-in native icons. Windows-only build-time tooling; no runtime dependency.
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
Add-Type -AssemblyName System.Drawing
$out = Join-Path (Get-Location) 'packaging\icons'
$null = New-Item -ItemType Directory -Path $out -Force

function Round-Rect([single]$x, [single]$y, [single]$size, [single]$radius) {
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = 2 * $radius
    $path.AddArc($x,$y,$d,$d,180,90)
    $path.AddArc($x+$size-$d,$y,$d,$d,270,90)
    $path.AddArc($x+$size-$d,$y+$size-$d,$d,$d,0,90)
    $path.AddArc($x,$y+$size-$d,$d,$d,90,90)
    $path.CloseFigure()
    return ,$path
}

function Draw-Icon([int]$size, [bool]$mark = $false) {
    $factor = if ($size -le 256) { 4 } else { 2 }
    $canvas = New-Object System.Drawing.Bitmap ($size*$factor),($size*$factor),([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($canvas)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.ScaleTransform(($size*$factor/1024.0),($size*$factor/1024.0))
    $white = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(250,250,252))
    $ink = [System.Drawing.Color]::FromArgb(37,38,44)
    if (!$mark) {
        $shadow = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(31,0,0,0))
        $path = Round-Rect 80 92 864 195
        $g.FillPath($shadow,$path)
        $path.Dispose(); $shadow.Dispose()
        $path = Round-Rect 80 80 864 195
        $gradient = New-Object System.Drawing.Drawing2D.LinearGradientBrush ([System.Drawing.PointF]::new(150,70)),([System.Drawing.PointF]::new(850,950)),([System.Drawing.Color]::FromArgb(54,55,64)),([System.Drawing.Color]::FromArgb(25,26,32))
        $g.FillPath($gradient,$path)
        $border = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(85,86,95)),2
        $g.DrawPath($border,$path)
        $border.Dispose(); $gradient.Dispose(); $path.Dispose()
    }
    $radius = if ($mark) { 424.0 } else { 292.0 }
    $g.FillEllipse($white,([single](512-$radius)),([single](512-$radius)),([single](2*$radius)),([single](2*$radius)))
    $scale = $radius/53.0
    $pen = New-Object System.Drawing.Pen $ink,([single](13*$scale))
    $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Square
    $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Square
    foreach($point in @(@(61,24),@(101,49),@(91,96),@(40,98),@(26,48))) {
        $g.DrawLine($pen,[single]512,[single]512,[single](512+($point[0]-64)*$scale),[single](512+($point[1]-64)*$scale))
    }
    $pen.Dispose(); $white.Dispose(); $g.Dispose()
    $image = New-Object System.Drawing.Bitmap $size,$size,([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($image)
    $g.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.DrawImage($canvas,0,0,$size,$size)
    $g.Dispose(); $canvas.Dispose()
    return ,$image
}

$images = @{}
foreach($size in @(16,24,32,48,64,128,256,512,1024)) {
    $image = Draw-Icon $size
    $stream = New-Object System.IO.MemoryStream
    $image.Save($stream,[System.Drawing.Imaging.ImageFormat]::Png)
    $images[$size] = $stream.ToArray()
    [System.IO.File]::WriteAllBytes((Join-Path $out "icon-$size.png"),$images[$size])
    $image.Dispose(); $stream.Dispose()
}
$mark = Draw-Icon 128 $true
$mark.Save((Join-Path $out 'mark.png'),[System.Drawing.Imaging.ImageFormat]::Png)
$mark.Dispose()

$stream = New-Object System.IO.MemoryStream
$writer = New-Object System.IO.BinaryWriter $stream
$sizes = @(16,24,32,48,64,128,256)
$writer.Write([uint16]0); $writer.Write([uint16]1); $writer.Write([uint16]$sizes.Count)
$offset = 6+16*$sizes.Count
foreach($size in $sizes) {
    $edge = if($size -eq 256){0}else{$size}
    $writer.Write([byte]$edge); $writer.Write([byte]$edge); $writer.Write([byte]0); $writer.Write([byte]0)
    $writer.Write([uint16]1); $writer.Write([uint16]32)
    $writer.Write([uint32]$images[$size].Length); $writer.Write([uint32]$offset)
    $offset += $images[$size].Length
}
foreach($size in $sizes) { $writer.Write([byte[]]$images[$size]) }
[System.IO.File]::WriteAllBytes((Join-Path $out 'localnet.ico'),$stream.ToArray())
$writer.Dispose(); $stream.Dispose()

function Write-BigEndian($writer,[uint32]$value) {
    $bytes = [BitConverter]::GetBytes($value)
    if([BitConverter]::IsLittleEndian) { [Array]::Reverse($bytes) }
    $writer.Write($bytes)
}
$chunks = @(@('icp4',16),@('icp5',32),@('icp6',64),@('ic07',128),@('ic08',256),@('ic09',512),@('ic10',1024),@('ic11',32),@('ic12',64),@('ic13',256),@('ic14',512))
$length = 8
foreach($chunk in $chunks){ $length += 8+$images[[int]$chunk[1]].Length }
$stream = New-Object System.IO.MemoryStream
$writer = New-Object System.IO.BinaryWriter $stream
$writer.Write([System.Text.Encoding]::ASCII.GetBytes('icns')); Write-BigEndian $writer $length
foreach($chunk in $chunks) {
    $png = $images[[int]$chunk[1]]
    $writer.Write([System.Text.Encoding]::ASCII.GetBytes($chunk[0]))
    Write-BigEndian $writer (8+$png.Length)
    $writer.Write([byte[]]$png)
}
[System.IO.File]::WriteAllBytes((Join-Path $out 'localnet.icns'),$stream.ToArray())
$writer.Dispose(); $stream.Dispose()
Write-Output 'Generated Localnet PNG, ICO, and ICNS icons.'
