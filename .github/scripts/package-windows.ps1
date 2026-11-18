param([switch]$Unsigned, [string]$ModelPath = $env:KANADE_TSUMUGI_GGUF)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
function Run-Native([scriptblock]$Command) {
    & $Command
    if ($LASTEXITCODE -ne 0) { throw "Native packaging command failed ($LASTEXITCODE)" }
}
function Run-Wix([scriptblock]$Command) {
    # WiX 4 targets .NET 6; permit current SDK runtimes when 6 is absent.
    $previousRollForward = $env:DOTNET_ROLL_FORWARD
    try {
        $env:DOTNET_ROLL_FORWARD = 'Major'
        Run-Native $Command
    } finally { $env:DOTNET_ROLL_FORWARD = $previousRollForward }
}

if (-not $ModelPath -and $env:KANADE_TSUMUGI_MODEL_URL -and
    -not (Test-Path -LiteralPath 'models/kanade-tsumugi-v0.1.0-s-F16.gguf')) {
    $uri = [uri]$env:KANADE_TSUMUGI_MODEL_URL
    if ($uri.Scheme -ne 'https') { throw 'The model source must use HTTPS.' }
    $directory = New-Item -ItemType Directory -Force artifacts/models
    $ModelPath = Join-Path $directory.FullName 'kanade-tsumugi-v0.1.0-s-F16.gguf'
    Invoke-WebRequest -Uri $uri -OutFile $ModelPath
}

Run-Native { rustup target add i686-pc-windows-msvc x86_64-pc-windows-msvc }
Run-Native { cargo build --release --locked --target x86_64-pc-windows-msvc -p kanade-engine -p kanade-ui -p kanade-llm -p kanade-indexer -p kanade-tip --features kanade-ui/native,kanade-llm/llama }
Run-Native { cargo build --release --locked --target i686-pc-windows-msvc -p kanade-tip }
Run-Native { cargo xtask build-worker }
Run-Native { cargo xtask stage-windows }
$payload = (Resolve-Path artifacts/windows/payload).Path
Copy-Item packaging/windows/WIX-LICENSE.txt (Join-Path $payload WIX-LICENSE.txt)

function Sign-File([string]$Path) {
    if ($Unsigned) { return }
    if (-not $env:WINDOWS_CERT_THUMBPRINT) { throw 'A signing certificate is required' }
    $cert = Get-Item "Cert:\CurrentUser\My\$env:WINDOWS_CERT_THUMBPRINT"
    $result = Set-AuthenticodeSignature -FilePath $Path -Certificate $cert -HashAlgorithm SHA256 -TimestampServer 'http://timestamp.digicert.com'
    if ($result.Status -ne 'Valid') { throw 'Authenticode signing failed' }
}
Get-ChildItem $payload -Recurse -File | Where-Object { $_.Extension -in '.exe', '.dll' } | ForEach-Object { Sign-File $_.FullName }
$modelArgs = @()
if ($ModelPath) { $modelArgs = @('--model', $ModelPath) }
Run-Native { python .github/scripts/stage-windows-model.py @modelArgs }
Run-Native { python .github/scripts/windows-payload.py }
Run-Native { dotnet tool install wix --version 4.0.6 --tool-path artifacts/wix }
Run-Wix { ./artifacts/wix/wix.exe extension add --global WixToolset.Bal.wixext/4.0.6 }
$version = (Get-Content artifacts/windows/version.txt -Raw).Trim()
Run-Wix { ./artifacts/wix/wix.exe build packaging/windows/Package.wxs artifacts/windows/Payload.wxs -arch x64 -d "Version=$version" -o artifacts/windows/Kanade.msi }
Sign-File (Resolve-Path artifacts/windows/Kanade.msi).Path
Run-Wix { ./artifacts/wix/wix.exe build packaging/windows/Bundle.wxs -ext WixToolset.Bal.wixext -arch x64 -d "Version=$version" -o artifacts/windows/KanadeSetup.exe }
New-Item -ItemType Directory -Force artifacts/release | Out-Null
$destination = 'artifacts/release/kanade-ime-x86_64-pc-windows-msvc.exe'
if ($Unsigned) {
    Copy-Item artifacts/windows/KanadeSetup.exe $destination
} else {
    Run-Wix { ./artifacts/wix/wix.exe burn detach artifacts/windows/KanadeSetup.exe -engine artifacts/windows/BurnEngine.exe }
    Sign-File (Resolve-Path artifacts/windows/BurnEngine.exe).Path
    Run-Wix { ./artifacts/wix/wix.exe burn reattach artifacts/windows/KanadeSetup.exe -engine artifacts/windows/BurnEngine.exe -o $destination }
    Sign-File (Resolve-Path $destination).Path
}
Remove-Item artifacts/release/kanade-ime-x86_64-pc-windows-msvc.zip -Force -ErrorAction SilentlyContinue
