# Capture a window by explicit hwnd (works when Get-Process.MainWindowHandle is 0).
param([long]$Hwnd, [Parameter(Mandatory=$true)][string]$Out)
Add-Type -AssemblyName System.Drawing
$sig = @"
using System;
using System.Runtime.InteropServices;
public class Win32CapH {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  public struct RECT { public int Left, Top, Right, Bottom; }
}
"@
Add-Type -TypeDefinition $sig
[Win32CapH]::SetProcessDPIAware() | Out-Null
$h = [IntPtr]::new($Hwnd)
$r = New-Object Win32CapH+RECT
[Win32CapH]::GetWindowRect($h, [ref]$r) | Out-Null
$w = $r.Right - $r.Left; $hh = $r.Bottom - $r.Top
if ($w -le 0 -or $hh -le 0) { Write-Error "bad rect $($r.Left),$($r.Top) ${w}x${hh}"; exit 1 }
$bmp = New-Object System.Drawing.Bitmap $w, $hh
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size)
$outPath = Join-Path (Get-Location) $Out
$bmp.Save($outPath, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
Write-Output "saved $outPath ($w x $hh)"
