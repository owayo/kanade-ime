param([string]$EngineDirectory = $PSScriptRoot)
$ErrorActionPreference = 'Stop'
$engine = Join-Path $EngineDirectory 'kanade-engine.exe'
if (-not (Test-Path -LiteralPath $engine -PathType Leaf)) { return }
$engine = Get-Item -LiteralPath $engine
$sessionId = (Get-Process -Id $PID).SessionId

# An update can leave processes that were started from the previous image.
# The supervisor is the oldest of them. Stop it first, so that it cannot start
# the previous engine again.
$running = @(Get-Process -Name kanade-engine -ErrorAction SilentlyContinue | Where-Object {
    $_.SessionId -eq $sessionId -and $_.Path -eq $engine.FullName
} | Sort-Object StartTime)
foreach ($process in $running) {
    if ($process.StartTime.ToUniversalTime() -lt $engine.LastWriteTimeUtc -and -not $process.HasExited) {
        Stop-Process -InputObject $process
        if (-not $process.WaitForExit(6000)) { throw 'The previous Kanade engine did not stop.' }
    }
}

# The supervisor starts the engine and starts it again whenever it ends. It is
# the engine's own executable, so nothing here stays running. A second
# supervisor for the same user and session exits at once.
Start-Process -FilePath $engine.FullName -ArgumentList '--supervise'
