$ErrorActionPreference = 'Stop'
$engine = Get-Item -LiteralPath (Join-Path $PSScriptRoot 'kanade-engine.exe')
$sessionId = (Get-Process -Id $PID).SessionId
$running = @(Get-Process -Name kanade-engine -ErrorAction SilentlyContinue | Where-Object {
    $_.SessionId -eq $sessionId -and $_.Path -eq $engine.FullName
})
foreach ($process in $running) {
    if ($process.StartTime.ToUniversalTime() -ge $engine.LastWriteTimeUtc) { return }
    # Windows Installer can replace the image while the previous engine still owns the pipe.
    if (-not $process.HasExited) {
        Stop-Process -InputObject $process
        if (-not $process.WaitForExit(6000)) { throw 'The previous Kanade engine did not stop.' }
    }
}
Start-Process -FilePath $engine.FullName -WindowStyle Hidden
