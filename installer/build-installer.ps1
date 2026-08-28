param(
    [string]$BuildDir = "",
    [string]$Version = "0.1.0",
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot ".." )).Path
$issPath = Join-Path $PSScriptRoot "phelper.iss"

if ([string]::IsNullOrWhiteSpace($BuildDir)) {
    $BuildDir = Join-Path $repoRoot "target\release"
} elseif (-not [IO.Path]::IsPathRooted($BuildDir)) {
    $BuildDir = Join-Path $repoRoot $BuildDir
}

if (-not $SkipBuild) {
    Push-Location $repoRoot
    try {
        cargo build -p phelper-desktop --release --features experimental
        if ($LASTEXITCODE -ne 0) {
            throw "release build failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
}

$BuildDir = (Resolve-Path $BuildDir).Path
$exePath = Join-Path $BuildDir "phelper-desktop.exe"
if (-not (Test-Path -LiteralPath $exePath -PathType Leaf)) {
    throw "release executable not found: $exePath"
}

$isccCandidates = @(
    (Get-Command iscc.exe -ErrorAction SilentlyContinue).Source,
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles}\Inno Setup 6\ISCC.exe"
) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) -and (Test-Path -LiteralPath $_) }
$iscc = $isccCandidates | Select-Object -First 1
if ([string]::IsNullOrWhiteSpace($iscc)) {
    throw "ISCC.exe not found; install Inno Setup 6 first"
}

$distDir = Join-Path $repoRoot "dist"
New-Item -ItemType Directory -Path $distDir -Force | Out-Null
Push-Location $repoRoot
try {
    & $iscc "/Qp" "/DMyAppVersion=$Version" "/DBuildDir=$BuildDir" $issPath
    if ($LASTEXITCODE -ne 0) {
        throw "Inno Setup compilation failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}

$installerPath = Join-Path $distDir "phelper-Setup-$Version.exe"
Get-Item -LiteralPath $installerPath | Select-Object FullName, Length, LastWriteTime
Get-FileHash -LiteralPath $installerPath -Algorithm SHA256 | Select-Object Algorithm, Hash, Path
