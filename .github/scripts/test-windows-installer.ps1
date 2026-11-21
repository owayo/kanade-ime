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
# The settings application is reachable from the Start menu of every user.
$shortcut = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Kanade の設定.lnk'
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
function New-LegacyMsi {
    # Recreate the cached uninstall that ran before packages disabled Restart Manager.
    [xml]$legacy = Get-Content packaging/windows/Package.wxs -Raw
    $names = [Xml.XmlNamespaceManager]::new($legacy.NameTable)
    $names.AddNamespace('w', $legacy.DocumentElement.NamespaceURI)
    $package = $legacy.SelectSingleNode('/w:Wix/w:Package', $names)
    foreach ($id in 'MSIRESTARTMANAGERCONTROL', 'REBOOT') {
        [void]$package.RemoveChild($package.SelectSingleNode("w:Property[@Id='$id']", $names))
    }
    $package.SelectSingleNode('w:MajorUpgrade', $names).SetAttribute('Schedule', 'afterInstallInitialize')
    $package.SelectSingleNode("w:InstallExecuteSequence/w:Custom[@Action='Rollback64']", $names).SetAttribute('After', 'InstallFiles')
    $legacy.Save((Join-Path (Get-Location) 'artifacts/windows/legacy.wxs'))
    $saved = $env:DOTNET_ROLL_FORWARD
    try {
        $env:DOTNET_ROLL_FORWARD = 'Major'
        & ./artifacts/wix/wix.exe build artifacts/windows/legacy.wxs artifacts/windows/Payload.wxs -arch x64 -d Version=1.0.0 -o artifacts/windows/legacy.msi | Out-Host
        if ($LASTEXITCODE -ne 0) { throw 'Could not build the legacy installer fixture' }
    } finally { $env:DOTNET_ROLL_FORWARD = $saved }
    return (Resolve-Path artifacts/windows/legacy.msi).Path
}
$failure = $null
$cleanupFailure = $null
$legacyMsi = $null
$editor = $null
try {
    Invoke-Msi @('/i', $msi, '/qn', '/norestart', 'KANADE_TEST_ROLLBACK=1', '/l*v', 'artifacts/windows/rollback.log') @(1603)
    if (-not (Select-String -Path artifacts/windows/rollback.log -SimpleMatch 'CustomAction TestRollback returned actual error code 1' -Quiet)) { throw 'The requested failure action did not execute' }
    if (Test-Registered) { throw 'Rollback left a COM registration' }
    if (Test-Path -LiteralPath $shortcut) { throw 'Rollback left the Start menu shortcut' }
    Write-Output 'MSI failure rollback removed both COM registrations'
    $legacyMsi = New-LegacyMsi
    Invoke-Msi @('/i', $legacyMsi, '/qn', '/norestart', '/l*v', 'artifacts/windows/legacy.log') @(0, 3010)
    $readyFile = Join-Path $env:RUNNER_TEMP ('editor-ready-' + [Guid]::NewGuid())
    $holder = (Resolve-Path .github/scripts/hold-installed-tip.ps1).Path
    $editor = Start-Process powershell.exe -WindowStyle Hidden -PassThru -ArgumentList "-NoProfile -STA -File `"$holder`" -ReadyFile `"$readyFile`""
    for ($i = 0; $i -lt 100 -and -not (Test-Path $readyFile); $i++) {
        if ($editor.HasExited) { throw 'Test editor exited before loading the TIP' }
        Start-Sleep -Milliseconds 100
    }
    if (-not (Test-Path $readyFile)) { throw 'Test editor did not load the installed TIP' }
    Invoke-Setup @('/install', '/quiet', '/norestart', '/log', 'artifacts/windows/install.log') @(0, 3010)
    if ($editor.HasExited) { throw 'Silent major upgrade closed the running editor' }
    $installer = New-Object -ComObject WindowsInstaller.Installer
    $legacyDb = $installer.OpenDatabase($legacyMsi, 0)
    $view = $legacyDb.OpenView("SELECT ``Value`` FROM ``Property`` WHERE ``Property``='ProductCode'")
    $view.Execute()
    $legacyCode = $view.Fetch().StringData(1)
    $view.Close()
    if ($installer.ProductState($legacyCode) -eq 5) { throw 'Legacy product was not removed by the upgrade' }
    Write-Output 'Silent upgrade removed the legacy product and preserved the original TIP host process'
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
        if (-not (Test-Path -LiteralPath $shortcut)) { throw 'The Start menu shortcut is missing' }
        $target = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut).TargetPath
        if ($target -ne (Join-Path $application 'kanade-ui.exe')) { throw 'The Start menu shortcut does not open the installed settings' }
        # The shortcut starts a console program. Its manifest keeps Windows from
        # opening a console for it, and must still declare GPUI's DPI awareness.
        $settingsImage = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes($target))
        if ($settingsImage -notmatch 'consoleAllocationPolicy[^<>]*>detached<') { throw 'The installed settings would open a console window from the Start menu' }
        if ($settingsImage -notmatch 'dpiAwareness[^<>]*>PerMonitorV2<') { throw 'The installed settings lost their DPI awareness' }
        $startup = Get-ItemPropertyValue 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Run' -Name Kanade
        $launcher = Join-Path $application 'start-engine.ps1'
        if (-not (Test-Path -LiteralPath $launcher) -or -not $startup.EndsWith(('"{0}" -Watch' -f $launcher), [StringComparison]::OrdinalIgnoreCase)) {
            throw 'Installed startup entry does not launch the engine supervisor.'
        }
        python .github/scripts/bundled_model.py --verify-windows-install $application
        if ($LASTEXITCODE -ne 0) { throw 'The installed Tsumugi model does not match the release lock' }
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
    if ($editor -and -not $editor.HasExited) { $editor.Kill(); $editor.WaitForExit() }
    try { Invoke-Setup @('/uninstall', '/quiet', '/norestart', '/log', 'artifacts/windows/bundle-uninstall.log') @(0, 1605, 3010) }
    catch { $cleanupFailure = $_ }
    try { Invoke-Msi @('/x', $msi, '/qn', '/norestart', '/l*v', 'artifacts/windows/uninstall.log') @(0, 1605, 3010) }
    catch { if (-not $cleanupFailure) { $cleanupFailure = $_ } }
    if ($legacyMsi) {
        try { Invoke-Msi @('/x', $legacyMsi, '/qn', '/norestart', 'MSIRESTARTMANAGERCONTROL=Disable') @(0, 1605, 3010) }
        catch { if (-not $cleanupFailure) { $cleanupFailure = $_ } }
    }
}
if ($failure) { throw $failure }
if ($cleanupFailure) { throw $cleanupFailure }
if (Test-Registered) { throw 'Uninstall left a COM registration' }
if (Get-ItemProperty 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Run' -Name Kanade -ErrorAction SilentlyContinue) { throw 'Uninstall left a startup entry' }
if (Test-Path -LiteralPath $shortcut) { throw 'Uninstall left the Start menu shortcut' }
Write-Output 'EXE uninstallation removed both COM registrations, startup entry and Start menu shortcut'
