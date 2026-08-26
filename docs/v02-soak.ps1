# v0.2 soak sampler: total CPU-seconds + WorkingSet64 every 5s.
# Compute deltas from the CPU column (it is CUMULATIVE — the M6 artifact).
param(
    [string]$ProcessName = "phelper-desktop",
    [string]$OutCsv = "C:\Users\exqin\AppData\Local\Temp\v02-soak.csv",
    [int]$Seconds = 300
)
$name = $ProcessName
$rows = @()
$start = Get-Date
for ($i = 0; $i -lt [int]($Seconds / 5); $i++) {
    $p = Get-Process -Name $name -ErrorAction SilentlyContinue
    if ($null -eq $p) {
        $rows += [pscustomobject]@{ t = [int]((Get-Date) - $start).TotalSeconds; cpu_s = ""; ws_mb = ""; note = "process-missing" }
    } else {
        $rows += [pscustomobject]@{
            t = [int]((Get-Date) - $start).TotalSeconds
            cpu_s = [math]::Round($p.CPU, 2)
            ws_mb = [math]::Round($p.WorkingSet64 / 1MB, 1)
            note = ""
        }
    }
    Start-Sleep -Seconds 5
}
$rows | Export-Csv -Path $OutCsv -NoTypeInformation
Write-Output "wrote $OutCsv ($($rows.Count) rows)"
