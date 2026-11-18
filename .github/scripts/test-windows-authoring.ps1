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
    foreach ($name in @('kanade-engine.exe', 'kanade-ui.exe', 'kanade_tip.dll', 'kanade-mozc-worker.exe', 'kanade-project-analyzer.exe')) {
        Copy-Item "$env:WINDIR/System32/cmd.exe" "$payload/$name"
    }
    Copy-Item "$env:WINDIR/SysWOW64/cmd.exe" "$payload/x86/kanade_tip.dll"
    foreach ($name in @('manifest.json', 'project-analyzer.json')) { Set-Content "$payload/$name" '{}' }
    Run-Native { python "$source/.github/scripts/windows-payload.py" }
    Run-Native { dotnet tool install wix --version 4.0.6 --tool-path artifacts/wix }
    $env:DOTNET_ROLL_FORWARD = 'Major'
    Run-Native { ./artifacts/wix/wix.exe extension add --global WixToolset.Bal.wixext/4.0.6 }
    $version = (Get-Content artifacts/windows/version.txt -Raw).Trim()
    Run-Native { ./artifacts/wix/wix.exe build packaging/windows/Package.wxs artifacts/windows/Payload.wxs -arch x64 -d "Version=$version" -o artifacts/windows/Kanade.msi }
    Run-Native { ./artifacts/wix/wix.exe build packaging/windows/Bundle.wxs -ext WixToolset.Bal.wixext -arch x64 -d "Version=$version" -o artifacts/windows/KanadeSetup.exe }
    foreach ($name in @('Kanade.msi', 'KanadeSetup.exe')) {
        if ((Get-Item "artifacts/windows/$name").Length -eq 0) { throw 'Empty installer fixture' }
    }
} finally {
    $env:DOTNET_ROLL_FORWARD = $previousRollForward
    Pop-Location
    Remove-Item $fixture -Recurse -Force
}
