$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System.Runtime.InteropServices;
public static class KanadeInputProfile {
    [DllImport("input.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool InstallLayoutOrTip(string profile, uint flags);
}
'@
$profileId = '0x0411:{6DAFA84E-C56A-42BE-983E-0D18F3DC5B21}{A1C81465-D31B-41D4-B7C4-35641955F560}'
if (-not [KanadeInputProfile]::InstallLayoutOrTip($profileId, 0)) {
    throw 'Windows could not enable the Japanese Kanade input profile for the current user.'
}
