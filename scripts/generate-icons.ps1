# Regenerate the Windows icons from the editable SVG sources. Windows PowerShell 5.1+
# and the .NET Framework WPF assemblies included with Windows are sufficient.
[CmdletBinding()]
param(
    [string]$IconDirectory = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName PresentationCore
Add-Type -AssemblyName PresentationFramework
Add-Type -AssemblyName WindowsBase

$culture = [System.Globalization.CultureInfo]::InvariantCulture
if ([string]::IsNullOrWhiteSpace($IconDirectory)) {
    $IconDirectory = Join-Path (Split-Path -Parent $PSScriptRoot) 'assets\icons'
}
$iconDirectoryPath = [System.IO.Path]::GetFullPath($IconDirectory)
if (-not (Test-Path -LiteralPath $iconDirectoryPath -PathType Container)) {
    throw "Icon source directory does not exist: $iconDirectoryPath"
}

function Read-Attribute {
    param([System.Xml.XmlElement]$Node, [string]$Name, [string]$Default = '')
    if ($Node.HasAttribute($Name)) { return $Node.GetAttribute($Name) }
    return $Default
}

function Convert-Number {
    param([string]$Value)
    return [double]::Parse($Value, $culture)
}

function Convert-Coordinate {
    param([string]$Value)
    if ($Value.EndsWith('%')) {
        return (Convert-Number $Value.Substring(0, $Value.Length - 1)) / 100.0
    }
    return Convert-Number $Value
}

function New-SvgBrush {
    param([string]$Paint, [hashtable]$Gradients, [double]$Opacity = 1.0)
    if ($Paint -eq 'none') { return $null }
    if ($Paint -match '^url\(#([^\)]+)\)$') {
        if (-not $Gradients.ContainsKey($Matches[1])) {
            throw "Unknown SVG gradient: $Paint"
        }
        $brush = $Gradients[$Matches[1]].Clone()
    }
    else {
        $color = [System.Windows.Media.ColorConverter]::ConvertFromString($Paint)
        $brush = [System.Windows.Media.SolidColorBrush]::new($color)
    }
    $brush.Opacity = $brush.Opacity * $Opacity
    $brush.Freeze()
    return $brush
}

function Read-SvgDrawing {
    param([string]$Path)
    $settings = [System.Xml.XmlReaderSettings]::new()
    $settings.DtdProcessing = [System.Xml.DtdProcessing]::Prohibit
    $settings.XmlResolver = $null
    $reader = [System.Xml.XmlReader]::Create($Path, $settings)
    $document = [System.Xml.XmlDocument]::new()
    $document.XmlResolver = $null
    try { $document.Load($reader) } finally { $reader.Dispose() }
    $root = $document.DocumentElement
    $viewBox = (Read-Attribute $root 'viewBox') -split '[\s,]+'
    if ($viewBox.Count -ne 4) { throw "A four-number SVG viewBox is required: $Path" }
    $x = Convert-Number $viewBox[0]
    $y = Convert-Number $viewBox[1]
    $width = Convert-Number $viewBox[2]
    $height = Convert-Number $viewBox[3]
    if ($width -le 0 -or $height -le 0 -or $width -ne $height) {
        throw "Icon SVG viewBox must be square and positive: $Path"
    }

    $gradients = @{}
    foreach ($node in $root.SelectNodes('./*[local-name()="defs"]/*[local-name()="linearGradient"]')) {
        $gradient = [System.Windows.Media.LinearGradientBrush]::new()
        $gradient.StartPoint = [System.Windows.Point]::new(
            (Convert-Coordinate (Read-Attribute $node 'x1' '0%')),
            (Convert-Coordinate (Read-Attribute $node 'y1' '0%')))
        $gradient.EndPoint = [System.Windows.Point]::new(
            (Convert-Coordinate (Read-Attribute $node 'x2' '100%')),
            (Convert-Coordinate (Read-Attribute $node 'y2' '0%')))
        if ((Read-Attribute $node 'gradientUnits') -eq 'userSpaceOnUse') {
            $gradient.MappingMode = [System.Windows.Media.BrushMappingMode]::Absolute
        }
        foreach ($stop in $node.SelectNodes('./*[local-name()="stop"]')) {
            $color = [System.Windows.Media.ColorConverter]::ConvertFromString(
                (Read-Attribute $stop 'stop-color' '#000000'))
            $color.A = [byte][Math]::Round(255 * (Convert-Number (Read-Attribute $stop 'stop-opacity' '1')))
            $gradient.GradientStops.Add([System.Windows.Media.GradientStop]::new(
                $color, (Convert-Coordinate (Read-Attribute $stop 'offset' '0'))))
        }
        $gradients[(Read-Attribute $node 'id')] = $gradient
    }

    $drawing = [System.Windows.Media.DrawingGroup]::new()
    $context = $drawing.Open()
    try {
        foreach ($node in $root.ChildNodes) {
            if ($node -isnot [System.Xml.XmlElement]) { continue }
            if ($node.LocalName -in @('defs', 'title', 'desc', 'metadata')) { continue }
            if ($node.LocalName -ne 'path') {
                throw "Unsupported SVG element '$($node.LocalName)' in $Path; use flat paths."
            }
            if ($node.HasAttribute('transform') -or $node.HasAttribute('style')) {
                throw "SVG transforms and inline styles must be flattened to path attributes: $Path"
            }
            $geometry = [System.Windows.Media.Geometry]::Parse((Read-Attribute $node 'd')).Clone()
            if ($geometry -is [System.Windows.Media.StreamGeometry] -or
                $geometry -is [System.Windows.Media.PathGeometry]) {
                $geometry.FillRule = [System.Windows.Media.FillRule]::Nonzero
                if ((Read-Attribute $node 'fill-rule') -eq 'evenodd') {
                    $geometry.FillRule = [System.Windows.Media.FillRule]::EvenOdd
                }
            }
            $opacity = Convert-Number (Read-Attribute $node 'opacity' '1')
            $fill = New-SvgBrush (Read-Attribute $node 'fill' '#000000') $gradients (
                $opacity * (Convert-Number (Read-Attribute $node 'fill-opacity' '1')))
            $stroke = New-SvgBrush (Read-Attribute $node 'stroke' 'none') $gradients (
                $opacity * (Convert-Number (Read-Attribute $node 'stroke-opacity' '1')))
            $pen = $null
            if ($null -ne $stroke) {
                $pen = [System.Windows.Media.Pen]::new($stroke,
                    (Convert-Number (Read-Attribute $node 'stroke-width' '1')))
                switch (Read-Attribute $node 'stroke-linecap' 'butt') {
                    'round' {
                        $pen.StartLineCap = [System.Windows.Media.PenLineCap]::Round
                        $pen.EndLineCap = [System.Windows.Media.PenLineCap]::Round
                    }
                    'square' {
                        $pen.StartLineCap = [System.Windows.Media.PenLineCap]::Square
                        $pen.EndLineCap = [System.Windows.Media.PenLineCap]::Square
                    }
                }
                switch (Read-Attribute $node 'stroke-linejoin' 'miter') {
                    'round' { $pen.LineJoin = [System.Windows.Media.PenLineJoin]::Round }
                    'bevel' { $pen.LineJoin = [System.Windows.Media.PenLineJoin]::Bevel }
                }
                $pen.Freeze()
            }
            $geometry.Freeze()
            $context.DrawGeometry($fill, $pen, $geometry)
        }
    }
    finally { $context.Close() }
    $drawing.Freeze()
    return [pscustomobject]@{ Drawing = $drawing; X = $x; Y = $y; Width = $width; Height = $height }
}

function Render-Svg {
    param($Source, [int]$Size)
    # Oversampling keeps curves and diagonal edges clean at notification-area sizes.
    $sampleSize = $Size * 4
    $visual = [System.Windows.Media.DrawingVisual]::new()
    $context = $visual.RenderOpen()
    try {
        $context.PushTransform([System.Windows.Media.ScaleTransform]::new(
            $sampleSize / $Source.Width, $sampleSize / $Source.Height))
        $context.PushTransform([System.Windows.Media.TranslateTransform]::new(-$Source.X, -$Source.Y))
        $context.DrawDrawing($Source.Drawing)
        $context.Pop()
        $context.Pop()
    }
    finally { $context.Close() }
    $sample = [System.Windows.Media.Imaging.RenderTargetBitmap]::new(
        $sampleSize, $sampleSize, 96, 96, [System.Windows.Media.PixelFormats]::Pbgra32)
    $sample.Render($visual)
    $sample.Freeze()

    $finalVisual = [System.Windows.Media.DrawingVisual]::new()
    [System.Windows.Media.RenderOptions]::SetBitmapScalingMode(
        $finalVisual, [System.Windows.Media.BitmapScalingMode]::HighQuality)
    $context = $finalVisual.RenderOpen()
    try { $context.DrawImage($sample, [System.Windows.Rect]::new(0, 0, $Size, $Size)) }
    finally { $context.Close() }
    $bitmap = [System.Windows.Media.Imaging.RenderTargetBitmap]::new(
        $Size, $Size, 96, 96, [System.Windows.Media.PixelFormats]::Pbgra32)
    $bitmap.Render($finalVisual)
    $bitmap.Freeze()
    return $bitmap
}

function Get-PngBytes {
    param([System.Windows.Media.Imaging.BitmapSource]$Bitmap)
    $encoder = [System.Windows.Media.Imaging.PngBitmapEncoder]::new()
    $encoder.Frames.Add([System.Windows.Media.Imaging.BitmapFrame]::Create($Bitmap))
    $stream = [System.IO.MemoryStream]::new()
    try {
        $encoder.Save($stream)
        return ,$stream.ToArray()
    }
    finally { $stream.Dispose() }
}

function Get-IconDib {
    param([System.Windows.Media.Imaging.BitmapSource]$Bitmap)
    $width = $Bitmap.PixelWidth
    $height = $Bitmap.PixelHeight
    # ICO DIBs use straight alpha; WPF's render target uses premultiplied alpha.
    $straight = [System.Windows.Media.Imaging.FormatConvertedBitmap]::new(
        $Bitmap, [System.Windows.Media.PixelFormats]::Bgra32, $null, 0)
    $stride = $width * 4
    $pixels = New-Object byte[] ($stride * $height)
    $straight.CopyPixels($pixels, $stride, 0)
    $maskStride = [int]([Math]::Ceiling($width / 32.0) * 4)
    $mask = New-Object byte[] ($maskStride * $height)
    for ($row = 0; $row -lt $height; $row++) {
        $sourceRow = $height - 1 - $row
        for ($column = 0; $column -lt $width; $column++) {
            if ($pixels[$sourceRow * $stride + $column * 4 + 3] -eq 0) {
                $index = $row * $maskStride + [int][Math]::Floor($column / 8.0)
                $mask[$index] = [byte]($mask[$index] -bor (0x80 -shr ($column % 8)))
            }
        }
    }
    $stream = [System.IO.MemoryStream]::new()
    $writer = [System.IO.BinaryWriter]::new($stream)
    try {
        $writer.Write([uint32]40)
        $writer.Write([int32]$width)
        $writer.Write([int32]($height * 2))
        $writer.Write([uint16]1)
        $writer.Write([uint16]32)
        $writer.Write([uint32]0)
        $writer.Write([uint32]($stride * $height))
        $writer.Write([int32]0)
        $writer.Write([int32]0)
        $writer.Write([uint32]0)
        $writer.Write([uint32]0)
        for ($row = $height - 1; $row -ge 0; $row--) {
            $writer.Write($pixels, $row * $stride, $stride)
        }
        $writer.Write($mask)
        $writer.Flush()
        return ,$stream.ToArray()
    }
    finally { $writer.Dispose(); $stream.Dispose() }
}

function Write-Ico {
    param([string]$Path, [System.Collections.IDictionary]$Bitmaps, [int[]]$Sizes)
    $frames = [System.Collections.Generic.List[byte[]]]::new()
    foreach ($size in $Sizes) {
        if ($size -ge 256) { $frames.Add((Get-PngBytes $Bitmaps[$size])) }
        else { $frames.Add((Get-IconDib $Bitmaps[$size])) }
    }
    $stream = [System.IO.File]::Create($Path)
    $writer = [System.IO.BinaryWriter]::new($stream)
    try {
        $writer.Write([uint16]0)
        $writer.Write([uint16]1)
        $writer.Write([uint16]$Sizes.Count)
        $offset = 6 + 16 * $Sizes.Count
        for ($index = 0; $index -lt $Sizes.Count; $index++) {
            $dimension = $Sizes[$index]
            if ($dimension -eq 256) { $dimension = 0 }
            $writer.Write([byte]$dimension)
            $writer.Write([byte]$dimension)
            $writer.Write([byte]0)
            $writer.Write([byte]0)
            $writer.Write([uint16]1)
            $writer.Write([uint16]32)
            $writer.Write([uint32]$frames[$index].Length)
            $writer.Write([uint32]$offset)
            $offset += $frames[$index].Length
        }
        foreach ($frame in $frames) { $writer.Write($frame) }
    }
    finally { $writer.Dispose(); $stream.Dispose() }
}

$appSizes = @(16, 20, 24, 32, 40, 48, 64, 128, 256)
$traySizes = @(16, 20, 24, 32, 40, 48)
$sources = @('bucket-mount')
foreach ($theme in @('dark', 'light')) {
    foreach ($state in @('idle', 'syncing', 'paused', 'error')) {
        $sources += "tray-on-$theme-$state"
    }
}

foreach ($name in $sources) {
    $sourcePath = Join-Path $iconDirectoryPath "$name.svg"
    if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
        throw "Missing SVG icon source: $sourcePath"
    }
    $source = Read-SvgDrawing $sourcePath
    $sizes = $traySizes
    if ($name -eq 'bucket-mount') { $sizes = $appSizes }
    $bitmaps = @{}
    foreach ($size in $sizes) {
        $bitmaps[$size] = Render-Svg $source $size
    }
    Write-Ico (Join-Path $iconDirectoryPath "$name.ico") $bitmaps $sizes
    Write-Output "Generated $name.ico."
}
