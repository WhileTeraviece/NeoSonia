# NeoSonia 端到端冒烟（真机）
#
# 流程：
#   1) 启动 app -> 等 dock -> 截图（基线）
#   2) 点导航「设置」-> 截图（设置页顶部）
#   3) 滚到设置页「连接其他设备（手机/平板）」-> 点「打开设备页」-> 截图（证明设备页可达）
#   4) 回设置页 -> 滚到「MCP 服务器」分组 -> 截图；点分组标题 -> 截图
#      （当前构建里 MCP 是普通分组、不可折叠，点击无视觉变化属正常）
#   5) 回顶部再滚到底 -> 点「技能」折叠标题（默认折叠）-> 截图；用前后像素差证明折叠面板工作
#   6) 关闭 app（try/finally，不留进程）
#
# 用法（Windows PowerShell 5.1）：
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\e2e-smoke.ps1
#   可选：-HomeDir、-OutDir、-NoCleanup（留着 app 给人工看）、各坐标/滚动量参数
#
# 重要前提：
# - 需要独占机器/前台：并行跑别的 UI 测试时，别人 Stop-Process neosonia 或启动第二个实例
#   会让 FindWindow 拿到别人的窗口、点击/截图全部不可信。
# - 截图前必须先 Invoke-NeoFocus：app 刚 dock 时窗口可能被别的窗口挡住。
# - 全局快捷键无法用 SendInput 验证（键盘钩子忽略 LLKHF_INJECTED 注入事件），本脚本只检查日志里的钩子行。
# - 下方“校准值”是本机 250% 缩放、sandbox 默认配置下的实测值；改主题/字体缩放/设置项数量后可能偏，
#   脚本会把每步截图都存下来，便于按图微调参数。

param(
    [string]$HomeDir = "E:\NeoSonia\sandbox",
    [string]$OutDir = "E:\NeoSonia\logs\ui\e2e",
    [string]$AppExe = "E:\NeoSonia\target\debug\neosonia.exe",
    [switch]$NoCleanup,
    # ---- 校准值（逻辑像素 / 滚轮格数，250% DPI、sandbox 默认配置实测）----
    [int]$NavY = 68,
    [int]$NavSettingsX = 201,
    [int]$DeviceScrollNotches = 48,
    [int]$DeviceButtonX = 345,
    [int]$DeviceButtonY = 221,
    [int]$McpScrollNotches = 100,
    [int]$McpHeaderX = 160,
    [int]$McpHeaderY = 210,
    [int]$BottomScrollNotches = 200,
    [int]$SkillHeaderX = 160,
    [int]$SkillHeaderY = 110
)

$ErrorActionPreference = "Stop"
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path $scriptDir "ui-drive.ps1")
$script:NeoExe = $AppExe
New-Item -ItemType Directory -Force $OutDir | Out-Null

function Compare-NeoImages {
    <# 比较两张同尺寸 PNG 的像素差，返回差异百分比（0-100）。 #>
    param([string]$PathA, [string]$PathB, [int]$SampleStep = 6)
    Add-Type -AssemblyName System.Drawing
    $a = [System.Drawing.Bitmap]::FromFile((Resolve-Path $PathA).Path)
    $b = [System.Drawing.Bitmap]::FromFile((Resolve-Path $PathB).Path)
    try {
        if ($a.Width -ne $b.Width -or $a.Height -ne $b.Height) { return 100.0 }
        $diff = 0
        $total = 0
        for ($y = 0; $y -lt $a.Height; $y += $SampleStep) {
            for ($x = 0; $x -lt $a.Width; $x += $SampleStep) {
                $ca = $a.GetPixel($x, $y)
                $cb = $b.GetPixel($x, $y)
                $delta = [math]::Abs([int]$ca.R - [int]$cb.R) + [math]::Abs([int]$ca.G - [int]$cb.G) + [math]::Abs([int]$ca.B - [int]$cb.B)
                if ($delta -gt 24) { $diff++ }
                $total++
            }
        }
        if ($total -eq 0) { return 0.0 }
        return [math]::Round(100.0 * $diff / $total, 2)
    } finally {
        $a.Dispose()
        $b.Dispose()
    }
}

function Get-PassText {
    param($Ok)
    if ($Ok) { return "通过" }
    return "未通过"
}

function Click-NeoNav {
    <# 点导航并确认 page switch（失败时左右各试一次，容忍 1-2px 坐标偏差）。 #>
    param([string]$Page, [int]$X, [int]$Y = 68)
    $candidates = @($X, ($X - 6), ($X + 6))
    foreach ($cx in $candidates) {
        $before = Get-NeoLogLineCount
        Invoke-NeoClickLogical $cx $Y | Out-Null
        if (Wait-NeoPage -Page $Page -TimeoutSec 4 -SinceLine $before) { return $cx }
    }
    return $null
}

function Scroll-NeoToTop {
    Invoke-NeoWheel -Notches -200
    Start-Sleep -Milliseconds 500
}

$results = [pscustomobject]@{
    Baseline = $null
    SettingsTop = $null
    DevicePage = $null
    DevicePageReachable = $false
    DevicePageViaFallback = $false
    McpSection = $null
    McpHeaderClicked = $null
    CollapseBefore = $null
    CollapseAfter = $null
    CollapseDiffPercent = $null
    CollapseWorked = $false
    HotkeyHook = $null
    Error = $null
}

$app = $null
try {
    Write-Host "== NeoSonia 端到端冒烟 =="
    Write-Host ("exe : " + $AppExe)
    Write-Host ("home: " + $HomeDir)
    Write-Host "提示：本脚本需要独占前台；如果别的 agent 正在起停 neosonia，结果不可信。"

    # ---------- 1. 启动 + 基线 ----------
    Write-Host "`n[1/6] 启动 app，等 dock ..."
    $app = Start-NeoApp -HomeDir $HomeDir
    Write-Host ("  冷启动：" + $app.StartupMs + " ms；窗口：" + [bool]$app.Window)
    $instances = @(Get-Process neosonia -ErrorAction SilentlyContinue)
    if ($instances.Count -ne 1) {
        throw ("检测到 " + $instances.Count + " 个 neosonia 实例：并行 UI 测试会污染截图/点击，请独占机器后重跑。")
    }
    Invoke-NeoFocus | Out-Null
    Start-Sleep -Milliseconds 1000
    $results.Baseline = (Save-NeoScreenshot (Join-Path $OutDir "01-baseline.png")).Path
    Write-Host ("  基线截图：" + $results.Baseline)
    $results.HotkeyHook = Test-NeoHotkeyHookInstalled -LogPath $app.Log
    Write-Host ("  全局快捷键钩子：" + [string]$results.HotkeyHook)

    # ---------- 2. 设置页 ----------
    Write-Host "`n[2/6] 点导航「设置」..."
    $settingsX = Click-NeoNav -Page "Settings" -X $NavSettingsX -Y $NavY
    if ($settingsX -ne $null) { Write-Host ("  page switch page=Settings 已确认（点击 x=" + $settingsX + "）") } else { Write-Host "  警告：点击后没看到 page switch page=Settings" }
    Start-Sleep -Milliseconds 500
    $results.SettingsTop = (Save-NeoScreenshot (Join-Path $OutDir "02-settings-top.png")).Path
    Write-Host ("  设置页截图：" + $results.SettingsTop)

    # ---------- 3. 设备页可达（设置页「连接其他设备」按钮） ----------
    Write-Host ("`n[3/6] 滚 " + $DeviceScrollNotches + " 格到「连接其他设备（手机/平板）」...")
    Invoke-NeoWheel -Notches $DeviceScrollNotches
    Start-Sleep -Milliseconds 600
    $beforeDevice = Get-NeoLogLineCount
    # 「打开设备页」按钮可能有 1-2px 校准偏差：按候选列表依次试
    $reached = $false
    foreach ($bx in @($DeviceButtonX, ($DeviceButtonX - 12), ($DeviceButtonX + 12))) {
        Invoke-NeoClickLogical $bx $DeviceButtonY | Out-Null
        if (Wait-NeoPage -Page "Remote" -TimeoutSec 3 -SinceLine $beforeDevice) { $reached = $true; break }
    }
    if (-not $reached) {
        Write-Host "  设置页按钮没点中，改用标题栏「连接其他设备」图标兜底 ..."
        $beforeFallback = Get-NeoLogLineCount
        # 新布局顶栏图标（逻辑 x）：分享/连接其他设备 288、时钟 327、眼睛 363、关闭 401
        Invoke-NeoClickLogical 288 24 | Out-Null
        $reached = Wait-NeoPage -Page "Remote" -TimeoutSec 5 -SinceLine $beforeFallback
        $results.DevicePageViaFallback = $reached
    }
    $results.DevicePageReachable = $reached
    Start-Sleep -Milliseconds 600
    $results.DevicePage = (Save-NeoScreenshot (Join-Path $OutDir "03-device-page.png")).Path
    Write-Host ("  设备页可达（page switch page=Remote）：" + $reached + "；截图：" + $results.DevicePage)

    # ---------- 4. MCP 分组 ----------
    Write-Host "`n[4/6] 回设置页，滚到「MCP 服务器」分组 ..."
    Click-NeoNav -Page "Settings" -X $NavSettingsX -Y $NavY | Out-Null
    Scroll-NeoToTop
    Invoke-NeoWheel -Notches $McpScrollNotches
    Start-Sleep -Milliseconds 700
    $results.McpSection = (Save-NeoScreenshot (Join-Path $OutDir "04-settings-mcp.png")).Path
    Write-Host ("  MCP 分组截图：" + $results.McpSection)
    Write-Host "  点「MCP 服务器」分组标题（当前构建不可折叠，无变化属正常）..."
    Invoke-NeoClickLogical $McpHeaderX $McpHeaderY | Out-Null
    Start-Sleep -Milliseconds 800
    $results.McpHeaderClicked = (Save-NeoScreenshot (Join-Path $OutDir "05-settings-mcp-clicked.png")).Path
    Write-Host ("  点击后截图：" + $results.McpHeaderClicked)

    # ---------- 5. 折叠面板证明（技能，默认折叠） ----------
    Write-Host "`n[5/6] 回顶部再滚到底，点「技能」折叠标题 ..."
    Scroll-NeoToTop
    Invoke-NeoWheel -Notches $BottomScrollNotches
    Start-Sleep -Milliseconds 800
    $results.CollapseBefore = (Save-NeoScreenshot (Join-Path $OutDir "06-collapse-before.png")).Path
    Invoke-NeoClickLogical $SkillHeaderX $SkillHeaderY | Out-Null
    Start-Sleep -Milliseconds 1200
    $results.CollapseAfter = (Save-NeoScreenshot (Join-Path $OutDir "07-collapse-after.png")).Path
    $diff = Compare-NeoImages -PathA $results.CollapseBefore -PathB $results.CollapseAfter
    $results.CollapseDiffPercent = $diff
    $results.CollapseWorked = ($diff -ge 0.5)
    Write-Host ("  折叠前：" + $results.CollapseBefore)
    Write-Host ("  折叠后：" + $results.CollapseAfter)
    Write-Host ("  前后像素差：" + $diff + "%")
} catch {
    $results.Error = $_.Exception.Message
    Write-Host ("错误：" + $results.Error)
} finally {
    if (-not $NoCleanup) {
        Stop-NeoApp -App $app
        Write-Host "`n[6/6] 已关闭 app（含 MCP node 子进程清理）"
    } else {
        Write-Host "`n[6/6] -NoCleanup：保留 app 进程"
    }
}

# ---------- 汇总 ----------
Write-Host "`n=== e2e 冒烟结果 ==="
Write-Host ("基线截图        : " + $results.Baseline)
Write-Host ("设置页          : " + $results.SettingsTop)
Write-Host ("设备页截图      : " + $results.DevicePage)
Write-Host ("设备页可达      : " + $results.DevicePageReachable + "（兜底入口：" + $results.DevicePageViaFallback + "）")
Write-Host ("MCP 分组        : " + $results.McpSection)
Write-Host ("MCP 标题点击后  : " + $results.McpHeaderClicked)
Write-Host ("折叠前          : " + $results.CollapseBefore)
Write-Host ("折叠后          : " + $results.CollapseAfter)
Write-Host ("折叠像素差      : " + $results.CollapseDiffPercent + "%（判定：" + (Get-PassText $results.CollapseWorked) + "）")
Write-Host ("全局快捷键钩子  : " + [string]$results.HotkeyHook + "（SendInput 不会触发自家钩子，热键需人工验证）")
if ($results.Error) { Write-Host ("错误            : " + $results.Error) }

$passed = ($results.CollapseWorked -and $results.DevicePageReachable -and -not $results.Error)
Write-Host ("总体判定        : " + (Get-PassText $passed))
if (-not $passed) { exit 1 }
