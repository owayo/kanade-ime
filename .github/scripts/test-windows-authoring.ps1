# Compile the real installer definitions with fixture files; never install them.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $env:GITHUB_ACTIONS) { throw 'Run only on a disposable CI runner' }
$source = (Get-Location).Path
$fixture = Join-Path $env:RUNNER_TEMP ('kanade-authoring-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force "$fixture/artifacts/windows/payload/x86", "$fixture/packaging" | Out-Null
Copy-Item packaging/windows "$fixture/packaging/windows" -Recurse
Copy-Item Cargo.toml $fixture
function Run-Native([scriptblock]$Command) {
    & $Command
    if ($LASTEXITCODE -ne 0) { throw "Authoring command failed ($LASTEXITCODE)" }
}
$previousRollForward = $env:DOTNET_ROLL_FORWARD
Push-Location $fixture
try {
    $payload = 'artifacts/windows/payload'
    New-Item -ItemType Directory "$payload/models" | Out-Null
    Set-Content "$payload/models/tsumugi.gguf" 'authoring fixture'
    foreach ($name in @('kanade-engine.exe', 'kanade-ui.exe', 'kanade_tip.dll', 'kanade-mozc-worker.exe', 'kanade-project-analyzer.exe')) {
        Copy-Item "$env:WINDIR/System32/cmd.exe" "$payload/$name"
    }
    Copy-Item "$env:WINDIR/SysWOW64/cmd.exe" "$payload/x86/kanade_tip.dll"
    Copy-Item "$env:WINDIR/System32/cmd.exe" artifacts/windows/kanade-installer.exe
    foreach ($name in @('manifest.json', 'project-analyzer.json')) { Set-Content "$payload/$name" '{}' }
    Run-Native { python "$source/.github/scripts/windows-payload.py" }
    & packaging/windows/build-icon.ps1 -Source "$source/assets/icons/app.png" -Destination artifacts/windows/Kanade.ico
    Run-Native { dotnet tool install wix --version 4.0.6 --tool-path artifacts/wix }
    $env:DOTNET_ROLL_FORWARD = 'Major'
    Run-Native { ./artifacts/wix/wix.exe extension add --global WixToolset.Bal.wixext/4.0.6 }
    $version = (Get-Content artifacts/windows/version.txt -Raw).Trim()
    Run-Native { ./artifacts/wix/wix.exe build packaging/windows/Package.wxs artifacts/windows/Payload.wxs -arch x64 -d "Version=$version" -o artifacts/windows/Kanade.msi }
    Run-Native { ./artifacts/wix/wix.exe build packaging/windows/Bundle.wxs -ext WixToolset.Bal.wixext -arch x64 -d "Version=$version" -o artifacts/windows/KanadeSetup.exe }
    foreach ($name in @('Kanade.msi', 'KanadeSetup.exe')) {
        if ((Get-Item "artifacts/windows/$name").Length -eq 0) { throw 'Empty installer fixture' }
    }
    # The package must carry the Start menu shortcut to the settings application.
    $installer = New-Object -ComObject WindowsInstaller.Installer
    $database = $installer.OpenDatabase((Resolve-Path artifacts/windows/Kanade.msi).Path, 0)
    $view = $database.OpenView('SELECT `Directory_`, `Name`, `Target`, `Icon_` FROM `Shortcut`')
    $view.Execute()
    $row = $view.Fetch()
    if (-not $row) { throw 'The package has no Start menu shortcut' }
    $shortcut = @(1..4 | ForEach-Object { $row.StringData($_) })
    if ($view.Fetch()) { throw 'The package has more than one shortcut' }
    $view.Close()
    if ($shortcut[0] -ne 'ProgramMenuFolder' -or $shortcut[1] -notlike '*|Kanade の設定' -or
        $shortcut[2] -ne '[INSTALLDIR]kanade-ui.exe' -or $shortcut[3] -ne 'Kanade.ico') {
        throw "Unexpected Start menu shortcut: $($shortcut -join ', ')"
    }
    foreach ($object in $row, $view, $database, $installer) {
        [void][Runtime.InteropServices.Marshal]::ReleaseComObject($object)
    }
    [GC]::Collect()
    [GC]::WaitForPendingFinalizers()
} finally {
    $env:DOTNET_ROLL_FORWARD = $previousRollForward
    Pop-Location
    Remove-Item $fixture -Recurse -Force
}
