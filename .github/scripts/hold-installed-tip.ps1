# Disposable installer rehearsal. Simulate an editor that has loaded the IME.
param([Parameter(Mandatory)][string]$ReadyFile)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class TipHost {
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern IntPtr LoadLibrary(string path);
}
'@
$tip = Join-Path $env:ProgramFiles 'Kanade/kanade_tip.dll'
if ([TipHost]::LoadLibrary($tip) -eq [IntPtr]::Zero) { throw 'Cannot load installed TIP' }
$form = [System.Windows.Forms.Form]::new()
$form.Text = 'Installer test editor'
$form.ShowInTaskbar = $false
$form.Add_Shown({ [IO.File]::WriteAllText($ReadyFile, [string]$PID) })
[System.Windows.Forms.Application]::Run($form)
