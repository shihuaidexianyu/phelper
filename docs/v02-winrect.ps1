# Print the phelper-desktop main window's physical rect + click point for
# a PNG-space coordinate (mirrors capture-window.ps1's type pattern).
param([int]$PngX = 90, [int]$PngY = 526)
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win32Rect {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  public struct RECT { public int Left, Top, Right, Bottom; }
}
"@
[Win32Rect]::SetProcessDPIAware() | Out-Null
$p = Get-Process phelper-desktop -ErrorAction Stop
$r = New-Object Win32Rect+RECT
[Win32Rect]::GetWindowRect($p.MainWindowHandle, [ref]$r) | Out-Null
$w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
Write-Output "rect: $($r.Left),$($r.Top) - $($r.Right),$($r.Bottom) ($w x $h)"
$captureW = 2182; $captureH = 1406
$cx = $r.Left + [int]($PngX * ($w / $captureW)); $cy = $r.Top + [int]($PngY * ($h / $captureH))
Write-Output "click: $cx,$cy"
