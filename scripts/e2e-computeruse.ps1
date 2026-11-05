# NeoSonia 计算机使用 E2E 一键运行脚本
#
# 用法（在仓库根或任意目录）：
#   powershell -ExecutionPolicy Bypass -File scripts\e2e-computeruse.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\e2e-computeruse.ps1 -Build          # 先 cargo build app
#   powershell -ExecutionPolicy Bypass -File scripts\e2e-computeruse.ps1 -Only 04         # 只跑用例 4
#   powershell -ExecutionPolicy Bypass -File scripts\e2e-computeruse.ps1 -Only 02,03      # 只跑 2、3
#
# 前置条件：
#   - 已构建 E:\NeoSonia\target\debug\neosonia.exe（v0.4 集成完成后）
#   - 桌面上没有系统安全提示（UAC/防火墙弹窗会吞掉合成输入）
#   - 当前桌面可交互（未锁屏）
#
# 产出：
#   - 控制台：逐用例 PASS/FAIL/SKIP + 最终「N 通过 / M 失败」
#   - 报告：E:\NeoSonia\logs\e2e-computeruse.md
#   - 截图：E:\NeoSonia\logs\e2e-screenshots\
#   - 失败现场：E:\NeoSonia\logs\e2e-failures\（*-tree.txt / *-fail.png）
param(
    [string]$Root = "E:\NeoSonia",
    [string]$HomeDir = "",
    [string]$Exe = "",
    [string]$Only = "",
    [switch]$Build
)

$ErrorActionPreference = "Continue"

if (-not $HomeDir) { $HomeDir = Join-Path $Root "sandbox" }
if (-not $Exe) { $Exe = Join-Path $Root "target\debug\neosonia.exe" }

$env:CARGO_TARGET_DIR = Join-Path $Root "target"
$env:NEOSONIA_E2E = "1"
$env:NEOSONIA_E2E_ROOT = $Root
$env:NEOSONIA_E2E_HOME = $HomeDir
$env:NEOSONIA_E2E_EXE = $Exe
if ($Only) {
    $env:NEOSONIA_E2E_ONLY = $Only
} else {
    Remove-Item Env:NEOSONIA_E2E_ONLY -ErrorAction SilentlyContinue
}

Write-Host "== NeoSonia 计算机使用 E2E ==" -ForegroundColor Cyan
Write-Host "root: $Root"
Write-Host "home: $HomeDir"
Write-Host "exe : $Exe"
if ($Only) { Write-Host "only: $Only" } else { Write-Host "only: (全部用例)" }

Push-Location $Root
try {
    if ($Build) {
        Write-Host "`n[cargo build -p neosonia-app]" -ForegroundColor Yellow
        cargo build --offline -p neosonia-app
        if ($LASTEXITCODE -ne 0) {
            Write-Host "构建失败，退出码 $LASTEXITCODE" -ForegroundColor Red
            exit $LASTEXITCODE
        }
    }

    if (-not (Test-Path $Exe)) {
        Write-Host "找不到 app 可执行文件：$Exe" -ForegroundColor Red
        Write-Host "请先运行：cargo build --offline -p neosonia-app" -ForegroundColor Yellow
        exit 1
    }

    $logDir = Join-Path $Root "logs"
    New-Item -ItemType Directory -Force -Path (Join-Path $logDir "e2e-screenshots") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $logDir "e2e-failures") | Out-Null
    $md = Join-Path $logDir "e2e-computeruse.md"
    if (Test-Path $md) { Remove-Item $md -Force }

    Write-Host "`n[cargo test --test e2e_computeruse -- --ignored]" -ForegroundColor Yellow
    cargo test --offline -p neosonia-tools --test e2e_computeruse -- --ignored --nocapture --test-threads=1
    $code = $LASTEXITCODE

    Write-Host ""
    if (Test-Path $md) {
        Write-Host "报告：$md" -ForegroundColor Green
        Write-Host "截图：$(Join-Path $logDir 'e2e-screenshots')" -ForegroundColor Green
        Write-Host "失败现场：$(Join-Path $logDir 'e2e-failures')" -ForegroundColor Green
        Write-Host "----- 报告摘要 -----"
        Get-Content $md -TotalCount 14
    } else {
        Write-Host "没有生成报告（测试可能在编译阶段就失败了，检查上面的 cargo 输出）" -ForegroundColor Yellow
    }
    exit $code
}
finally {
    Pop-Location
}
