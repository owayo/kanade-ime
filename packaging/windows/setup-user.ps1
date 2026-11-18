$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'enable-profile.ps1')
& (Join-Path $PSScriptRoot 'start-engine.ps1')
Start-Process -FilePath (Join-Path $PSScriptRoot 'kanade-ui.exe')
