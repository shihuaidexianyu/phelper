param(
    [string]$BuildDir = "",
    [string]$Version = "0.2.0",
    [string]$VCRuntimeDir = "",
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
        cargo build -p phelper-desktop --release --locked
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

$toolsDir = Join-Path $repoRoot "target\installer-support\tools"
& (Join-Path $repoRoot "scripts\install-presentmon.ps1") -ToolsDir $toolsDir

# Deploy the release CRT app-locally from Visual Studio's redistributable
# directory; do not copy system DLLs or install a machine-wide runtime.
if ([string]::IsNullOrWhiteSpace($VCRuntimeDir)) {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path -LiteralPath $vswhere) {
        $vsPath = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($vsPath) {
            $redist = Get-ChildItem -Path "$vsPath\VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT\vcruntime140.dll" |
                Sort-Object { [version]$_.VersionInfo.FileVersion } -Descending | Select-Object -First 1
            if ($redist) { $VCRuntimeDir = $redist.DirectoryName }
        }
    }
}
if ([string]::IsNullOrWhiteSpace($VCRuntimeDir) -or
    -not (Test-Path -LiteralPath (Join-Path $VCRuntimeDir "vcruntime140.dll") -PathType Leaf)) {
    throw "Visual C++ x64 redistributable DLL not found; specify -VCRuntimeDir from Visual Studio's VC\Redist directory"
}
$VCRuntimeDir = (Resolve-Path -LiteralPath $VCRuntimeDir).Path

$distDir = Join-Path $repoRoot "dist"
New-Item -ItemType Directory -Path $distDir -Force | Out-Null
Push-Location $repoRoot
try {
    & $iscc "/Qp" "/DMyAppVersion=$Version" "/DBuildDir=$BuildDir" "/DToolsDir=$toolsDir" "/DVCRuntimeDir=$VCRuntimeDir" $issPath
    if ($LASTEXITCODE -ne 0) {
        throw "Inno Setup compilation failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}

$installerPath = Join-Path $distDir "phelper-Setup-$Version.exe"
$hash = (Get-FileHash -LiteralPath $installerPath -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $([IO.Path]::GetFileName($installerPath))" |
    Set-Content -LiteralPath "$installerPath.sha256" -Encoding ascii
[pscustomobject]@{
    Installer = $installerPath
    Bytes = (Get-Item -LiteralPath $installerPath).Length
    SHA256 = $hash
    VCRuntime = $VCRuntimeDir
} | Format-List
