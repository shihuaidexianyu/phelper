# HIL-8: manual fan 3000/3000 held 150s; RPM sampled at key timeline points.
# Each sample is a short telemetry engine run; its shutdown-restore clobbers
# the held state, and the holder's 60s keepalive re-asserts — also observed.
$ErrorActionPreference = "Continue"
$cli = ".\target\debug\phelper-cli.exe"
function Stamp($msg) { "[{0}] {1}" -f (Get-Date -Format "HH:mm:ss"), $msg | Out-File -Append $env:TEMP\ph-hil8.log }
function Sample($label) {
  $line = & $cli telemetry --duration 4 --interval-ms 500 --metrics fan 2>$null | Select-String "fan\." | Select-Object -Last 2
  Stamp "SAMPLE $label :: $($line -join ' | ')"
}
Remove-Item $env:TEMP\ph-hil8.log -ErrorAction SilentlyContinue
Stamp "START hold process (fan manual 3000/3000 --hold 150)"
$hold = Start-Process -FilePath $cli -ArgumentList "control","fan","manual","--cpu","3000","--gpu","3000","--hold","150" -RedirectStandardOutput "$env:TEMP\ph-hil8-hold.out" -RedirectStandardError "$env:TEMP\ph-hil8-hold.err" -PassThru -NoNewWindow
Start-Sleep -Seconds 15
Sample "t~15s (post-write, expect Verified ~3000)"
Start-Sleep -Seconds 50
Sample "t~65s (after keepalive tick #1 re-asserts the sample#1 sabotage)"
Start-Sleep -Seconds 60
Sample "t~125s (PAST 120s clawback window — heartbeat proof)"
$hold.WaitForExit(60000) | Out-Null
Stamp "hold process exited code=$($hold.ExitCode)"
Start-Sleep -Seconds 8
Sample "t~160s (after graceful restore — expect firmware auto, not 3000)"
Stamp "DONE"
