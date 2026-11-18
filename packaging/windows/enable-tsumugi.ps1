param([Parameter(Mandatory)][string]$Model)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$location = Join-Path $env:ProgramW6432 'Kanade'
$engine = Join-Path $location 'kanade-engine.exe'
$ui = Join-Path $location 'kanade-ui.exe'
if (-not (Test-Path -LiteralPath $engine -PathType Leaf)) { throw 'The Kanade installation is missing.' }
$modelPath = (Resolve-Path -LiteralPath $Model).Path

function Get-State {
    $ErrorActionPreference = 'Continue'
    $json = & $ui models status 2>$null
    if ($LASTEXITCODE -eq 0) { return ($json | Out-String | ConvertFrom-Json) }
}

& (Join-Path $PSScriptRoot 'start-engine.ps1')
$deadline = [DateTime]::UtcNow.AddSeconds(40)
do {
    $state = Get-State
    if ($state -and $state.state -ne 'loading') { break }
    Start-Sleep -Milliseconds 400
} while ([DateTime]::UtcNow -lt $deadline)
if (@($state.catalog).Count -ne 1 -or $state.catalog[0].id -ne 'tsumugi-s-f16') {
    throw 'The installed engine did not report the fixed Tsumugi model.'
}
$fixed = $state.catalog[0]
$hash = [Security.Cryptography.SHA256]::Create()
$stream = [IO.File]::OpenRead($modelPath)
try { $digest = [BitConverter]::ToString($hash.ComputeHash($stream)).Replace('-', '') }
finally { $stream.Dispose(); $hash.Dispose() }
if ($digest -ne $fixed.sha256) {
    throw 'The supplied GGUF does not match the installed Tsumugi worker.'
}
if (-not ($state.ready -and $state.installed.id -eq $fixed.id)) {
    $response = & $ui models import $modelPath --accept-license --accept-local-processing
    if ($LASTEXITCODE -ne 0) { throw 'Tsumugi import failed.' }
    $deadline = [DateTime]::UtcNow.AddMinutes(3)
    do {
        Start-Sleep -Milliseconds 400
        $state = Get-State
        if ($state -and $state.job.phase -eq 'failed') { throw "Tsumugi qualification failed ($($state.job.error))." }
        if ($state.ready -and $state.installed.id -eq $fixed.id) { break }
    } while ([DateTime]::UtcNow -lt $deadline)
}
if (-not $state.ready) { throw 'Kanade did not confirm Tsumugi qualification.' }
if (-not $state.enabled) {
    $raw = & $ui settings
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read the installed settings.' }
    $settings = $raw | Out-String | ConvertFrom-Json
    $settings.llm = $true
    $temp = [IO.Path]::GetTempFileName()
    try {
        [IO.File]::WriteAllText($temp, ($settings | ConvertTo-Json -Depth 10), [Text.UTF8Encoding]::new($false))
        $null = & $ui settings set $temp
        if ($LASTEXITCODE -ne 0) { throw 'The engine refused to enable Tsumugi.' }
    } finally { Remove-Item -LiteralPath $temp }
}
$state = Get-State
if (-not ($state.ready -and $state.enabled -and $state.installed.id -eq $fixed.id)) {
    throw 'The installed engine did not confirm activation.'
}
Write-Output 'Tsumugi is qualified and enabled.'
