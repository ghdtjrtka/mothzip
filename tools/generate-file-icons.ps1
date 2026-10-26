param([string]$Workspace = (Split-Path -Parent $PSScriptRoot))
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# The Rust catalog also drives the resource compiler and Windows registration.
$catalog = [IO.File]::ReadAllText((Join-Path $Workspace 'src\core\file_types.rs'))
$list = [regex]::Match($catalog, 'SUPPORTED_EXTENSIONS:[\s\S]*?=\s*&\[([\s\S]*?)\];').Groups[1].Value
if (-not $list) { throw 'Supported extension catalog not found' }
$extensions = @([regex]::Matches($list, '"(\.[^"]+)"') | ForEach-Object { $_.Groups[1].Value })
$output = Join-Path $Workspace 'assets\file-icons'
[IO.Directory]::CreateDirectory($output) | Out-Null
$moth = [Drawing.Image]::FromFile((Join-Path $Workspace 'assets\moth_icon.png'))
$cocoon = [Drawing.Image]::FromFile((Join-Path $Workspace 'assets\cocoon_icon.png'))
$sizes = @(16,20,24,32,40,48,64,96,128,256)
$ink = [Drawing.ColorTranslator]::FromHtml('#3B2922')
$gold = [Drawing.ColorTranslator]::FromHtml('#DDA73E')
$cream = [Drawing.ColorTranslator]::FromHtml('#FFF4D8')

# A 3x5 pixel alphabet keeps the format label legible at Windows' 16px list size.
$glyphs = @{
 'A'=@('010','101','111','101','101'); 'B'=@('110','101','110','101','110')
 'C'=@('111','100','100','100','111'); 'G'=@('111','100','101','101','111')
 'I'=@('111','010','010','010','111'); 'L'=@('100','100','100','100','111')
 'P'=@('110','101','110','100','100'); 'R'=@('110','101','110','101','101')
 'S'=@('111','100','111','001','111'); 'T'=@('111','010','010','010','010')
 'X'=@('101','101','010','101','101'); 'Z'=@('111','001','010','100','111')
 '4'=@('101','101','111','001','001'); '7'=@('111','001','010','010','010')
}

function Paint-Icon([string]$Label,[int]$Size) {
    $image = [Drawing.Bitmap]::new($Size,$Size,[Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [Drawing.Graphics]::FromImage($image)
    try {
        $graphics.Clear([Drawing.Color]::Transparent)
        $graphics.CompositingQuality = [Drawing.Drawing2D.CompositingQuality]::HighQuality
        $graphics.InterpolationMode = [Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.SmoothingMode = [Drawing.Drawing2D.SmoothingMode]::AntiAlias
        $compound = $Label.Contains('.')
        $smallLabel = if($compound) { $Label.Split('.')[-1] } else { $Label }
        if (-not $Label) {
            $graphics.DrawImage($moth,[Drawing.Rectangle]::new(0,0,$Size,$Size))
        } elseif ($Size -le 24) {
            $footerHeight = if($Size -le 20) {7} else {8}
            $bodyHeight = $Size - $footerHeight + 1
            $graphics.DrawImage($cocoon,[Drawing.RectangleF]::new(0,-1,$Size,$bodyHeight+1))
            $brush = [Drawing.SolidBrush]::new($ink)
            $graphics.FillRectangle($brush,0,$Size-$footerHeight,$Size,$footerHeight);$brush.Dispose()
            $graphics.SmoothingMode = [Drawing.Drawing2D.SmoothingMode]::None
            $brush = [Drawing.SolidBrush]::new($cream)
            $width = $smallLabel.Length*4-1
            $left = [Math]::Floor(($Size-$width)/2)
            $top = $Size-$footerHeight+[Math]::Floor(($footerHeight-5)/2)
            for($character=0;$character -lt $smallLabel.Length;$character++) {
                $rows = $glyphs[[string]$smallLabel[$character]]
                if (-not $rows) { throw "Missing pixel glyph: $($smallLabel[$character])" }
                for($y=0;$y -lt 5;$y++) {for($x=0;$x -lt 3;$x++) {
                    if($rows[$y][$x] -eq '1') {$graphics.FillRectangle($brush,$left+$character*4+$x,$top+$y,1,1)}
                }}
            }
            $brush.Dispose()
        } else {
            $footerHeight = [Math]::Ceiling($Size * $(if($compound){0.31}else{0.23}))
            $footerTop = $Size-$footerHeight-1
            $graphics.DrawImage($cocoon,[Drawing.RectangleF]::new(0,-$Size*0.025,$Size,$footerTop+$Size*0.06))
            $radius = $footerHeight*0.15
            $rectangle = [Drawing.RectangleF]::new(1,$footerTop,$Size-2,$footerHeight)
            $path = [Drawing.Drawing2D.GraphicsPath]::new()
            $diameter = $radius*2
            $path.AddArc($rectangle.X,$rectangle.Y,$diameter,$diameter,180,90)
            $path.AddArc($rectangle.Right-$diameter,$rectangle.Y,$diameter,$diameter,270,90)
            $path.AddArc($rectangle.Right-$diameter,$rectangle.Bottom-$diameter,$diameter,$diameter,0,90)
            $path.AddArc($rectangle.X,$rectangle.Bottom-$diameter,$diameter,$diameter,90,90);$path.CloseFigure()
            $brush = [Drawing.SolidBrush]::new($ink);$graphics.FillPath($brush,$path);$brush.Dispose()
            $pen = [Drawing.Pen]::new($gold,[Math]::Max(1,$Size/128));$graphics.DrawPath($pen,$path);$pen.Dispose();$path.Dispose()
            $text = if($compound){$Label.Replace('.',"`n")}else{$Label}
            $fontSize = $footerHeight * $(if($compound){0.42}else{0.72})
            $font = [Drawing.Font]::new('Segoe UI',$fontSize,[Drawing.FontStyle]::Bold,[Drawing.GraphicsUnit]::Pixel)
            $format = [Drawing.StringFormat]::new();$format.Alignment='Center';$format.LineAlignment='Center'
            $format.FormatFlags=[Drawing.StringFormatFlags]::NoClip
            $graphics.TextRenderingHint = [Drawing.Text.TextRenderingHint]::AntiAliasGridFit
            $brush = [Drawing.SolidBrush]::new($cream)
            $graphics.DrawString($text,$font,$brush,$rectangle,$format)
            $brush.Dispose();$font.Dispose();$format.Dispose()
        }
    } finally { $graphics.Dispose() }
    return ,$image
}

function Write-Icon([string]$Name,[string]$Label,[switch]$App) {
    $frames = @()
    foreach($size in $sizes) {
        $image = Paint-Icon $Label $size
        $stream = [IO.MemoryStream]::new()
        $image.Save($stream,[Drawing.Imaging.ImageFormat]::Png)
        $frames += ,$stream.ToArray()
        if($size -eq 256 -and -not $App) {$image.Save((Join-Path $output ($Name+'.png')),[Drawing.Imaging.ImageFormat]::Png)}
        $stream.Dispose();$image.Dispose()
    }
    $directory = if($App){Join-Path $Workspace 'assets'}else{$output}
    $stream = [IO.File]::Create((Join-Path $directory ($Name+'.ico')))
    $writer = [IO.BinaryWriter]::new($stream)
    try {
        $writer.Write([uint16]0);$writer.Write([uint16]1);$writer.Write([uint16]$sizes.Count)
        $offset = 6+16*$sizes.Count
        for($index=0;$index -lt $sizes.Count;$index++) {
            $dimension = if($sizes[$index] -eq 256){0}else{$sizes[$index]}
            $writer.Write([byte]$dimension);$writer.Write([byte]$dimension);$writer.Write([byte]0);$writer.Write([byte]0)
            $writer.Write([uint16]1);$writer.Write([uint16]32)
            $writer.Write([uint32]$frames[$index].Length);$writer.Write([uint32]$offset)
            $offset += $frames[$index].Length
        }
        foreach($frame in $frames){$writer.Write([byte[]]$frame)}
    } finally { $writer.Dispose();$stream.Dispose() }
    if($App){return}
    # Editable layout source references the same cocoon artwork as the app icon.
    $lines = @(if($Label.Contains('.')){$Label.Split('.')}else{$Label})
    $footer = if($lines.Count -gt 1){80}else{59}
    $top = 255-$footer
    $svg = '<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">' +
        '<image href="../cocoon_icon.png" x="0" y="-6" width="256" height="'+($top+15)+'" preserveAspectRatio="none"/>' +
        '<rect x="1" y="'+$top+'" width="254" height="'+$footer+'" rx="9" fill="#3B2922" stroke="#DDA73E" stroke-width="2"/>'
    for($i=0;$i -lt $lines.Count;$i++) {
        $font = if($lines.Count -gt 1){32}else{42}
        $y = $top+($footer/$lines.Count)*($i+0.5)
        $svg += '<text x="128" y="'+$y+'" dominant-baseline="central" text-anchor="middle" font-family="Segoe UI,sans-serif" font-size="'+$font+'" font-weight="700" fill="#FFF4D8">'+$lines[$i]+'</text>'
    }
    [IO.File]::WriteAllText((Join-Path $output ($Name+'.svg')),$svg+'</svg>')
}

try {
    Write-Icon 'moth' '' -App
    Write-Icon 'archive' 'ARC'
    foreach($extension in $extensions){$name=$extension.TrimStart('.');Write-Icon $name $name.ToUpperInvariant()}
    $columns = 5;$cell = 156;$rows = [int][Math]::Ceiling($extensions.Count/$columns)
    $preview = [Drawing.Bitmap]::new($columns*$cell,$rows*($cell+30))
    $graphics = [Drawing.Graphics]::FromImage($preview);$graphics.Clear([Drawing.ColorTranslator]::FromHtml('#F3F1ED'))
    $font = [Drawing.Font]::new('Segoe UI',13,[Drawing.FontStyle]::Regular,[Drawing.GraphicsUnit]::Pixel)
    $brush = [Drawing.SolidBrush]::new([Drawing.ColorTranslator]::FromHtml('#483B32'))
    $format = [Drawing.StringFormat]::new();$format.Alignment='Center'
    for($index=0;$index -lt $extensions.Count;$index++) {
        $x=($index%$columns)*$cell;$y=[Math]::Floor($index/$columns)*($cell+30)
        $image=Paint-Icon ($extensions[$index].TrimStart('.').ToUpperInvariant()) 128
        $graphics.DrawImage($image,$x+14,$y+10,128,128);$image.Dispose()
        $graphics.DrawString($extensions[$index],$font,$brush,[Drawing.RectangleF]::new($x,$y+144,$cell,26),$format)
    }
    $preview.Save((Join-Path $output 'preview.png'),[Drawing.Imaging.ImageFormat]::Png)
    $format.Dispose();$brush.Dispose();$font.Dispose();$graphics.Dispose();$preview.Dispose()
} finally {$moth.Dispose();$cocoon.Dispose()}
Write-Output "Generated $($extensions.Count) format icons plus a generic archive icon."
