# Exercise only the disposable engine image in this test's own directory.
$ErrorActionPreference = 'Stop'
$scratch = Join-Path ([IO.Path]::GetTempPath()) ('kanade-recovery-' + [Guid]::NewGuid().ToString('N'))
$launcher = Join-Path $scratch 'start-engine.ps1'
$image = Join-Path $scratch 'kanade-engine.exe'
$shell = Join-Path ([Environment]::SystemDirectory) 'WindowsPowerShell\v1.0\powershell.exe'
function Get-FixtureEngine {
    Get-Process -Name kanade-engine -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $image }
}
function Start-Watcher {
    Start-Process -FilePath $shell -WindowStyle Hidden -PassThru -ArgumentList (
        '-NoLogo -NoProfile -NonInteractive -ExecutionPolicy RemoteSigned -File "{0}" -Watch' -f $launcher
    )
}
New-Item -ItemType Directory -Path $scratch | Out-Null
$watcher = $null
$duplicate = $null
try {
    Copy-Item (Join-Path $PSScriptRoot '../../packaging/windows/start-engine.ps1') $launcher
    # Compile the fixture with Windows PowerShell's .NET Framework compiler.
    $source = 'class Fixture { static void Main() { System.Threading.Thread.Sleep(-1); } }'
    $compiler = Join-Path ([Runtime.InteropServices.RuntimeEnvironment]::GetRuntimeDirectory()) 'csc.exe'
    if (-not (Test-Path $compiler)) {
        $compiler = Join-Path $env:SystemRoot 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
    }
    $cs = Join-Path $scratch 'Fixture.cs'
    Set-Content -LiteralPath $cs -Value $source
    & $compiler /nologo /target:winexe "/out:$image" $cs
    if ($LASTEXITCODE) { throw 'Could not compile the recovery fixture.' }
    $existing = Start-Process -FilePath $image -WindowStyle Hidden -PassThru
    $watcher = Start-Watcher
    # Either watcher can win the mutex. The other must exit without starting an engine.
    $duplicate = Start-Watcher
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    while (-not ($watcher.HasExited -or $duplicate.HasExited) -and [DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Milliseconds 100
    }
    if ($watcher.HasExited) { $watcher, $duplicate = $duplicate, $watcher }
    if ($watcher.HasExited -or -not $duplicate.HasExited -or $duplicate.ExitCode -ne 0) { throw 'Expected one active supervisor.' }
    $processes = @(Get-FixtureEngine)
    if ($processes.Count -ne 1 -or $processes[0].Id -ne $existing.Id) { throw 'The running engine was replaced or duplicated.' }
    $existing.Kill()
    $existing.WaitForExit()
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        Start-Sleep -Milliseconds 100
        $processes = @(Get-FixtureEngine)
    } until ($processes.Count -or [DateTime]::UtcNow -gt $deadline)
    if ($processes.Count -ne 1 -or $processes[0].Id -eq $existing.Id) { throw 'Engine did not recover.' }
    $processes[0].Kill()
    $processes[0].WaitForExit()
    Remove-Item -LiteralPath $image
    if (-not $watcher.WaitForExit(15000) -or $watcher.ExitCode -ne 0) { throw 'Supervisor did not exit after removal.' }
    Write-Output 'Attached to the existing engine, recovered one replacement and stopped after removal.'
} finally {
    foreach ($p in @($duplicate, $watcher)) {
        if ($p -and -not $p.HasExited) { $p.Kill(); $p.WaitForExit() }
    }
    foreach ($p in @(Get-FixtureEngine)) { $p.Kill(); $p.WaitForExit() }
    foreach ($file in @($image, $cs, $launcher)) {
        if ($file -and (Test-Path -LiteralPath $file)) { Remove-Item -LiteralPath $file }
    }
    Remove-Item -LiteralPath $scratch
}
