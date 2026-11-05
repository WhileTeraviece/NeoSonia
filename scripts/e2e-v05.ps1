# NeoSonia v0.5 计算机使用验收一键运行
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File scripts\e2e-v05.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\e2e-v05.ps1 -Build
#   powershell -ExecutionPolicy Bypass -File scripts\e2e-v05.ps1 -Only "01,02"
#
# 用例编号（NEOSONIA_E2E_ONLY 子集）：
#   01 窗口显隐（Alt+Space → rect.x≈-12000 → 再按回 2750）
#   02 按钮点击（顶栏「连接其他设备」→ 设备页，日志 page switch page=Remote）
#   03 按住拖拽（mouse_press → 10 步 mouse_move(+120,+60) → mouse_release）
#   04 切页流畅度（5 个导航，读日志 page switch ms，目标 p95<50ms，断言 max<200ms）
#   05 人设新建（新建人设 → 填 4 个字段 → 保存 → personas/e2e-auto.md 落盘 → 删除清理）
#   06 附件条 + 加载态（启动 NEOSONIA_ATTACH=<png> 预置附件卡；发消息抓忙碌态）
#
# 前置：app 已构建（v0.5）、桌面上没有别的实例/安全提示。
# 产出：logs/e2e-v05.md；截图 logs/ui/e2e-v05/。
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
$env:NEOSONIA_E2E_V05 = "1"
$env:NEOSONIA_E2E_ROOT = $Root
$env:NEOSONIA_E2E_HOME = $HomeDir
$env:NEOSONIA_E2E_EXE = $Exe
if ($Only) {
    $env:NEOSONIA_E2E_ONLY = $Only
} else {
    Remove-Item Env:NEOSONIA_E2E_ONLY -ErrorAction SilentlyContinue
}

Write-Host "== NeoSonia v0.5 计算机使用验收 ==" -ForegroundColor Cyan
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
    $shotDir = Join-Path $logDir "ui\e2e-v05"
    New-Item -ItemType Directory -Force -Path $shotDir | Out-Null
    $md = Join-Path $logDir "e2e-v05.md"
    if (Test-Path $md) { Remove-Item $md -Force }

    Write-Host "`n[cargo test --test e2e_computeruse e2e_v05 -- --ignored]" -ForegroundColor Yellow
    cargo test --offline -p neosonia-tools --test e2e_computeruse e2e_v05 -- --ignored --nocapture --test-threads=1
    $code = $LASTEXITCODE

    Write-Host ""
    if (Test-Path $md) {
        Write-Host "报告：$md" -ForegroundColor Green
        Write-Host "截图：$shotDir" -ForegroundColor Green
        Write-Host "----- 报告摘要 -----"
        Get-Content $md -TotalCount 16
    } else {
        Write-Host "没有生成报告（测试可能在编译阶段就失败了，检查上面的 cargo 输出）" -ForegroundColor Yellow
    }
    exit $code
}
finally {
    Pop-Location
}
