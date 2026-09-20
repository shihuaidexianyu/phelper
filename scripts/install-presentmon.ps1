param(
    [string]$ToolsDir = (Join-Path $env:LOCALAPPDATA 'phelper\tools')
)

$ErrorActionPreference = 'Stop'
$taskTools = $ToolsDir
New-Item -ItemType Directory -Force -Path $taskTools | Out-Null
$taskTarget = Join-Path $taskTools 'PresentMon-2.5.1-x64.exe'
$taskExpected = '9bec3083069f58f911e6a512f4806db51a27bd096103087bc1d05ef54c80a191'
if (Test-Path -LiteralPath $taskTarget) {
    if ((Get-FileHash -LiteralPath $taskTarget -Algorithm SHA256).Hash -ine $taskExpected) { throw 'Existing PresentMon binary has an unexpected hash; it was not replaced.' }
} else {
    $taskDownload = Join-Path $taskTools ('presentmon-' + [guid]::NewGuid().ToString('N') + '.download')
    Invoke-WebRequest -Uri 'https://github.com/GameTechDev/PresentMon/releases/download/v2.5.1/PresentMon-2.5.1-x64.exe' -OutFile $taskDownload
    if ((Get-FileHash -LiteralPath $taskDownload -Algorithm SHA256).Hash -ine $taskExpected) { throw 'PresentMon download did not match the GitHub release SHA-256.' }
    Move-Item -LiteralPath $taskDownload -Destination $taskTarget
}
Write-Output "PresentMon 2.5.1 installed: $taskTarget"
Write-Output 'Source and MIT license: https://github.com/GameTechDev/PresentMon/tree/v2.5.1'
