# Add the Windows settings of a manifest file to the manifest embedded in a program.
# A program holds one manifest and GPUI embeds its own while linking, so Kanade's
# settings are merged into that one after the build and before signing.
param(
    [Parameter(Mandatory)][string]$Program,
    [Parameter(Mandatory)][string]$Manifest
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# mt.exe belongs to the Windows SDK, which the MSVC build already requires.
function Find-ManifestTool {
    $onPath = Get-Command mt.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($onPath) { return $onPath.Source }
    foreach ($key in 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows Kits\Installed Roots',
        'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots') {
        $roots = Get-ItemProperty -LiteralPath $key -ErrorAction SilentlyContinue
        if (-not $roots -or -not $roots.PSObject.Properties['KitsRoot10']) { continue }
        $tools = Join-Path $roots.KitsRoot10 'bin'
        if (-not (Test-Path -LiteralPath $tools)) { continue }
        $tool = Get-ChildItem -LiteralPath $tools -Directory |
            Where-Object { $_.Name -match '^\d+(\.\d+){3}$' } |
            Sort-Object { [version]$_.Name } -Descending |
            ForEach-Object { Join-Path $_.FullName 'x64\mt.exe' } |
            Where-Object { Test-Path -LiteralPath $_ } |
            Select-Object -First 1
        if ($tool) { return $tool }
    }
    throw 'mt.exe of the Windows SDK was not found'
}

# The settings under application/windowsSettings, by element name.
function Get-WindowsSettings([xml]$Document) {
    $settings = @{}
    $path = "/*[local-name()='assembly']" +
        "/*[local-name()='application' and namespace-uri()='urn:schemas-microsoft-com:asm.v3']" +
        "/*[local-name()='windowsSettings']/*"
    foreach ($node in $Document.SelectNodes($path)) { $settings[$node.LocalName] = $node.InnerText.Trim() }
    $settings
}

function Read-EmbeddedSettings([string]$Tool, [string]$Image) {
    $file = Join-Path ([System.IO.Path]::GetTempPath()) ('kanade-manifest-' + [System.IO.Path]::GetRandomFileName())
    try {
        & $Tool -nologo "-inputresource:$Image;#1" "-out:$file" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "The program has no manifest to extend ($LASTEXITCODE)" }
        $document = [xml]::new()
        $document.Load($file)
        Get-WindowsSettings $document
    } finally {
        Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue
    }
}

$image = (Resolve-Path -LiteralPath $Program).Path
$addition = (Resolve-Path -LiteralPath $Manifest).Path
$document = [xml]::new()
$document.Load($addition)
$wanted = Get-WindowsSettings $document
if ($wanted.Count -eq 0) { throw 'The manifest file adds no Windows setting' }

$tool = Find-ManifestTool
$before = Read-EmbeddedSettings $tool $image
# A second run on the same program has nothing to add.
$pending = @($wanted.Keys | Where-Object { $before[$_] -ne $wanted[$_] })
if ($pending.Count -gt 0) {
    & $tool -nologo -manifest $addition "-inputresource:$image;#1" "-outputresource:$image;#1" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "mt.exe could not merge the manifest ($LASTEXITCODE)" }
}

# The merge must add every setting and keep those the program already had,
# such as its DPI awareness.
$after = Read-EmbeddedSettings $tool $image
foreach ($name in $wanted.Keys) {
    if ($after[$name] -ne $wanted[$name]) { throw "The merged manifest lacks the setting $name" }
}
foreach ($name in $before.Keys) {
    if ($pending -notcontains $name -and $after[$name] -ne $before[$name]) { throw "The merge changed the setting $name" }
}
