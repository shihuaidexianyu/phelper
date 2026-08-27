# Restore a hidden (tray-parked) phelper-desktop window: find the
# top-level window owned by the process (hidden windows report
# MainWindowHandle=0, so enumerate), then SW_RESTORE + foreground.
$sig = @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Win32Enum {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern int GetWindowTextLengthW(IntPtr h);
}
"@
Add-Type -TypeDefinition $sig
$target = (Get-Process phelper-desktop -ErrorAction Stop).Id
$found = [IntPtr]::Zero
$cb = [Win32Enum+EnumProc]{ param($h, $l)
    $pid2 = 0
    [void][Win32Enum]::GetWindowThreadProcessId($h, [ref]$pid2)
    if ($pid2 -eq $target -and [Win32Enum]::GetWindowTextLengthW($h) -gt 0) {
        $script:found = $h
        return $false
    }
    return $true
}
[void][Win32Enum]::EnumWindows($cb, [IntPtr]::Zero)
if ($found -eq [IntPtr]::Zero) { Write-Error "no top-level window found"; exit 1 }
[void][Win32Enum]::ShowWindow($found, 9)  # SW_RESTORE
[void][Win32Enum]::SetForegroundWindow($found)
Write-Output "restored hwnd=$($found.ToInt64())"
