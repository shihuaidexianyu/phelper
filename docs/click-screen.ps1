# click-screen.ps1 — left-click at PHYSICAL screen coordinates (M6 HIL
# helper). DPI-aware like capture-window.ps1: pair them — read the PNG
# (PNG pixel (0,0) = window top-left physical), add the window's physical
# rect origin, click there.
param(
    [Parameter(Mandatory = $true)][int]$X,
    [Parameter(Mandatory = $true)][int]$Y,
    [int]$SettleMs = 300
)
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class DpiAwareClick {
  [DllImport("shcore.dll")] public static extern int SetProcessDpiAwareness(int v);
}
public class Win32Click {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
}
"@
[DpiAwareClick]::SetProcessDpiAwareness(2) | Out-Null
[Win32Click]::SetCursorPos($X, $Y) | Out-Null
Start-Sleep -Milliseconds 120
[Win32Click]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero) # LEFTDOWN
Start-Sleep -Milliseconds 60
[Win32Click]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero) # LEFTUP
Start-Sleep -Milliseconds $SettleMs
Write-Output "clicked ($X,$Y)"
