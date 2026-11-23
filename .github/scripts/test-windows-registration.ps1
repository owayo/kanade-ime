# Exercise real x64/x86 registration without building the conversion worker.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $env:GITHUB_ACTIONS) { throw 'Run only on a disposable CI runner' }
. (Join-Path $PSScriptRoot 'windows-brand-icon.ps1')
$class = '{6DAFA84E-C56A-42BE-983E-0D18F3DC5B21}'
$servers = @{
    x64 = @("$env:WINDIR/System32/regsvr32.exe", (Resolve-Path target/x86_64-pc-windows-msvc/release/kanade_tip.dll).Path)
    x86 = @("$env:WINDIR/SysWOW64/regsvr32.exe", (Resolve-Path target/i686-pc-windows-msvc/release/kanade_tip.dll).Path)
}
function Invoke-Registration([string]$Architecture, [bool]$Remove) {
    $server = $servers[$Architecture]
    $start = [System.Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $server[0]
    $start.UseShellExecute = $false
    $start.ArgumentList.Add('/s')
    if ($Remove) { $start.ArgumentList.Add('/u') }
    $start.ArgumentList.Add($server[1])
    $process = [System.Diagnostics.Process]::Start($start)
    try {
        if (-not $process.WaitForExit(30000)) {
            $process.Kill($true)
            throw 'Registration process timed out'
        }
        if ($process.ExitCode -ne 0) {
            throw "Registration failed: architecture=$Architecture remove=$Remove exit=$($process.ExitCode)"
        }
    } finally { $process.Dispose() }
}
function Assert-Registered([bool]$Present) {
    foreach ($view in @('Registry64', 'Registry32')) {
        $registry = [Microsoft.Win32.RegistryKey]::OpenBaseKey('LocalMachine', $view)
        try {
            $key = $registry.OpenSubKey("Software\Classes\CLSID\$class\InprocServer32")
            try {
                if ([bool]$key -ne $Present) { throw "Unexpected COM state in $view" }
                if ($key -and -not (Test-Path $key.GetValue(''))) { throw 'Registered DLL is missing' }
            } finally { if ($key) { $key.Dispose() } }
        } finally { $registry.Dispose() }
    }
}
# The input profile is shared by both architectures and keeps the icon file of
# the DLL registered last. Windows shows the same brand icon either way only
# if both DLLs hold it at the registered position.
function Assert-BrandIcon([string]$Last) {
    $icon = Get-RegisteredBrandIcon
    if ((Get-Item -LiteralPath $icon.Path).FullName -ine $servers[$Last][1]) {
        throw "The profile's icon file is not the DLL registered last ($Last): $($icon.Path)"
    }
    foreach ($architecture in $servers.Keys) {
        if ((Get-BrandIcon $servers[$architecture][1] $icon.Index).Digest -ne $icon.Digest) {
            throw "The $architecture DLL holds another icon at position $($icon.Index)"
        }
    }
    return $icon
}
Assert-Registered $false
$failure = $null
$cleanupFailure = $null
$orders = @(@('x64', 'x86'), @('x86', 'x64'))
try {
    foreach ($registration in $orders) {
        foreach ($removal in $orders) {
            Invoke-Registration $registration[0] $false
            Invoke-Registration $registration[1] $false
            Assert-Registered $true
            $icon = Assert-BrandIcon $registration[1]
            Invoke-Registration $removal[0] $true
            Invoke-Registration $removal[1] $true
            Assert-Registered $false
            # An absent shared TSF registration is a successful no-op in either DLL.
            Invoke-Registration $removal[1] $true
            Invoke-Registration $removal[0] $true
            Assert-Registered $false
            Write-Output "Registration ($($registration -join ' then ')) and repeated removal ($($removal -join ' then ')) passed; brand icon at position $($icon.Index) of the $($registration[1]) DLL, sizes $($icon.Sizes -join ', ')"
        }
    }
} catch { $failure = $_ }
finally {
    foreach ($architecture in @('x64', 'x86')) {
        try { Invoke-Registration $architecture $true }
        catch { if (-not $cleanupFailure) { $cleanupFailure = $_ } }
    }
}
if ($failure) { throw $failure }
if ($cleanupFailure) { throw $cleanupFailure }
Assert-Registered $false
