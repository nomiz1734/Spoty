# Builds Spoty for the TrimUI Brick / Brick Pro and packages it for the stock OS.
#
#   .\build.ps1                    -> dist\Spoty (copy to SD:\Apps\Spoty), dist\Spoty-stock.zip
#                                     and the OTA files dist\update\{update.json, spoty-update.tar.gz}
#   .\build.ps1 -UpdateUrl <url>   -> bakes the OTA manifest URL into the app, e.g.
#                                     https://github.com/<you>/<repo>/releases/latest/download/update.json
#   .\build.ps1 -Notes "..."       -> release notes shown on the device before updating
#   .\build.ps1 -Screenshots       -> also renders every screen to dist\screenshots
#   .\build.ps1 -Publish           -> also creates the GitHub release (tools\publish_release.py)
#
# Needs: rustup (x86_64-pc-windows-gnu host) with target aarch64-unknown-linux-gnu,
#        and `pip install ziglang cargo-zigbuild`.
param(
    [switch]$Screenshots,
    [switch]$Publish,
    [string]$UpdateUrl = "",
    [string]$Notes = ""
)

# Native tools print progress on stderr; failures are detected with $LASTEXITCODE.
$ErrorActionPreference = "Continue"
$root = $PSScriptRoot
$pyScripts = python -c "import sysconfig; print(sysconfig.get_path('scripts', scheme='nt_user'))"
$env:PATH = "$env:USERPROFILE\.cargo\bin;$pyScripts;$env:PATH"

# dlltool shim (see tools\dlltool.rs).
$toolDir = Join-Path $root "target\tools"
$shim = Join-Path $toolDir "dlltool.exe"
if (-not (Test-Path $shim)) {
    New-Item -ItemType Directory -Force $toolDir | Out-Null
    rustc -O (Join-Path $root "tools\dlltool.rs") -o $shim
    if ($LASTEXITCODE -ne 0) { throw "could not build the dlltool shim" }
}
$env:PATH = "$toolDir;$env:PATH"

# The default OTA manifest URL is compiled in (settings.json can override it).
if ($UpdateUrl) {
    $env:SPOTY_UPDATE_URL = $UpdateUrl
} elseif (Test-Path (Join-Path $root "update-url.txt")) {
    $env:SPOTY_UPDATE_URL = (Get-Content (Join-Path $root "update-url.txt") -Raw).Trim()
}
if ($env:SPOTY_UPDATE_URL) { Write-Host "OTA manifest: $env:SPOTY_UPDATE_URL" }

$version = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version = "(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value

# glibc 2.17 keeps the binary compatible with old device firmwares.
$target = "aarch64-unknown-linux-gnu"
cargo zigbuild --release --target "$target.2.17"
if ($LASTEXITCODE -ne 0) { throw "ARM64 build failed" }

$distRoot = Join-Path $root "dist"
$dist = Join-Path $distRoot "Spoty"
if (Test-Path $distRoot) { Remove-Item -Recurse -Force $distRoot -ErrorAction Stop }
New-Item -ItemType Directory -Force $dist, (Join-Path $dist "assets\fonts") | Out-Null
Copy-Item (Join-Path $root "target\$target\release\spoty") $dist -ErrorAction Stop
Copy-Item (Join-Path $root "package\stock\*") $dist -Recurse -ErrorAction Stop
# Shell scripts must have LF line endings on the device, whatever git did on checkout.
$launch = Join-Path $dist "launch.sh"
[IO.File]::WriteAllText($launch, ([IO.File]::ReadAllText($launch) -replace "`r`n", "`n"))
Copy-Item (Join-Path $root "assets\fonts\OFL.txt") (Join-Path $dist "assets\fonts\")
Set-Content -Encoding utf8 (Join-Path $dist "assets\fonts\README.txt") @"
Put an extra .ttf/.otf here (for example NotoSansCJK) to display
Chinese/Japanese/Korean song titles. Noto Sans (Latin + Vietnamese) is built in.
"@
Compress-Archive -Path $dist -DestinationPath (Join-Path $distRoot "Spoty-stock.zip")
Write-Host "Package ready: $dist (version $version)"

$notesArg = $Notes
if (-not $notesArg -and (Test-Path (Join-Path $root "release-notes.txt"))) {
    $notesArg = Join-Path $root "release-notes.txt"
}
python (Join-Path $root "tools\make_update.py") $dist (Join-Path $distRoot "update") $version $notesArg
if ($LASTEXITCODE -ne 0) { throw "OTA package failed" }
Write-Host "Upload dist\update\update.json and dist\update\spoty-update.tar.gz to your release."

if ($Screenshots) {
    cargo zigbuild --release --target x86_64-pc-windows-gnu
    if ($LASTEXITCODE -ne 0) { throw "Windows build failed" }
    & (Join-Path $root "target\x86_64-pc-windows-gnu\release\spoty.exe") --screenshots (Join-Path $distRoot "screenshots")
}

if ($Publish) {
    python (Join-Path $root "tools\publish_release.py")
    if ($LASTEXITCODE -ne 0) { throw "publishing the release failed" }
}
