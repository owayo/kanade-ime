# Run in the same checkout and Windows user session on either side of an update.
param([Parameter(Mandatory)][ValidateSet('BeforeUpdate', 'AfterUpdate', 'AfterReboot')][string]$Stage)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().ToString('o')
Write-Host "$Stage (last Windows boot $boot)"

# The in-process TIP uses its own test pipe. The second process instead selects
# the registered TIP in a Rich Edit window and exercises Windows character input.
cargo test --locked -p kanade-tip --lib -- native::host::key_routing_ native::host::reentry_ --test-threads=1 | Out-Host
$hostExit = $LASTEXITCODE
cargo test --locked -p kanade-tip --test windows_direct_input -- --ignored --exact installed_profile_direct_input --test-threads=1 | Out-Host
$profileExit = $LASTEXITCODE
Write-Host "$Stage result: in-process host=$hostExit, registered profile=$profileExit"
if ($hostExit -ne 0 -or $profileExit -ne 0) { exit 1 }
exit 0
