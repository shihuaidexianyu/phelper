# HIL-14: watchdog pre-write gate (R4). With the PawnIO module absent, the
# engine has NO fresh cpu.pkg_temp_c → manual fan must be rejected as
# UnsafeRequest (the safety net never flies blind). Then restore the module
# and confirm the gate opens again.
$ErrorActionPreference = "Continue"
$cli = ".\target\debug\phelper-cli.exe"
$bin = ".\assets\pawnio\IntelMSR.bin"
$away = ".\assets\pawnio\IntelMSR.bin.hold"
$log = "$env:TEMP\ph-hil14.log"
function Stamp($msg) { "[{0}] {1}" -f (Get-Date -Format "HH:mm:ss"), $msg | Out-File -Append $log }
Remove-Item $log -ErrorAction SilentlyContinue

Move-Item $bin $away
Stamp "IntelMSR.bin moved away — engine will have no temp feed"
$out = & $cli control fan manual --cpu 3000 --gpu 3000 --hold 0 2>&1 | Out-String
($out -split "`n" | Select-String "pawnio provider unavailable|status:|COMMAND") | ForEach-Object { Stamp "  $($_.Line.Trim())" }
Move-Item $away $bin
Stamp "IntelMSR.bin restored"
$out2 = & $cli control fan manual --cpu 3000 --gpu 3000 --hold 8 2>&1 | Out-String
($out2 -split "`n" | Select-String "status:|verification:") | ForEach-Object { Stamp "  $($_.Line.Trim())" }
Stamp "DONE (positive control: --hold 8 with temp feed back)"
