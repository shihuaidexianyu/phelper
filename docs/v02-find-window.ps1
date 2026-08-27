# Find ANY visible-or-cloaked top-level window whose title contains a
# substring, system-wide (virtual-desktop cloaking included).
param([string]$Needle = "phelper")
$sig = @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Win32Find {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern int GetWindowTextLengthW(IntPtr h);
  [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int attr, out int val, int size);
}
"@
Add-Type -TypeDefinition $sig
$hits = 0
$cb = [Win32Find+EnumProc]{ param($h, $l)
    if ([Win32Find]::GetWindowTextLengthW($h) -gt 0) {
        $sb = New-Object System.Text.StringBuilder 512
        [void][Win32Find]::GetWindowTextW($h, $sb, 512)
        $t = $sb.ToString()
        if ($t -like "*$Needle*") {
            $pid2 = 0
            [void][Win32Find]::GetWindowThreadProcessId($h, [ref]$pid2)
            $cloaked = 0
            [void][Win32Find]::DwmGetWindowAttribute($h, 14, [ref]$cloaked, 4)
            $vis = [Win32Find]::IsWindowVisible($h)
            Write-Output ("hwnd={0} pid={1} visible={2} cloaked={3} title=[{4}]" -f $h.ToInt64(), $pid2, $vis, $cloaked, $t)
            $script:hits++
        }
    }
    return $true
}
[void][Win32Find]::EnumWindows($cb, [IntPtr]::Zero)
if ($hits -eq 0) { Write-Output "no window titled *$Needle* anywhere" }
