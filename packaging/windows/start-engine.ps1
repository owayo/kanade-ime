param([switch]$Watch, [string]$EngineDirectory = $PSScriptRoot)
$ErrorActionPreference = 'Stop'
$engine = Join-Path $EngineDirectory 'kanade-engine.exe'
if (-not (Test-Path -LiteralPath $engine -PathType Leaf)) { return }
$engine = Get-Item -LiteralPath $engine
$sessionId = (Get-Process -Id $PID).SessionId

function Get-SessionEngine {
    Get-Process -Name kanade-engine -ErrorAction SilentlyContinue | Where-Object {
        $_.SessionId -eq $sessionId -and $_.Path -eq $engine.FullName
    } | Select-Object -First 1
}

if (-not $Watch) {
    # An explicit setup after an MSI upgrade may need to retire the old image.
    $running = Get-SessionEngine
    if ($running -and $running.StartTime.ToUniversalTime() -lt $engine.LastWriteTimeUtc -and -not $running.HasExited) {
        Stop-Process -InputObject $running
        if (-not $running.WaitForExit(6000)) { throw 'The previous Kanade engine did not stop.' }
    }
    $shell = Join-Path ([Environment]::SystemDirectory) 'WindowsPowerShell\v1.0\powershell.exe'
    $arguments = '-NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy RemoteSigned -File "{0}" -Watch -EngineDirectory "{1}"' -f $PSCommandPath, $engine.DirectoryName
    Start-Process -FilePath $shell -ArgumentList $arguments -WindowStyle Hidden
    return
}

# Local mutexes are session scoped. Include the user and install path so that a
# second setup attaches to the same supervisor and isolated installations coexist.
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$hash = [Security.Cryptography.SHA256]::Create()
try { $pathId = [BitConverter]::ToString($hash.ComputeHash([Text.Encoding]::UTF8.GetBytes($engine.FullName.ToUpperInvariant()))) }
finally { $hash.Dispose() }
$created = $false
$guard = [Threading.Mutex]::new($false, "Local\Kanade.EngineWatch.$sid.$pathId", [ref]$created)
try {
    if (-not $created) { return }
    $delay = 1
    while (Test-Path -LiteralPath $engine.FullName -PathType Leaf) {
        $process = Get-SessionEngine
        $uptime = [Diagnostics.Stopwatch]::StartNew()
        try {
            if (-not $process) {
                $process = Start-Process -FilePath $engine.FullName -WindowStyle Hidden -PassThru
            }
            $process.WaitForExit()
        } catch [System.ComponentModel.Win32Exception], [System.InvalidOperationException] {
            # The image may be replaced by the installer or the process may have already exited.
        } finally {
            if ($process) { $process.Dispose() }
        }
        if ($uptime.Elapsed.TotalSeconds -ge 30) { $delay = 1 }
        Start-Sleep -Seconds $delay
        $delay = [Math]::Min(30, 2 * $delay)
    }
} finally { $guard.Dispose() }
