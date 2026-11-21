# Build the icon of the Start menu shortcut from the application artwork.
param(
    [Parameter(Mandatory)][string]$Source,
    [Parameter(Mandatory)][string]$Destination
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.Drawing

# Sizes Windows asks for at 100% to 300% scaling, and the large Start tile.
$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$artwork = [System.Drawing.Image]::FromFile((Resolve-Path -LiteralPath $Source).Path)
$images = [System.Collections.Generic.List[byte[]]]::new()
try {
    foreach ($size in $sizes) {
        $bitmap = [System.Drawing.Bitmap]::new($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $edges = [System.Drawing.Imaging.ImageAttributes]::new()
        try {
            $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
            $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
            $graphics.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
            # Mirror the artwork past its border so the edge pixels keep their colour.
            $edges.SetWrapMode([System.Drawing.Drawing2D.WrapMode]::TileFlipXY)
            $target = [System.Drawing.Rectangle]::new(0, 0, $size, $size)
            $graphics.DrawImage($artwork, $target, 0, 0, $artwork.Width, $artwork.Height, [System.Drawing.GraphicsUnit]::Pixel, $edges)
            $stream = [System.IO.MemoryStream]::new()
            try {
                if ($size -eq 256) {
                    # Windows reads the largest image as PNG.
                    $bitmap.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
                } else {
                    # A device-independent bitmap: header, rows from the bottom, then an empty mask.
                    $writer = [System.IO.BinaryWriter]::new($stream)
                    $writer.Write([uint32]40)
                    $writer.Write([int32]$size)
                    $writer.Write([int32]($size * 2))
                    $writer.Write([uint16]1)
                    $writer.Write([uint16]32)
                    $writer.Write([byte[]]::new(24))
                    $area = [System.Drawing.Rectangle]::new(0, 0, $size, $size)
                    $data = $bitmap.LockBits($area, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
                    try {
                        $row = [byte[]]::new($size * 4)
                        for ($y = $size - 1; $y -ge 0; $y--) {
                            [System.Runtime.InteropServices.Marshal]::Copy([IntPtr]::Add($data.Scan0, $y * $data.Stride), $row, 0, $row.Length)
                            $writer.Write($row)
                        }
                    } finally { $bitmap.UnlockBits($data) }
                    $maskRow = [int][Math]::Ceiling($size / 32) * 4
                    $writer.Write([byte[]]::new($maskRow * $size))
                    $writer.Flush()
                }
                $images.Add($stream.ToArray())
            } finally { $stream.Dispose() }
        } finally {
            $edges.Dispose()
            $graphics.Dispose()
            $bitmap.Dispose()
        }
    }
} finally { $artwork.Dispose() }

# .NET resolves a relative path against the process directory, not this location.
$Destination = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Destination)
New-Item -ItemType Directory -Force (Split-Path -Parent $Destination) | Out-Null
$file = [System.IO.File]::Create($Destination)
try {
    $writer = [System.IO.BinaryWriter]::new($file)
    $writer.Write([uint16]0)
    $writer.Write([uint16]1)
    $writer.Write([uint16]$sizes.Count)
    $offset = 6 + 16 * $sizes.Count
    for ($index = 0; $index -lt $sizes.Count; $index++) {
        # A width or height of 256 is written as zero.
        $edge = [byte]($sizes[$index] % 256)
        $writer.Write($edge)
        $writer.Write($edge)
        $writer.Write([byte]0)
        $writer.Write([byte]0)
        $writer.Write([uint16]1)
        $writer.Write([uint16]32)
        $writer.Write([uint32]$images[$index].Length)
        $writer.Write([uint32]$offset)
        $offset += $images[$index].Length
    }
    foreach ($image in $images) { $writer.Write($image) }
    $writer.Flush()
} finally { $file.Dispose() }
