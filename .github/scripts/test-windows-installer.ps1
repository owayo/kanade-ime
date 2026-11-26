$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
Set-StrictMode -Version Latest
if (-not $env:GITHUB_ACTIONS) { throw 'Run only on a disposable CI runner' }
. (Join-Path $PSScriptRoot 'windows-brand-icon.ps1')
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
$scheduler = New-Object -ComObject Schedule.Service
$scheduler.Connect()
$taskFolder = $scheduler.GetFolder('\')
function Get-LogonTask {
    try { return $taskFolder.GetTask('Kanade IME') }
    catch [System.IO.FileNotFoundException] { return $null }
}
function Assert-LogonTask {
    $task = Get-LogonTask
    if (-not $task) { throw 'The engine logon task is missing' }
    $definition = $task.Definition
    $trigger = $definition.Triggers.Item(1)
    $action = $definition.Actions.Item(1)
    $settings = $definition.Settings
    $group = $definition.Principal.GroupId
    if ($group -notlike 'S-1-*') {
        $group = [Security.Principal.NTAccount]::new($group).Translate([Security.Principal.SecurityIdentifier]).Value
    }
    if (-not $task.Enabled -or $definition.Triggers.Count -ne 1 -or $trigger.Type -ne 9 -or
        -not $trigger.Enabled -or $trigger.UserId -or ($trigger.Delay -and $trigger.Delay -ne 'PT0S') -or
        $group -ne 'S-1-5-32-545' -or $definition.Principal.RunLevel -ne 0 -or
        $settings.MultipleInstances -ne 0 -or $settings.DisallowStartIfOnBatteries -or
        $settings.StopIfGoingOnBatteries -or $settings.ExecutionTimeLimit -ne 'PT0S' -or
        $settings.Priority -ne 5 -or $definition.Actions.Count -ne 1 -or
        $action.Path -ne (Join-Path $env:ProgramFiles 'Kanade/kanade-engine.exe') -or
        $action.Arguments -ne '--supervise') {
        throw 'The installed task does not start the supervisor at interactive logon'
    }
}
if (Get-LogonTask) { throw 'Runner already has a Kanade logon task' }
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
function Get-MsiProperty($Installer, [string]$Path, [string]$Name) {
    $database = $Installer.OpenDatabase($Path, 0)
    $view = $database.OpenView("SELECT ``Value`` FROM ``Property`` WHERE ``Property``='$Name'")
    # A function returns everything its statements emit, and these COM calls
    # emit a value: the property alone must be the result.
    [void]$view.Execute()
    $value = $view.Fetch().StringData(1)
    [void]$view.Close()
    return $value
}
function New-SameVersionMsi([string]$Version) {
    # Another build of the same package and payload: the same version and
    # upgrade code, and a product code of its own.
    $saved = $env:DOTNET_ROLL_FORWARD
    try {
        $env:DOTNET_ROLL_FORWARD = 'Major'
        & ./artifacts/wix/wix.exe build packaging/windows/Package.wxs artifacts/windows/Payload.wxs -arch x64 -d "Version=$Version" -o artifacts/windows/same-version.msi | Out-Host
        if ($LASTEXITCODE -ne 0) { throw 'Could not build the same-version installer fixture' }
    } finally { $env:DOTNET_ROLL_FORWARD = $saved }
    return (Resolve-Path artifacts/windows/same-version.msi).Path
}
function New-SameVersionSetup([string]$Version, [string]$Package) {
    # A setup like the installed one, carrying the other build of its package.
    [xml]$bundle = Get-Content packaging/windows/Bundle.wxs -Raw
    $names = [Xml.XmlNamespaceManager]::new($bundle.NameTable)
    $names.AddNamespace('w', $bundle.DocumentElement.NamespaceURI)
    $bundle.SelectSingleNode('/w:Wix/w:Bundle/w:Chain/w:MsiPackage', $names).SetAttribute('SourceFile', $Package)
    $bundle.Save((Join-Path (Get-Location) 'artifacts/windows/same-version-bundle.wxs'))
    $saved = $env:DOTNET_ROLL_FORWARD
    try {
        $env:DOTNET_ROLL_FORWARD = 'Major'
        & ./artifacts/wix/wix.exe build artifacts/windows/same-version-bundle.wxs -ext WixToolset.Bal.wixext -arch x64 -d "Version=$Version" -o artifacts/windows/same-version-setup.exe | Out-Host
        if ($LASTEXITCODE -ne 0) { throw 'Could not build the same-version setup fixture' }
    } finally { $env:DOTNET_ROLL_FORWARD = $saved }
    return (Resolve-Path artifacts/windows/same-version-setup.exe).Path
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
    # Older packages owned the Run value and had no logon task.
    foreach ($node in $package.SelectNodes("w:InstallExecuteSequence/w:Custom[starts-with(@Action,'ApplyLogonTask')]|w:SetProperty[@Id='KanadeLogonAction']|w:CustomAction[starts-with(@Id,'ApplyLogonTask')]", $names)) {
        [void]$node.ParentNode.RemoveChild($node)
    }
    $directory = $package.SelectSingleNode(".//w:Directory[@Id='INSTALLDIR']", $names)
    $oldStartup = $legacy.CreateDocumentFragment()
    $oldStartup.InnerXml = @'
<Component xmlns="http://wixtoolset.org/schemas/v4/wxs" Id="Startup" Guid="3C1F580E-90DC-4CFA-BB52-562A326C079B" Bitness="always64">
  <RegistryValue Root="HKLM" Key="Software\Microsoft\Windows\CurrentVersion\Run" Name="Kanade" Type="string" Value="&quot;[INSTALLDIR]kanade-engine.exe&quot; --supervise" KeyPath="yes" />
</Component>
'@
    [void]$directory.AppendChild($oldStartup)
    $reference = $legacy.CreateElement('ComponentRef', $legacy.DocumentElement.NamespaceURI)
    $reference.SetAttribute('Id', 'Startup')
    [void]$package.SelectSingleNode('w:Feature', $names).AppendChild($reference)
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
$sameMsi = $null
$sameSetup = $null
$editor = $null
try {
    Invoke-Msi @('/i', $msi, '/qn', '/norestart', 'KANADE_TEST_ROLLBACK=1', '/l*v', 'artifacts/windows/rollback.log') @(1603)
    if (-not (Select-String -Path artifacts/windows/rollback.log -SimpleMatch 'CustomAction TestRollback returned actual error code 1' -Quiet)) { throw 'The requested failure action did not execute' }
    if (Test-Registered) { throw 'Rollback left a COM registration' }
    if (Get-LogonTask) { throw 'Rollback left an engine logon task' }
    if (Test-Path -LiteralPath $shortcut) { throw 'Rollback left the Start menu shortcut' }
    Write-Output 'MSI failure rollback removed both COM registrations'
    $legacyMsi = New-LegacyMsi
    Invoke-Msi @('/i', $legacyMsi, '/qn', '/norestart', '/l*v', 'artifacts/windows/legacy.log') @(0, 3010)
    $runKey = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Run'
    if (-not (Get-ItemPropertyValue $runKey -Name Kanade) -or (Get-LogonTask)) { throw 'Unexpected legacy startup state' }
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
    Assert-LogonTask
    if (Get-ItemProperty $runKey -Name Kanade -ErrorAction SilentlyContinue) { throw 'Upgrade left the legacy Run entry' }
    $installer = New-Object -ComObject WindowsInstaller.Installer
    $legacyCode = Get-MsiProperty $installer $legacyMsi 'ProductCode'
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
    # The input switcher and the taskbar read the brand icon from the installed DLL.
    $icon = Get-RegisteredBrandIcon
    if (-not $icon.Path.StartsWith((Join-Path $env:ProgramFiles 'Kanade\'), [StringComparison]::OrdinalIgnoreCase)) {
        throw "The input profile's icon file is outside the installation: $($icon.Path)"
    }
    Write-Output "Installed input profile shows the brand icon of $($icon.Path), sizes $($icon.Sizes -join ', ')"
    # Windows Installer compares three fields of a version, which builds that
    # differ only in their preview number share. Another build with the same
    # version replaces the installed one instead of being installed beside it,
    # and removing the setup it replaced leaves the TIP registered.
    $installedCode = Get-MsiProperty $installer $msi 'ProductCode'
    $version = Get-MsiProperty $installer $msi 'ProductVersion'
    $sameMsi = New-SameVersionMsi $version
    if ((Get-MsiProperty $installer $sameMsi 'ProductCode') -eq $installedCode) { throw 'The same-version fixture has the installed product code' }
    $sameSetup = New-SameVersionSetup $version $sameMsi
    Invoke-Installer $sameSetup @('/install', '/quiet', '/norestart', '/log', 'artifacts/windows/same-version.log') @(0, 3010)
    if ($installer.ProductState($installedCode) -eq 5) { throw 'A build with the same version did not replace the installed one' }
    if ($installer.ProductState((Get-MsiProperty $installer $sameMsi 'ProductCode')) -ne 5) { throw 'The same-version build is not installed' }
    $products = @($installer.RelatedProducts((Get-MsiProperty $installer $msi 'UpgradeCode')))
    if ($products.Count -ne 1) { throw "Expected one installed Kanade product, found $($products.Count)" }
    if (-not (Test-Registered)) { throw 'The same-version replacement left the TIP unregistered' }
    if ($editor.HasExited) { throw 'The same-version replacement closed the running editor' }
    Write-Output 'A build with the same version replaced the installed one and kept the TIP registered'
    $previousData = $env:KANADE_DATA_DIR
    $previousUnsigned = $env:KANADE_ALLOW_UNSIGNED
    $temporaryData = Join-Path $env:RUNNER_TEMP ('kanade-installer-test-' + [Guid]::NewGuid().ToString('N'))
    $env:KANADE_DATA_DIR = $temporaryData
    $env:KANADE_ALLOW_UNSIGNED = '1'
    $engine = $null
    try {
        $application = Join-Path $env:ProgramFiles Kanade
        if (-not (Test-Path -LiteralPath $shortcut)) { throw 'The Start menu shortcut is missing' }
        # WScript.Shell opens no link whose name is outside the computer's ANSI
        # code page and then reports an empty target, so it reads a copy
        # under a plain name.
        $link = Join-Path $env:RUNNER_TEMP ('kanade-settings-' + [Guid]::NewGuid().ToString('N') + '.lnk')
        Copy-Item -LiteralPath $shortcut -Destination $link
        try { $target = (New-Object -ComObject WScript.Shell).CreateShortcut($link).TargetPath }
        finally { Remove-Item -LiteralPath $link -Force }
        if ($target -ne (Join-Path $application 'kanade-ui.exe')) { throw "The Start menu shortcut does not open the installed settings but '$target'" }
        # The shortcut starts a console program. Its manifest keeps Windows from
        # opening a console for it, and must still declare GPUI's DPI awareness.
        $settingsImage = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes($target))
        if ($settingsImage -notmatch 'consoleAllocationPolicy[^<>]*>detached<') { throw 'The installed settings would open a console window from the Start menu' }
        if ($settingsImage -notmatch 'dpiAwareness[^<>]*>PerMonitorV2<') { throw 'The installed settings lost their DPI awareness' }
        Assert-LogonTask
        if (-not (Test-Path -LiteralPath (Join-Path $application 'start-engine.ps1'))) {
            throw 'The setup launcher is not installed.'
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
    if ($sameSetup) {
        try { Invoke-Installer $sameSetup @('/uninstall', '/quiet', '/norestart', '/log', 'artifacts/windows/same-version-uninstall.log') @(0, 1605, 3010) }
        catch { $cleanupFailure = $_ }
    }
    try { Invoke-Setup @('/uninstall', '/quiet', '/norestart', '/log', 'artifacts/windows/bundle-uninstall.log') @(0, 1605, 3010) }
    catch { if (-not $cleanupFailure) { $cleanupFailure = $_ } }
    try { Invoke-Msi @('/x', $msi, '/qn', '/norestart', '/l*v', 'artifacts/windows/uninstall.log') @(0, 1605, 3010) }
    catch { if (-not $cleanupFailure) { $cleanupFailure = $_ } }
    if ($sameMsi) {
        try { Invoke-Msi @('/x', $sameMsi, '/qn', '/norestart', '/l*v', 'artifacts/windows/same-version-msi-uninstall.log') @(0, 1605, 3010) }
        catch { if (-not $cleanupFailure) { $cleanupFailure = $_ } }
    }
    if ($legacyMsi) {
        try { Invoke-Msi @('/x', $legacyMsi, '/qn', '/norestart', 'MSIRESTARTMANAGERCONTROL=Disable') @(0, 1605, 3010) }
        catch { if (-not $cleanupFailure) { $cleanupFailure = $_ } }
    }
}
if ($failure) { throw $failure }
if ($cleanupFailure) { throw $cleanupFailure }
if (Test-Registered) { throw 'Uninstall left a COM registration' }
if (Get-LogonTask) { throw 'Uninstall left an engine logon task' }
if (Get-ItemProperty 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Run' -Name Kanade -ErrorAction SilentlyContinue) { throw 'Uninstall left a startup entry' }
if (Test-Path -LiteralPath $shortcut) { throw 'Uninstall left the Start menu shortcut' }
Write-Output 'EXE uninstallation removed both COM registrations, startup entry and Start menu shortcut'
