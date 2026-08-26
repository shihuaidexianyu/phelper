# scroll-screen.ps1 — mouse-wheel scroll at PHYSICAL screen coordinates
# (M6 HIL helper). Negative Clicks = down. DPI-aware; pair with
# capture-window.ps1 coordinates.
param(
    [Parameter(Mandatory = $true)][int]$X,
    [Parameter(Mandatory = $true)][int]$Y,
    [int]$Clicks = -6,
    [int]$SettleMs = 400
)
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class DpiAwareScroll {
  [DllImport("shcore.dll")] public static extern int SetProcessDpiAwareness(int v);
}
public class Win32Scroll {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, int d, UIntPtr e);
}
"@
[DpiAwareScroll]::SetProcessDpiAwareness(2) | Out-Null
[Win32Scroll]::SetCursorPos($X, $Y) | Out-Null
Start-Sleep -Milliseconds 120
[Win32Scroll]::mouse_event(0x0800, 0, 0, 120 * $Clicks, [UIntPtr]::Zero) # MOUSEEVENTF_WHEEL
Start-Sleep -Milliseconds $SettleMs
Write-Output "scrolled $Clicks clicks at ($X,$Y)"
