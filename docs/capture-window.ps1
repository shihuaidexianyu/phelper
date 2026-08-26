# capture-window.ps1 — grab a window's screen region to PNG (M6 dev/HIL
# helper; GPU-rendered windows like GPUI/DX11 capture BLACK via PrintWindow,
# so this brings the window to front and uses CopyFromScreen instead).
param(
    [Parameter(Mandatory = $true)][string]$ProcessName,
    [string]$Out = "capture.png",
    [int]$SettleMs = 2500
)
Add-Type -AssemblyName System.Drawing
# CRITICAL: this process must be DPI-aware or GetWindowRect returns
# VIRTUALIZED (logical) coordinates while CopyFromScreen reads PHYSICAL
# pixels — on a 150% display that mismatch captured only the top-left
# 2/3 of the target window (M6-D4 layout "bug" that was actually a
# capture artifact; red-box calibration proved layout correct).
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class DpiAware {
  [DllImport("shcore.dll")] public static extern int SetProcessDpiAwareness(int v);
}
"@
[DpiAware]::SetProcessDpiAwareness(2) | Out-Null # PROCESS_PER_MONITOR_DPI_AWARE
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win32Cap {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
  public struct RECT { public int Left, Top, Right, Bottom; }
}
"@
$proc = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue |
    Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $proc) { Write-Error "no windowed process '$ProcessName'"; exit 1 }
[Win32Cap]::ShowWindow($proc.MainWindowHandle, 9) | Out-Null # SW_RESTORE
[Win32Cap]::SetForegroundWindow($proc.MainWindowHandle) | Out-Null
# Foreground-stealing is denied for background processes — force topmost
# instead (HWND_TOPMOST=-1, SWP_NOMOVE|SWP_NOSIZE=0x3), then drop it.
[Win32Cap]::SetWindowPos($proc.MainWindowHandle, [IntPtr](-1), 0, 0, 0, 0, 0x3) | Out-Null
Start-Sleep -Milliseconds $SettleMs
[Win32Cap]::SetWindowPos($proc.MainWindowHandle, [IntPtr](-2), 0, 0, 0, 0, 0x3) | Out-Null
$r = New-Object Win32Cap+RECT
[Win32Cap]::GetWindowRect($proc.MainWindowHandle, [ref]$r) | Out-Null
$w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
if ($w -le 0 -or $h -le 0) { Write-Error "bad rect ($w x $h)"; exit 1 }
$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size)
$path = Join-Path (Get-Location) $Out
$bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
Write-Output "saved $path ($w x $h)"
