$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
Set-StrictMode -Version Latest
if (-not $env:GITHUB_ACTIONS) { throw 'Run only on a disposable CI runner' }
$msi = (Resolve-Path artifacts/windows/Kanade.msi).Path
$setup = (Resolve-Path artifacts/release/kanade-ime-x86_64-pc-windows-msvc.exe).Path
$class = '{6DAFA84E-C56A-42BE-983E-0D18F3DC5B21}'
function Test-Registered {
    foreach ($view in @([Microsoft.Win32.RegistryView]::Registry64, [Microsoft.Win32.RegistryView]::Registry32)) {
        $registry = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, $view)
        try {
            $key = $registry.OpenSubKey("Software\Classes\CLSID\$class\InprocServer32")
            if ($key) { $key.Dispose(); return $true }
        } finally { $registry.Dispose() }
    }
    return $false
}
if (Test-Registered) { throw 'Runner already has Kanade installed' }
function Invoke-Installer([string]$File, [string[]]$Arguments, [int[]]$Expected) {
    $start = [System.Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $File
    $start.UseShellExecute = $false
    foreach ($argument in $Arguments) { $start.ArgumentList.Add($argument) }
    $process = [System.Diagnostics.Process]::Start($start)
    try {
        if (-not $process.WaitForExit(120000)) {
            $process.Kill($true)
            throw 'Installer process timed out'
        }
        if ($process.ExitCode -notin $Expected) { throw "Installer failed ($($process.ExitCode))" }
    } finally { $process.Dispose() }
}
function Invoke-Msi([string[]]$Arguments, [int[]]$Expected) {
    Invoke-Installer msiexec.exe $Arguments $Expected
}
function Invoke-Setup([string[]]$Arguments, [int[]]$Expected) {
    Invoke-Installer $setup $Arguments $Expected
}
$failure = $null
$cleanupFailure = $null
try {
    Invoke-Msi @('/i', $msi, '/qn', '/norestart', 'KANADE_TEST_ROLLBACK=1', '/l*v', 'artifacts/windows/rollback.log') @(1603)
    if (-not (Select-String -Path artifacts/windows/rollback.log -SimpleMatch 'CustomAction TestRollback returned actual error code 1' -Quiet)) { throw 'The requested failure action did not execute' }
    if (Test-Registered) { throw 'Rollback left a COM registration' }
    Write-Output 'MSI failure rollback removed both COM registrations'
    Invoke-Setup @('/install', '/quiet', '/norestart', '/log', 'artifacts/windows/install.log') @(0, 3010)
    if (-not (Test-Registered)) { throw 'COM registration is missing' }
    foreach ($view in @('Registry64', 'Registry32')) {
        $registry = [Microsoft.Win32.RegistryKey]::OpenBaseKey('LocalMachine', $view)
        try {
            $key = $registry.OpenSubKey("Software\Classes\CLSID\$class\InprocServer32")
            if (-not $key) { throw "Registration missing in $view" }
            try { if (-not (Test-Path $key.GetValue(''))) { throw 'Registered DLL is missing' } }
            finally { $key.Dispose() }
        } finally { $registry.Dispose() }
    }
    $previousData = $env:KANADE_DATA_DIR
    $previousUnsigned = $env:KANADE_ALLOW_UNSIGNED
    $temporaryData = Join-Path $env:RUNNER_TEMP ('kanade-installer-test-' + [Guid]::NewGuid().ToString('N'))
    $env:KANADE_DATA_DIR = $temporaryData
    $env:KANADE_ALLOW_UNSIGNED = '1'
    $engine = $null
    try {
        $application = Join-Path $env:ProgramFiles Kanade
        $engine = Start-Process (Join-Path $application kanade-engine.exe) -PassThru -NoNewWindow -RedirectStandardOutput artifacts/windows/engine-out.log -RedirectStandardError artifacts/windows/engine-error.log
        $ready = $false
        for ($attempt = 0; $attempt -lt 5; $attempt++) {
            if ($engine.HasExited) { throw 'The installed engine exited during startup' }
            $client = Start-Process (Join-Path $application kanade-ui.exe) -ArgumentList settings -PassThru -NoNewWindow -RedirectStandardOutput artifacts/windows/settings.json -RedirectStandardError artifacts/windows/management-error.log
            if (-not $client.WaitForExit(10000)) {
                $client.Kill($true)
                throw 'The installed management client timed out'
            }
            if ($client.ExitCode -eq 0) {
                $settings = Get-Content artifacts/windows/settings.json -Raw | ConvertFrom-Json
                if ($null -eq $settings) { throw 'The installed management response is empty' }
                $ready = $true
                break
            }
            Start-Sleep -Seconds 1
        }
        if (-not $ready) { throw 'The installed engine did not accept its sibling management client' }
        Write-Output 'EXE installation and installed sibling management client passed'
    } finally {
        if ($engine -and -not $engine.HasExited) { Stop-Process -Id $engine.Id -Force }
        $env:KANADE_DATA_DIR = $previousData
        $env:KANADE_ALLOW_UNSIGNED = $previousUnsigned
    }
} catch { $failure = $_ }
finally {
    try { Invoke-Setup @('/uninstall', '/quiet', '/norestart', '/log', 'artifacts/windows/bundle-uninstall.log') @(0, 1605, 3010) }
    catch { $cleanupFailure = $_ }
    try { Invoke-Msi @('/x', $msi, '/qn', '/norestart', '/l*v', 'artifacts/windows/uninstall.log') @(0, 1605, 3010) }
    catch { if (-not $cleanupFailure) { $cleanupFailure = $_ } }
}
if ($failure) { throw $failure }
if ($cleanupFailure) { throw $cleanupFailure }
if (Test-Registered) { throw 'Uninstall left a COM registration' }
if (Get-ItemProperty 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Run' -Name Kanade -ErrorAction SilentlyContinue) { throw 'Uninstall left a startup entry' }
Write-Output 'EXE uninstallation removed both COM registrations and startup entry'
