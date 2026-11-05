<#
.SYNOPSIS
  构建 NeoSonia Windows release 包（exe + 资源 + 文档 + zip + sha256）。

.DESCRIPTION
  - 从根 Cargo.toml 读取版本号（[package] version）。
  - `cargo build --release --offline`（Release profile：opt-level 3 / thin LTO / strip symbols）。
  - 产物目录：dist\NeoSonia-<version>-win64\
      neosonia.exe
      assets\            （logo / 托盘图标等运行时资源）
      README.md
      docs\verification.md、docs\release-notes-v<version>.md
      SHA256SUMS.txt
  - 打 zip：dist\NeoSonia-<version>-win64.zip
  - 不打包任何用户数据：config\、logs\、sandbox\、data\ 一律排除（避免泄露 API key / 邮箱 token）。

.EXAMPLE
  pwsh scripts\build-release.ps1
  pwsh scripts\build-release.ps1 -SkipTests -KeepDir
#>
[CmdletBinding()]
param(
    [switch]$SkipTests,
    [switch]$KeepDir,
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$env:CARGO_TARGET_DIR = Join-Path $root "target"

# ---------- 版本 ----------
if (-not $Version) {
    $manifest = Get-Content (Join-Path $root "Cargo.toml") -Raw
    $m = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $m.Success) { throw "Cargo.toml 里找不到 [package] version" }
    $Version = $m.Groups[1].Value
}
Write-Host "NeoSonia release v$Version" -ForegroundColor Cyan

# ---------- 前置检查 ----------
$running = Get-Process neosonia -ErrorAction SilentlyContinue
if ($running) {
    Write-Host "检测到正在运行的 NeoSonia（PID: $($running.Id -join ',')），先退出以便覆盖 exe" -ForegroundColor Yellow
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 800
}

if (-not $SkipTests) {
    Write-Host "==> cargo test（debug，快速回归）" -ForegroundColor Cyan
    cmd /c "cargo test --offline --workspace > logs\release-tests.log 2>&1"
    if ($LASTEXITCODE -ne 0) {
        Write-Host "测试失败，详见 logs\release-tests.log" -ForegroundColor Red
        exit 1
    }
    Write-Host "测试通过" -ForegroundColor Green
}

# ---------- 构建 ----------
Write-Host "==> cargo build --release（可能要几分钟）" -ForegroundColor Cyan
$sw = [System.Diagnostics.Stopwatch]::StartNew()
cmd /c "cargo build --offline --release -p neosonia-app > logs\release-build.log 2>&1"
if ($LASTEXITCODE -ne 0) {
    Write-Host "release 构建失败，详见 logs\release-build.log" -ForegroundColor Red
    exit 1
}
$sw.Stop()
$exe = Join-Path $root "target\release\neosonia.exe"
if (-not (Test-Path $exe)) { throw "找不到 $exe" }
$sizeMb = [math]::Round((Get-Item $exe).Length / 1MB, 1)
Write-Host ("release 构建完成：{0} MB，用时 {1:N1}s" -f $sizeMb, $sw.Elapsed.TotalSeconds) -ForegroundColor Green

# ---------- 打包 ----------
$name = "NeoSonia-$Version-win64"
$dist = Join-Path $root "dist"
$out = Join-Path $dist $name
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory -Path $out -Force | Out-Null

Copy-Item $exe (Join-Path $out "neosonia.exe")
if (Test-Path (Join-Path $root "assets")) {
    Copy-Item (Join-Path $root "assets") (Join-Path $out "assets") -Recurse
}
Copy-Item (Join-Path $root "README.md") $out
$docsOut = Join-Path $out "docs"
New-Item -ItemType Directory -Path $docsOut -Force | Out-Null
Copy-Item (Join-Path $root "docs\verification.md") $docsOut
$notes = Join-Path $root "docs\release-notes-v$Version.md"
if (Test-Path $notes) { Copy-Item $notes $docsOut }

# SHA256
$hash = (Get-FileHash (Join-Path $out "neosonia.exe") -Algorithm SHA256).Hash
@(
    "SHA256（neosonia.exe）: $hash"
    "版本: $Version"
    "构建时间: $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
) | Set-Content (Join-Path $out "SHA256SUMS.txt") -Encoding utf8

# zip
$zip = Join-Path $dist "$name.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Write-Host "==> 打包 $zip" -ForegroundColor Cyan
Compress-Archive -Path (Join-Path $out "*") -DestinationPath $zip -CompressionLevel Optimal
$zipMb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Host "zip 完成：$zip（$zipMb MB）" -ForegroundColor Green

if (-not $KeepDir) {
    Write-Host "保留 dist\$name 目录以便检查；如需清理用 -KeepDir 之外的方式手动删" -ForegroundColor DarkGray
}

Write-Host ""
Write-Host "完成：" -ForegroundColor Cyan
Write-Host "  exe : $exe"
Write-Host "  dir : $out"
Write-Host "  zip : $zip"
Write-Host "  sha : $hash"
Write-Host ""
Write-Host "提醒：release 包不含 config/（API key、邮箱 token），首次运行会在 --home 目录生成默认配置。" -ForegroundColor Yellow
