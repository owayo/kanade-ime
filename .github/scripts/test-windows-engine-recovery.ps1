# Exercise the setup launcher with a disposable image in this test's own
# directory. The supervisor itself is tested with the real engine by
# `cargo test -p kanade-engine --test windows_supervisor`.
$ErrorActionPreference = 'Stop'
$scratch = Join-Path ([IO.Path]::GetTempPath()) ('kanade-recovery-' + [Guid]::NewGuid().ToString('N'))
$launcher = Join-Path $scratch 'start-engine.ps1'
$image = Join-Path $scratch 'kanade-engine.exe'
$cs = Join-Path $scratch 'Fixture.cs'
$shell = Join-Path ([Environment]::SystemDirectory) 'WindowsPowerShell\v1.0\powershell.exe'
function Get-Fixture {
    @(Get-CimInstance Win32_Process -Filter "Name='kanade-engine.exe'" | Where-Object { $_.ExecutablePath -eq $image })
}
function Invoke-Launcher {
    $run = Start-Process -FilePath $shell -WindowStyle Hidden -PassThru -ArgumentList (
        '-NoLogo -NoProfile -NonInteractive -ExecutionPolicy RemoteSigned -File "{0}"' -f $launcher
    )
    # Not -Wait: that also waits for the supervisor the launcher leaves running.
    if (-not $run.WaitForExit(30000)) {
        $run.Kill()
        throw 'The launcher did not return.'
    }
    if ($run.ExitCode -ne 0) { throw "The launcher failed ($($run.ExitCode))." }
}
function Wait-Fixture([int]$Count) {
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        $found = Get-Fixture
        if ($found.Count -eq $Count) { return $found }
        Start-Sleep -Milliseconds 100
    } until ([DateTime]::UtcNow -gt $deadline)
    throw "Expected $Count processes of the fixture, found $($found.Count)."
}
New-Item -ItemType Directory -Path $scratch | Out-Null
try {
    Copy-Item (Join-Path $PSScriptRoot '../../packaging/windows/start-engine.ps1') $launcher
    # Without an image there is nothing to start, and that is not an error.
    Invoke-Launcher
    if ((Get-Fixture).Count) { throw 'Something was started without an image.' }

    # Compile the fixture with Windows PowerShell's .NET Framework compiler.
    Set-Content -LiteralPath $cs -Value 'class Fixture { static void Main() { System.Threading.Thread.Sleep(-1); } }'
    $compiler = Join-Path ([Runtime.InteropServices.RuntimeEnvironment]::GetRuntimeDirectory()) 'csc.exe'
    if (-not (Test-Path $compiler)) {
        $compiler = Join-Path $env:SystemRoot 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
    }
    & $compiler /nologo /target:winexe "/out:$image" $cs
    if ($LASTEXITCODE) { throw 'Could not compile the recovery fixture.' }

    # The launcher starts the engine's own supervisor, not a script.
    Invoke-Launcher
    $first = Wait-Fixture 1
    if ($first[0].CommandLine -notmatch '--supervise\s*$') { throw 'The engine was not started as its supervisor.' }

    # A current engine is left alone. Another supervisor is offered; the real
    # one exits by itself when it finds the first.
    Invoke-Launcher
    $both = Wait-Fixture 2
    if ($first[0].ProcessId -notin $both.ProcessId) { throw 'A current engine was stopped.' }

    # After an update the image is newer than the processes started from it.
    (Get-Item -LiteralPath $image).LastWriteTimeUtc = [DateTime]::UtcNow.AddMinutes(5)
    Invoke-Launcher
    $next = Wait-Fixture 1
    if ($next[0].ProcessId -in $both.ProcessId) { throw 'An engine of the previous image kept running.' }
    if ($next[0].CommandLine -notmatch '--supervise\s*$') { throw 'The updated engine was not started as its supervisor.' }
    Write-Output 'Started the supervisor, kept a current engine and replaced one older than its image.'
} finally {
    foreach ($process in Get-Fixture) { Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue }
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while ((Get-Fixture).Count -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 100 }
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
