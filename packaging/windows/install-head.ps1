# Invoked by install.bat in a Git checkout. A package version can span several commits.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-Git {
    $output = & git @args
    if ($LASTEXITCODE -ne 0) { throw "Git failed ($LASTEXITCODE): $($args[0])" }
    $output
}

$root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '../..')).Path
$commit = Invoke-Git -C $root rev-parse --verify HEAD
$name = 'kanade-ime-x86_64-pc-windows-msvc.exe'
$cache = Join-Path $root "artifacts/windows/install-head/$commit"
$setup = Join-Path $cache $name
$checksum = "$setup.sha256"

function Build-HeadInstaller {
    Write-Host "Building the Windows installer for $commit."
    $source = $root
    $scratch = $null
    try {
        # Build committed sources even when there are staged, unstaged or new files.
        if (Invoke-Git -C $root status --porcelain --untracked-files=normal --ignore-submodules=none) {
            $scratch = Join-Path ([IO.Path]::GetTempPath()) ("kanade-head-" + [guid]::NewGuid().ToString('N'))
            Invoke-Git clone --local --no-checkout -- $root $scratch
            Invoke-Git -C $scratch checkout --detach $commit
            $source = $scratch
        }
        Push-Location -LiteralPath $source
        try {
            Invoke-Git submodule update --init --recursive
            # stage-windows reads the release binaries from this checkout's target/.
            $env:CARGO_TARGET_DIR = Join-Path $source 'target'
            $localSdk = Join-Path $root '.tools/dotnet-sdk'
            if (!(Get-Command dotnet.exe -ErrorAction SilentlyContinue) -and
                (Test-Path -LiteralPath (Join-Path $localSdk 'dotnet.exe'))) {
                $env:DOTNET_ROOT = $localSdk
                $env:Path = "$localSdk;$env:Path"
            }
            $arguments = @('-NoLogo', '-NoProfile', '-ExecutionPolicy', 'RemoteSigned',
                '-File', './.github/scripts/package-windows.ps1', '-Unsigned')
            if ((Test-Path -LiteralPath 'mise.toml') -and $env:SYSTEM_TOOLS -ne '1') {
                & mise exec -- powershell.exe @arguments
            } else {
                & powershell.exe @arguments
            }
            if ($LASTEXITCODE -ne 0) { throw "Windows packaging failed ($LASTEXITCODE)." }
            if ((Invoke-Git rev-parse HEAD) -ne $commit -or
                (Invoke-Git status --porcelain --untracked-files=normal --ignore-submodules=none)) {
                throw 'The source changed during packaging. Run install.bat again.'
            }
            Copy-Item -LiteralPath "artifacts/release/$name" -Destination $setup -Force
            (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash | Set-Content -LiteralPath $checksum -Encoding Ascii
        } finally { Pop-Location }
    } finally {
        if ($scratch -and (Test-Path -LiteralPath $scratch)) {
            $directory = Get-Item -LiteralPath $scratch
            if ($directory.Parent.FullName -ne (Get-Item -LiteralPath ([IO.Path]::GetTempPath())).FullName.TrimEnd('\') -or
                $directory.Name -notmatch '^kanade-head-[0-9a-f]{32}$') { throw 'Unexpected temporary build directory.' }
            Remove-Item -LiteralPath $directory.FullName -Recurse -Force
        }
    }
}

New-Item -ItemType Directory -Force -Path $cache | Out-Null
# Packaging uses one payload directory for the checkout, even across commits.
$lockPath = Join-Path $root 'artifacts/windows/install-head/build.lock'
$lock = [IO.File]::Open($lockPath, 'OpenOrCreate', 'ReadWrite', 'None')
try {
    if ((Test-Path -LiteralPath $setup -PathType Leaf) -and
        (Test-Path -LiteralPath $checksum -PathType Leaf) -and
        (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash -eq [IO.File]::ReadAllText($checksum).Trim()) {
        Write-Host "Using the verified installer for $commit."
    } else {
        Build-HeadInstaller
    }
    if ((Invoke-Git -C $root rev-parse HEAD) -ne $commit) { throw 'HEAD changed. Run install.bat again.' }
    $process = Start-Process -FilePath $setup -ArgumentList '/install','/passive','/norestart' -PassThru -Wait
} finally { $lock.Dispose() }
exit $process.ExitCode
