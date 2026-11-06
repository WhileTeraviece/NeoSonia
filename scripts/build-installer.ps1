# 构建 NeoSonia 自解压安装包（内嵌 payload.zip + 卸载程序，注册到 Windows「应用和功能」）
#
# 前置：已用 scripts\build-release.ps1 生成 target\release\neosonia.exe
# 依赖：Windows 自带 .NET Framework csc.exe（无需安装 Inno Setup / 7-Zip）
# 卸载：安装目录里的 neosonia.exe --uninstall（不再单独生成 Uninstall.exe，
#   避免第三方安全软件拦截/隔离"自删除的卸载器"）。
#
# 产物：
#   dist\NeoSoniaSetup-<version>-win64.exe   （自解压安装程序，17-18 MB）
#   dist\SHA256SUMS.txt
#
# 安装行为：解压到 %LOCALAPPDATA%\Programs\NeoSonia（可用 /D= 指定），
#   配置/数据/workspace/personas/skills/logs 全部保存在安装目录下；
#   写 HKCU Uninstall 注册表项（「应用和功能」可见）+ 开始菜单/桌面快捷方式；
#   卸载：neosonia.exe --uninstall（删目录 + 注册表 + 快捷方式，不依赖独立卸载器）。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$manifest = Get-Content (Join-Path $root "Cargo.toml") -Raw
$version = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
if (-not $version) { throw "Cargo.toml 里找不到 version" }
Write-Host "构建安装包 NeoSoniaSetup-$version-win64" -ForegroundColor Cyan

$csc = "C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
if (-not (Test-Path $csc)) { throw "找不到 csc.exe（需要 .NET Framework 4.x）" }

$relExe = Join-Path $root "target\release\neosonia.exe"
if (-not (Test-Path $relExe)) { throw "请先跑 scripts\build-release.ps1 生成 $relExe" }

New-Item -ItemType Directory -Force build | Out-Null

# 1) PNG -> ICO
Add-Type -AssemblyName System.Drawing
$pngPath = Join-Path $root "assets\icon-256.png"
$bmp = New-Object System.Drawing.Bitmap 256, 256
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.Clear([System.Drawing.Color]::Transparent)
$g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$src = [System.Drawing.Image]::FromFile((Resolve-Path $pngPath))
$g.DrawImage($src, 0, 0, 256, 256)
$g.Dispose(); $src.Dispose()
$iconPath = Join-Path $root "build\icon.ico"
$fs = [System.IO.File]::Create($iconPath)
([System.Drawing.Icon]::FromHandle($bmp.GetHicon())).Save($fs)
$fs.Close(); $bmp.Dispose()

# 2) payload 目录（卸载程序不再单独提供：neosonia.exe --uninstall）
$payload = Join-Path $root "build\payload"
if (Test-Path $payload) { Remove-Item $payload -Recurse -Force }
New-Item -ItemType Directory -Force $payload | Out-Null
Copy-Item $relExe $payload
Copy-Item (Join-Path $root "assets") $payload -Recurse
Copy-Item (Join-Path $root "README.md"),(Join-Path $root "LICENSE") $payload
New-Item -ItemType Directory -Force (Join-Path $payload "docs") | Out-Null
Copy-Item (Join-Path $root "docs\verification.md") (Join-Path $payload "docs")
$notes = Join-Path $root "docs\release-notes-v$version.md"
if (Test-Path $notes) { Copy-Item $notes (Join-Path $payload "docs") }
foreach ($d in @("config", "data", "logs", "workspace", "personas", "skills")) {
    New-Item -ItemType Directory -Force (Join-Path $payload $d) | Out-Null
    Set-Content (Join-Path $payload "$d\.gitkeep") "" -Encoding utf8
}
Set-Content (Join-Path $payload "VERSION.txt") `
    "NeoSonia $version - 配置/数据/workspace/personas/skills/logs 均保存在安装目录下" -Encoding utf8

# 4) payload.zip
$zipPath = Join-Path $root "build\payload.zip"
if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::CreateFromDirectory($payload, $zipPath,
    [System.IO.Compression.CompressionLevel]::Optimal, $false)

# 5) 安装程序（内嵌 payload.zip）
New-Item -ItemType Directory -Force (Join-Path $root "dist") | Out-Null
$setup = Join-Path $root "dist\NeoSoniaSetup-$version-win64.exe"
& $csc /nologo /target:winexe "/out:$setup" "/win32icon:$iconPath" `
    "/resource:$zipPath,payload.zip" `
    /r:System.Windows.Forms.dll /r:System.Drawing.dll /r:System.IO.Compression.dll /r:System.IO.Compression.FileSystem.dll `
    (Join-Path $root "installer\Setup.cs") | Out-Null
if ($LASTEXITCODE -ne 0) { throw "安装程序编译失败" }

# 6) SHA256
$hash = (Get-FileHash $setup -Algorithm SHA256).Hash
@(
    "SHA256（NeoSoniaSetup-$version-win64.exe）: $hash"
    "版本: $version"
    "构建时间: $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
) | Set-Content (Join-Path $root "dist\SHA256SUMS.txt") -Encoding utf8

$sizeMb = [math]::Round((Get-Item $setup).Length / 1MB, 1)
Write-Host "完成：$setup（$sizeMb MB）" -ForegroundColor Green
Write-Host "SHA256: $hash"
Write-Host "静默安装：NeoSoniaSetup-$version-win64.exe /S [/D=<目录>]；卸载：<安装目录>\neosonia.exe --uninstall [--silent]"
