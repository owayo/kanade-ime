# Reads the brand icon of a Kanade input DLL the way Windows resolves it for
# the input switcher and the taskbar's input indicator. Dot-source this file.
# It reads files and the registry and changes nothing.
if (-not ('KanadeIconFile' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;

public static class KanadeIconFile
{
    const uint AsDataFile = 0x2;
    const uint AsImageResource = 0x20;
    static readonly IntPtr GroupIcon = new IntPtr(14);
    static readonly IntPtr Icon = new IntPtr(3);

    [UnmanagedFunctionPointer(CallingConvention.Winapi)]
    delegate bool NameCallback(IntPtr module, IntPtr type, IntPtr name, IntPtr parameter);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr LoadLibraryExW(string path, IntPtr file, uint flags);
    [DllImport("kernel32.dll")]
    static extern bool FreeLibrary(IntPtr module);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool EnumResourceNamesW(IntPtr module, IntPtr type, NameCallback callback, IntPtr parameter);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr FindResourceW(IntPtr module, IntPtr name, IntPtr type);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr LoadResource(IntPtr module, IntPtr resource);
    [DllImport("kernel32.dll")]
    static extern IntPtr LockResource(IntPtr data);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint SizeofResource(IntPtr module, IntPtr resource);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern uint PrivateExtractIconsW(string file, int index, int width, int height, IntPtr[] icons, uint[] identifiers, uint count, uint flags);
    [DllImport("user32.dll")]
    static extern bool DestroyIcon(IntPtr icon);

    static IntPtr Open(string path)
    {
        IntPtr module = LoadLibraryExW(path, IntPtr.Zero, AsDataFile | AsImageResource);
        if (module == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(), "Cannot read resources of " + path);
        return module;
    }

    static byte[] Read(string path, IntPtr type, int identifier)
    {
        IntPtr module = Open(path);
        try
        {
            IntPtr found = FindResourceW(module, new IntPtr(identifier), type);
            if (found == IntPtr.Zero) throw new InvalidOperationException("Resource " + identifier + " is missing in " + path);
            IntPtr data = LoadResource(module, found);
            if (data == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
            byte[] bytes = new byte[SizeofResource(module, found)];
            Marshal.Copy(LockResource(data), bytes, 0, bytes.Length);
            return bytes;
        }
        finally { FreeLibrary(module); }
    }

    // Identifiers of the icon groups, in the order a position counts them.
    public static int[] Groups(string path)
    {
        List<int> groups = new List<int>();
        NameCallback collect = delegate(IntPtr module, IntPtr type, IntPtr name, IntPtr parameter)
        {
            // A numbered resource passes its number where a named one passes a pointer.
            long value = name.ToInt64();
            groups.Add(value < 0x10000 ? (int)value : -1);
            return true;
        };
        IntPtr file = Open(path);
        try
        {
            // Fails with "resource type not found" when the file has no icon.
            if (!EnumResourceNamesW(file, GroupIcon, collect, IntPtr.Zero)) groups.Clear();
        }
        finally { FreeLibrary(file); }
        GC.KeepAlive(collect);
        return groups.ToArray();
    }

    public static byte[] Group(string path, int identifier) { return Read(path, GroupIcon, identifier); }

    public static byte[] Bitmap(string path, int identifier) { return Read(path, Icon, identifier); }

    // The bitmap resource Windows picks for an icon position and a size.
    public static int Pick(string path, int index, int size)
    {
        IntPtr[] icons = new IntPtr[1];
        uint[] identifiers = new uint[1];
        uint found = PrivateExtractIconsW(path, index, size, size, icons, identifiers, 1, 0);
        if (found != 1 || icons[0] == IntPtr.Zero) throw new InvalidOperationException("Windows found no " + size + " pixel icon at position " + index + " of " + path);
        DestroyIcon(icons[0]);
        return (int)identifiers[0];
    }
}
'@
}

# Checks the icon at $Index of $Path against Microsoft's requirements for an
# input method's brand icon: 16 to 48 pixels in six sizes, 32 bits with an
# alpha channel. Returns its sizes and a digest of its directory and bitmaps.
function Get-BrandIcon([string]$Path, [int]$Index) {
    $groups = [KanadeIconFile]::Groups($Path)
    # A negative position names a group by its identifier.
    $group = if ($Index -lt 0) { -$Index } elseif ($Index -lt $groups.Length) { $groups[$Index] } else { -1 }
    if ($group -lt 1) { throw "No icon at position $Index of $Path (icon groups: $($groups.Length))" }
    $directory = [KanadeIconFile]::Group($Path, $group)
    $count = [BitConverter]::ToUInt16($directory, 4)
    if ([BitConverter]::ToUInt16($directory, 2) -ne 1 -or $directory.Length -ne 6 + 14 * $count) { throw "Malformed icon directory in $Path" }
    $content = [System.IO.MemoryStream]::new()
    $content.Write($directory, 0, $directory.Length)
    $sizes = for ($entry = 0; $entry -lt $count; $entry++) {
        $at = 6 + 14 * $entry
        $size = [int]$directory[$at]
        $identifier = [BitConverter]::ToUInt16($directory, $at + 12)
        $bitmap = [KanadeIconFile]::Bitmap($Path, $identifier)
        $content.Write($bitmap, 0, $bitmap.Length)
        if ($directory[$at + 1] -ne $size -or [BitConverter]::ToUInt16($directory, $at + 6) -ne 32) { throw "Icon entry $entry of $Path is not a square 32-bit image" }
        if ([BitConverter]::ToUInt32($directory, $at + 8) -ne $bitmap.Length) { throw "Icon entry $entry of $Path has the wrong length" }
        # BITMAPINFOHEADER: the height counts the colour rows and the mask rows.
        if ([BitConverter]::ToInt32($bitmap, 4) -ne $size -or [BitConverter]::ToInt32($bitmap, 8) -ne 2 * $size -or
            [BitConverter]::ToUInt16($bitmap, 14) -ne 32 -or [BitConverter]::ToUInt32($bitmap, 16) -ne 0) {
            throw "The $size pixel bitmap of $Path is not an uncompressed 32-bit bitmap"
        }
        $alpha = for ($pixel = 0; $pixel -lt $size * $size; $pixel++) { $bitmap[40 + 4 * $pixel + 3] }
        if (-not ($alpha -eq 255) -or -not ($alpha | Where-Object { $_ -gt 0 -and $_ -lt 255 })) { throw "The $size pixel bitmap of $Path does not use its alpha channel" }
        if ([KanadeIconFile]::Pick($Path, $Index, $size) -ne $identifier) { throw "Windows does not pick the $size pixel bitmap at position $Index of $Path" }
        $size
    }
    if (Compare-Object @($sizes | Sort-Object) @(16, 20, 24, 32, 40, 48)) { throw "Icon sizes of $Path are $($sizes -join ', ')" }
    $hash = [System.Security.Cryptography.SHA256]::Create()
    try { $digest = [BitConverter]::ToString($hash.ComputeHash($content.ToArray())) -replace '-' }
    finally { $hash.Dispose() }
    [pscustomobject]@{ Path = $Path; Index = $Index; Sizes = $sizes; Digest = $digest }
}

# The brand icon that the registered Japanese Kanade profile points to. The
# profile is shared by the 64-bit and the 32-bit registry view.
function Get-RegisteredBrandIcon {
    $languageProfile = 'SOFTWARE\Microsoft\CTF\TIP\{6DAFA84E-C56A-42BE-983E-0D18F3DC5B21}\LanguageProfile\0x00000411\{A1C81465-D31B-41D4-B7C4-35641955F560}'
    $registered = foreach ($view in @('Registry64', 'Registry32')) {
        $registry = [Microsoft.Win32.RegistryKey]::OpenBaseKey('LocalMachine', $view)
        try {
            $key = $registry.OpenSubKey($languageProfile)
            if (-not $key) { throw "The Kanade input profile is not registered in $view" }
            try {
                $file = $key.GetValue('IconFile')
                $index = $key.GetValue('IconIndex')
                if (-not $file -or $null -eq $index) { throw "The Kanade input profile names no icon in $view" }
                [pscustomobject]@{ File = [string]$file; Index = [int]$index }
            } finally { $key.Dispose() }
        } finally { $registry.Dispose() }
    }
    if ($registered[0].File -ne $registered[1].File -or $registered[0].Index -ne $registered[1].Index) { throw 'The registry views name different brand icons' }
    if (-not (Test-Path -LiteralPath $registered[0].File -PathType Leaf)) { throw "Registered icon file is missing: $($registered[0].File)" }
    Get-BrandIcon $registered[0].File $registered[0].Index
}
