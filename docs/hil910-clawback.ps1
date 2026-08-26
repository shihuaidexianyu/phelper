# HIL-9: manual fan hold 300s + injected CTRL_BREAK at t~20s → graceful
# restore (journal origin=shutdown), fans leave 4000 within seconds.
# HIL-10 (run after): manual fan hold 300s + taskkill /F at t~20s → NO
# graceful restore; ONE sample at kill+150s must show firmware-auto (≠4000)
# — the AR-12 clawback proof. No intermediate sampling: every phelper
# engine shutdown restores auto, which would fake the clawback.
$ErrorActionPreference = "Continue"
$cli = ".\target\debug\phelper-cli.exe"
$log = "$env:TEMP\ph-hil910.log"
function Stamp($msg) { "[{0}] {1}" -f (Get-Date -Format "HH:mm:ss"), $msg | Out-File -Append $log }
function FanNow($label) {
  $line = & $cli telemetry --duration 4 --interval-ms 500 --metrics fan 2>$null | Select-String "fan\." | Select-Object -Last 2
  Stamp "SAMPLE $label :: $($line -join ' | ')"
}
Remove-Item $log -ErrorAction SilentlyContinue

Add-Type -Namespace Win32 -Name NativeConsole -MemberDefinition @"
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool AttachConsole(uint dwProcessId);
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool FreeConsole();
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool GenerateConsoleCtrlEvent(uint dwCtrlEvent, uint dwProcessGroupId);
[DllImport("kernel32.dll", SetLastError=true)] public static extern System.IntPtr SetConsoleCtrlHandler(System.IntPtr handler, bool add);
"@

# ---------------- HIL-9 ----------------
Stamp "HIL-9 START: fan manual 4000/4000 --hold 300, CTRL_BREAK at t~20s"
$proc = Start-Process -FilePath $cli -ArgumentList "control","fan","manual","--cpu","4000","--gpu","4000","--hold","300" -RedirectStandardOutput "$env:TEMP\ph-hil9.out" -RedirectStandardError "$env:TEMP\ph-hil9.err" -PassThru
Start-Sleep -Seconds 20
FanNow "t~20s pre-break (expect 4000/4000)"
[Win32.NativeConsole]::FreeConsole() | Out-Null
$attached = [Win32.NativeConsole]::AttachConsole($proc.Id)
[Win32.NativeConsole]::SetConsoleCtrlHandler([System.IntPtr]::Zero, $true) | Out-Null
$sent = [Win32.NativeConsole]::GenerateConsoleCtrlEvent(1, $proc.Id)
[Win32.NativeConsole]::FreeConsole() | Out-Null
[Win32.NativeConsole]::SetConsoleCtrlHandler([System.IntPtr]::Zero, $false) | Out-Null
Stamp "CTRL_BREAK sent (attach=$attached send=$sent) to pid $($proc.Id)"
$exited = $proc.WaitForExit(20000)
Stamp "hold process exited=$exited code=$(if ($exited) { $proc.ExitCode } else { 'TIMEOUT — killed'; $proc.Kill() })"
Start-Sleep -Seconds 10
FanNow "post-break +10s (expect firmware auto, NOT 4000 — graceful restore worked)"

# ---------------- HIL-10 ----------------
Stamp "HIL-10 START: fan manual 4000/4000 --hold 300, taskkill /F at t~20s"
$proc2 = Start-Process -FilePath $cli -ArgumentList "control","fan","manual","--cpu","4000","--gpu","4000","--hold","300" -RedirectStandardOutput "$env:TEMP\ph-hil10.out" -RedirectStandardError "$env:TEMP\ph-hil10.err" -PassThru -NoNewWindow
Start-Sleep -Seconds 20
Stamp "taskkill /F pid $($proc2.Id)"
& taskkill /F /PID $proc2.Id | Out-Null
$proc2.WaitForExit(5000) | Out-Null
Stamp "killed (exit code $($proc2.ExitCode)); heartbeat dead — waiting 150s for firmware clawback (NO sampling in between)"
Start-Sleep -Seconds 150
FanNow "kill+150s (AR-12: expect firmware auto ≠ 4000 — clawback proof)"
Stamp "DONE"
