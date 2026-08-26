# HIL-13: thermal hysteresis safety net (§33.1). Fan manual 2000/2000 (slowest
# clamp) + all-core load → pkg ≥90°C → SafetySupervisor forces max fan
# (journal origin=safety ForceMaxFan); load stops → ≤85°C → ReleaseTo(Manual).
# Evidence = journal only: no telemetry sampling during the hot phase (an
# external engine's shutdown restore would disarm the forced max fan).
$ErrorActionPreference = "Continue"
$cli = ".\target\debug\phelper-cli.exe"
$log = "$env:TEMP\ph-hil13.log"
$journal = "$env:LOCALAPPDATA\phelper\state\control-journal.jsonl"
function Stamp($msg) { "[{0}] {1}" -f (Get-Date -Format "HH:mm:ss"), $msg | Out-File -Append $log }
Remove-Item $log -ErrorAction SilentlyContinue
$journalLinesBefore = (Get-Content $journal | Measure-Object -Line).Lines

Stamp "START: fan manual 2000/2000 --hold 300, load at t~15s"
$hold = Start-Process -FilePath $cli -ArgumentList "control","fan","manual","--cpu","2000","--gpu","2000","--hold","300" -RedirectStandardOutput "$env:TEMP\ph-hil13-hold.out" -RedirectStandardError "$env:TEMP\ph-hil13-hold.err" -PassThru -NoNewWindow
Start-Sleep -Seconds 15

$jobs = 1..32 | ForEach-Object { Start-Job { $end = [DateTime]::Now.AddSeconds(240); while ([DateTime]::Now -lt $end) {} } }
Stamp "32 busy-loop jobs running; polling journal for ForceMaxFan…"

$forceSeen = $false
$releaseSeen = $false
$sw = [Diagnostics.Stopwatch]::StartNew()
while ($sw.Elapsed.TotalSeconds -lt 270) {
  Start-Sleep -Seconds 8
  $new = Get-Content $journal | Select-Object -Skip $journalLinesBefore | Select-String '"origin":"safety"'
  if (-not $forceSeen -and ($new | Select-String "SAFETY max-fan on")) {
    $forceSeen = $true
    Stamp "ForceMaxFan journaled at +$([int]$sw.Elapsed.TotalSeconds)s — killing load"
    $jobs | Stop-Job -PassThru | Remove-Job
    $jobs = @()
  }
  if ($forceSeen -and -not $releaseSeen -and ($new | Select-String '"manual"')) {
    $releaseSeen = $true
    Stamp "ReleaseTo(Manual) journaled at +$([int]$sw.Elapsed.TotalSeconds)s"
    break
  }
}
$jobs | Stop-Job -PassThru | Remove-Job -ErrorAction SilentlyContinue
if (-not $forceSeen) { Stamp "FAIL: ForceMaxFan never journaled within 270s" }
if ($forceSeen -and -not $releaseSeen) { Stamp "WARN: ReleaseTo not seen within window (manual check needed)" }

Stamp "safety evidence (journal, origin=safety):"
Get-Content $journal | Select-Object -Skip $journalLinesBefore | Select-String '"origin":"safety"' | ForEach-Object { Stamp "  $_" }

# Machine is safe now (≤85°C, manual 2000 or already restored). End the hold.
if (-not $hold.HasExited) {
  Stamp "taskkill hold process (clawback already proven in HIL-10)"
  & taskkill /F /PID $hold.Id | Out-Null
}
Start-Sleep -Seconds 5
$FanLine = & $cli telemetry --duration 4 --interval-ms 500 --metrics fan --metrics pkg_temp 2>$null | Select-String "fan\.|pkg_temp" | Select-Object -Last 3
Stamp "final state sample :: $($FanLine -join ' | ')"
Stamp "DONE"
