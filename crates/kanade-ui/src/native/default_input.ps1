param([bool]$Apply)
$ErrorActionPreference = 'Stop'
$tip = '0411:{6DAFA84E-C56A-42BE-983E-0D18F3DC5B21}{A1C81465-D31B-41D4-B7C4-35641955F560}'
Import-Module (Join-Path $PSHOME 'Modules/International/International.psd1')
if ($Apply) {
    $enabled = Get-WinUserLanguageList | ForEach-Object { $_.InputMethodTips }
    if ($enabled -notcontains $tip) { exit 1 }
    Set-WinDefaultInputMethodOverride -InputTip $tip
}
if ((Get-WinDefaultInputMethodOverride).InputMethodTip -eq $tip) { exit 0 }
exit 3
