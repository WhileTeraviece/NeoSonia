# NeoSonia 性能基准（真机）
#
# 做三件事：
#   1) 冷启动：进程启动 -> 日志出现 dock attempt 的耗时；
#   2) 采集日志里的 view built（每帧构建耗时）与 frame rate（帧率）；
#   3) 用 ui-drive.ps1 依次点击 对话/邮件/记忆/人设/设置 各 N 次，采集 page switch 延迟。
# 输出：
#   logs/perf-report.json  （机器可读）
#   logs/perf-report.md    （人读的 markdown 表格）
#
# 用法（Windows PowerShell 5.1）：
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\perf-bench.ps1
#   可选参数：-Home、-OutDir、-NavClicks、-WarmupSeconds、-MeasureSeconds、-AppExe
#
# 注意：
# - 需要带性能日志的 app 构建。日志格式（NEOSONIA_PROFILE=1 时）：
#     view built page=Chat ms=3.2 items=0 attachments=0
#     page switch page=Settings ms=1.4
#     frame rate fps=59.9
#   如果日志里一条 view built 都没有，说明 exe 是旧构建或 NEOSONIA_PROFILE 没生效，
#   脚本会给出明确提示（不会瞎编数据）。
# - 脚本会先杀掉同名 app 进程、清空日志，保证“冷启动”可重复；结束时 try/finally 关掉 app。
# - 本机是 250% 缩放（DPI 240），所有点击都用逻辑坐标，由 ui-drive.ps1 自动换成物理像素。

param(
    [string]$HomeDir = "E:\NeoSonia\sandbox",
    [string]$OutDir = "E:\NeoSonia\logs",
    [int]$NavClicks = 3,
    [int]$WarmupSeconds = 8,
    [int]$MeasureSeconds = 14,
    [string]$AppExe = "E:\NeoSonia\target\debug\neosonia.exe",
    [int]$NavY = 68,
    [int[]]$NavXs = @(19, 62, 106, 157, 201),
    [string[]]$NavPages = @("Chat", "Mail", "Memory", "Persona", "Settings")
)

$ErrorActionPreference = "Stop"
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path $scriptDir "ui-drive.ps1")
# 允许覆盖 exe（ui-drive 内部的 $script:NeoExe 是全局默认值）
$script:NeoExe = $AppExe
New-Item -ItemType Directory -Force $OutDir | Out-Null

function Get-Percentile {
    param([double[]]$Values, [double]$P)
    if (-not $Values -or $Values.Count -eq 0) { return $null }
    $sorted = @($Values | Sort-Object)
    $idx = [int][math]::Ceiling($P / 100.0 * $sorted.Count) - 1
    if ($idx -lt 0) { $idx = 0 }
    if ($idx -ge $sorted.Count) { $idx = $sorted.Count - 1 }
    return [math]::Round([double]$sorted[$idx], 1)
}

function Get-LogTimestamp {
    param([string]$Line)
    if (-not $Line) { return $null }
    $m = [regex]::Match($Line, "^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z)")
    if (-not $m.Success) { return $null }
    try {
        return [DateTimeOffset]::Parse($m.Groups[1].Value, [System.Globalization.CultureInfo]::InvariantCulture)
    } catch {
        return $null
    }
}

function Format-Ms {
    param($Value)
    if ($null -eq $Value) { return "-" }
    return ([string]$Value + " ms")
}

function Get-VerdictText {
    param($Ok)
    if ($Ok -eq $true) { return "达标" }
    return "超标"
}

Write-Host "== NeoSonia 性能基准 =="
Write-Host ("exe : " + $AppExe)
if (-not (Test-Path $AppExe)) { throw ("找不到 app 可执行文件：" + $AppExe) }
$exeInfo = Get-Item $AppExe
Write-Host ("构建: " + $exeInfo.LastWriteTime)
Write-Host ("home: " + $HomeDir)

$app = $null
$errorText = $null
try {
    # ---------- 1. 冷启动 ----------
    Write-Host "`n[1/4] 冷启动测量：启动 app，等 dock attempt ..."
    $app = Start-NeoApp -HomeDir $HomeDir -Profile
    $coldStartMs = $app.StartupMs
    $logLines = @(Get-Content $app.Log -ErrorAction SilentlyContinue)
    $firstLine = $logLines | Select-Object -First 1
    $dockLine = $logLines | Where-Object { $_ -match "dock attempt" } | Select-Object -First 1
    $coldStartLogMs = $null
    $t0 = Get-LogTimestamp $firstLine
    $t1 = Get-LogTimestamp $dockLine
    if ($t0 -and $t1) { $coldStartLogMs = [int](($t1 - $t0).TotalMilliseconds) }
    Write-Host ("  进程启动 -> dock：" + $coldStartMs + " ms（日志时间戳差 " + $coldStartLogMs + " ms）")
    if (-not $app.Window) { throw "窗口没创建出来（app 可能启动失败，见日志：" + $app.Log + "）" }

    # 数据可信性自检：多实例会互相抢窗口/共用日志，结果不可信；Profile 没生效则拿不到 view built
    $instances = @(Get-Process neosonia -ErrorAction SilentlyContinue)
    if ($instances.Count -ne 1) {
        throw ("检测到 " + $instances.Count + " 个 neosonia 实例：并行 UI 测试会污染数据，请独占机器后重跑。")
    }
    Write-Host ("  NEOSONIA_PROFILE 生效检查：等待 view built 日志 ...")
    $profileOk = $false
    $deadlineProfile = (Get-Date).AddSeconds(8)
    while ((Get-Date) -lt $deadlineProfile) {
        Start-Sleep -Milliseconds 250
        if (Select-String -Path $app.Log -Pattern "view built" -Quiet -ErrorAction SilentlyContinue) { $profileOk = $true; break }
    }
    if (-not $profileOk) {
        throw "日志里没有 view built：NEOSONIA_PROFILE=1 没生效或有别的实例共用日志，性能数据不可信。"
    }

    # 窗口可能被别的窗口挡住，截图/点击前必须强制前台
    Invoke-NeoFocus | Out-Null
    Start-Sleep -Milliseconds 600

    # ---------- 2. 预热 + 采集基线 ----------
    Write-Host ("`n[2/4] 预热 " + $WarmupSeconds + " 秒（等首帧/索引扫描等一次性开销过去）...")
    Start-Sleep -Seconds $WarmupSeconds
    $linesBefore = @(Get-Content $app.Log -ErrorAction SilentlyContinue).Count

    # ---------- 3. 导航点击 ----------
    Write-Host ("`n[3/4] 导航点击：" + ($NavPages -join " / ") + " 各 " + $NavClicks + " 次")
    $clicked = 0
    for ($round = 1; $round -le $NavClicks; $round++) {
        for ($i = 0; $i -lt $NavPages.Count; $i++) {
            $page = $NavPages[$i]
            $x = $NavXs[$i]
            # 只看“这次点击之后新增的日志行”，避免把上一轮的 page switch 当成这一轮的
            $before = 0
            if (Test-Path $app.Log) { $before = @(Get-Content $app.Log -ErrorAction SilentlyContinue).Count }
            $result = Invoke-NeoClickLogical $x $NavY
            $clicked++
            # 等这一跳的 page switch 落日志（最多 2 秒），避免连点把日志挤在一起
            $found = $false
            $deadline = (Get-Date).AddMilliseconds(2000)
            while ((Get-Date) -lt $deadline) {
                Start-Sleep -Milliseconds 60
                $all = @(Get-Content $app.Log -ErrorAction SilentlyContinue)
                if ($all.Count -gt $before) {
                    $newLines = $all[$before..($all.Count - 1)]
                    $hit = $newLines | Select-String -Pattern ("page switch page=" + [regex]::Escape($page) + "\b") -Quiet -ErrorAction SilentlyContinue
                    if ($hit) { $found = $true; break }
                }
            }
            if (-not $found) { Write-Host ("  警告：" + $page + " 点击后 2 秒内没看到 page switch 日志（" + $result + "）") }
        }
    }
    Write-Host ("  完成 " + $clicked + " 次点击")

    # ---------- 4. 帧率窗口 ----------
    Write-Host ("`n[4/4] 采集 " + $MeasureSeconds + " 秒帧数据 ...")
    Start-Sleep -Seconds $MeasureSeconds
} catch {
    $errorText = $_.Exception.Message
    Write-Host ("错误：" + $errorText)
} finally {
    if ($app) { Stop-NeoApp -App $app }
}

# ---------- 解析日志 ----------
Write-Host "`n== 解析日志 =="
$logPath = Join-Path $HomeDir "logs\neosonia.log"
$lines = @()
if (Test-Path $logPath) { $lines = @(Get-Content $logPath -ErrorAction SilentlyContinue) }
Write-Host ("日志行数：" + $lines.Count + " -> " + $logPath)

$viewBuiltMs = [System.Collections.Generic.List[double]]::new()
$viewBuiltSteadyMs = [System.Collections.Generic.List[double]]::new()
$pageSwitch = [System.Collections.Generic.List[object]]::new()
$fpsSamples = [System.Collections.Generic.List[double]]::new()
$pages = @{}
for ($idx = 0; $idx -lt $lines.Count; $idx++) {
    $line = $lines[$idx]
    $m = [regex]::Match($line, "view built .*?ms=([0-9.]+)")
    if ($m.Success) {
        $value = [double]$m.Groups[1].Value
        $viewBuiltMs.Add($value)
        # $linesBefore 是预热结束时的行数：之后的行算“稳态”，不含启动首帧
        if ($linesBefore -ne $null -and $idx -ge $linesBefore) { $viewBuiltSteadyMs.Add($value) }
        continue
    }
    $m = [regex]::Match($line, "page switch page=(\w+) ms=([0-9.]+)")
    if ($m.Success) {
        $pageName = $m.Groups[1].Value
        $ms = [double]$m.Groups[2].Value
        $pageSwitch.Add([pscustomobject]@{ Page = $pageName; Ms = $ms })
        if (-not $pages.ContainsKey($pageName)) { $pages[$pageName] = [System.Collections.Generic.List[double]]::new() }
        $pages[$pageName].Add($ms)
        continue
    }
    $m = [regex]::Match($line, "frame rate .*?fps=([0-9.]+)")
    if ($m.Success) { $fpsSamples.Add([double]$m.Groups[1].Value); continue }
}

$viewStats = $null
if ($viewBuiltMs.Count -gt 0) {
    $viewStats = [pscustomobject]@{
        Count = $viewBuiltMs.Count
        P50   = Get-Percentile @($viewBuiltMs.ToArray()) 50
        P95   = Get-Percentile @($viewBuiltMs.ToArray()) 95
        Max   = [math]::Round(($viewBuiltMs | Measure-Object -Maximum).Maximum, 1)
        Avg   = [math]::Round(($viewBuiltMs | Measure-Object -Average).Average, 1)
    }
}
$viewSteadyStats = $null
if ($viewBuiltSteadyMs.Count -gt 0) {
    $steadyArr = @($viewBuiltSteadyMs.ToArray())
    $viewSteadyStats = [pscustomobject]@{
        Count = $viewBuiltSteadyMs.Count
        P50   = Get-Percentile $steadyArr 50
        P95   = Get-Percentile $steadyArr 95
        Max   = [math]::Round(($viewBuiltSteadyMs | Measure-Object -Maximum).Maximum, 1)
        Avg   = [math]::Round(($viewBuiltSteadyMs | Measure-Object -Average).Average, 1)
    }
}
$switchStats = $null
if ($pageSwitch.Count -gt 0) {
    $allSwitch = @($pageSwitch | ForEach-Object { $_.Ms })
    $switchStats = [pscustomobject]@{
        Count = $pageSwitch.Count
        P50   = Get-Percentile $allSwitch 50
        P95   = Get-Percentile $allSwitch 95
        Max   = [math]::Round(($allSwitch | Measure-Object -Maximum).Maximum, 1)
        PerPage = @{}
    }
    foreach ($key in ($pages.Keys | Sort-Object)) {
        $vals = $pages[$key].ToArray()
        $switchStats.PerPage[$key] = [pscustomobject]@{
            Count = $vals.Count
            P50   = Get-Percentile $vals 50
            P95   = Get-Percentile $vals 95
            Max   = [math]::Round(($vals | Measure-Object -Maximum).Maximum, 1)
        }
    }
}
$fpsStats = $null
if ($fpsSamples.Count -gt 0) {
    $fpsStats = [pscustomobject]@{
        Samples = $fpsSamples.Count
        Min     = [math]::Round(($fpsSamples | Measure-Object -Minimum).Minimum, 1)
        Avg     = [math]::Round(($fpsSamples | Measure-Object -Average).Average, 1)
        P50     = Get-Percentile @($fpsSamples.ToArray()) 50
        Max     = [math]::Round(($fpsSamples | Measure-Object -Maximum).Maximum, 1)
        Values  = @($fpsSamples.ToArray())
    }
}

# 从 view built 的密度估算实测帧率（profile=1 时每帧一行，比 5 秒一档更细）
$measuredFps = $null
if ($viewStats -and $viewStats.Count -gt 30 -and $coldStartLogMs -ne $null) {
    $runSeconds = $null
    $tFirst = Get-LogTimestamp ($lines | Select-Object -First 1)
    $tLast = Get-LogTimestamp ($lines | Select-Object -Last 1)
    if ($tFirst -and $tLast) {
        $runSeconds = ($tLast - $tFirst).TotalSeconds
        if ($runSeconds -gt 1) { $measuredFps = [math]::Round($viewStats.Count / $runSeconds, 1) }
    }
}

# ---------- 判定 ----------
$thresholds = [pscustomobject]@{
    ColdStartMs   = 4000
    ViewP50Ms     = 8.0
    ViewP95Ms     = 16.7
    PageSwitchP95 = 120.0
    MinFps        = 55.0
}
$verdict = [pscustomobject]@{}
$viewForVerdict = $viewStats
if ($viewSteadyStats) { $viewForVerdict = $viewSteadyStats }
if ($coldStartLogMs -ne $null) { $verdict | Add-Member -NotePropertyName "cold_start" -NotePropertyValue ($coldStartLogMs -le $thresholds.ColdStartMs) }
if ($viewForVerdict) { $verdict | Add-Member -NotePropertyName "view_p50" -NotePropertyValue ($viewForVerdict.P50 -le $thresholds.ViewP50Ms); $verdict | Add-Member -NotePropertyName "view_p95" -NotePropertyValue ($viewForVerdict.P95 -le $thresholds.ViewP95Ms) }
if ($switchStats) { $verdict | Add-Member -NotePropertyName "page_switch_p95" -NotePropertyValue ($switchStats.P95 -le $thresholds.PageSwitchP95) }
$fpsForVerdict = $null
if ($fpsStats) { $fpsForVerdict = $fpsStats.Avg } elseif ($measuredFps) { $fpsForVerdict = $measuredFps }
if ($fpsForVerdict) { $verdict | Add-Member -NotePropertyName "fps" -NotePropertyValue ($fpsForVerdict -ge $thresholds.MinFps) }

# ---------- 输出 JSON ----------
$report = [pscustomobject]@{
    GeneratedAt      = (Get-Date).ToString("yyyy-MM-dd HH:mm:ss")
    AppExe           = $AppExe
    AppBuildTime     = $exeInfo.LastWriteTime.ToString("yyyy-MM-dd HH:mm:ss")
    Home             = $HomeDir
    Error            = $errorText
    ColdStart        = [pscustomobject]@{
        ProcessToDockMs = $coldStartMs
        LogStartToDockMs = $coldStartLogMs
    }
    ProfileEnabled   = ($fpsStats -ne $null -or $viewStats -ne $null)
    ViewBuilt        = $viewStats
    ViewBuiltSteady  = $viewSteadyStats
    PageSwitch       = $switchStats
    FrameRate        = $fpsStats
    MeasuredFps      = $measuredFps
    Thresholds       = $thresholds
    Verdict          = $verdict
    LogPath          = $logPath
}
$jsonPath = Join-Path $OutDir "perf-report.json"
$report | ConvertTo-Json -Depth 8 | Set-Content -Path $jsonPath -Encoding UTF8

# ---------- 输出 markdown ----------
$md = New-Object System.Text.StringBuilder
[void]$md.AppendLine("# NeoSonia 性能报告")
[void]$md.AppendLine("")
[void]$md.AppendLine("- 生成时间：" + $report.GeneratedAt)
[void]$md.AppendLine("- 可执行文件：" + $report.AppExe + "（构建于 " + $report.AppBuildTime + "）")
[void]$md.AppendLine("- home：" + $report.Home)
[void]$md.AppendLine("- 日志：" + $report.LogPath)
if ($errorText) { [void]$md.AppendLine("- 错误：" + $errorText) }
[void]$md.AppendLine("")
[void]$md.AppendLine("## 冷启动（进程启动 -> dock attempt）")
[void]$md.AppendLine("")
[void]$md.AppendLine("| 口径 | 耗时 | 阈值 | 结论 |")
[void]$md.AppendLine("| --- | --- | --- | --- |")
$coldVerdict = if ($verdict.cold_start -eq $true) { "达标" } else { "超标" }
[void]$md.AppendLine("| 进程启动 -> 日志 dock | " + $coldStartMs + " ms | <= " + $thresholds.ColdStartMs + " ms | " + $coldVerdict + " |")
if ($coldStartLogMs -ne $null) {
    [void]$md.AppendLine("| 日志首行 -> dock（纯 app 内部） | " + $coldStartLogMs + " ms | - | - |")
}
[void]$md.AppendLine("")
[void]$md.AppendLine("## 帧构建耗时 view built")
[void]$md.AppendLine("")
if ($viewStats) {
    [void]$md.AppendLine("| 范围 | 样本 | p50 | p95 | max | avg |")
    [void]$md.AppendLine("| --- | --- | --- | --- | --- | --- |")
    [void]$md.AppendLine("| 全部（含启动首帧） | " + $viewStats.Count + " | " + $viewStats.P50 + " ms | " + $viewStats.P95 + " ms | " + $viewStats.Max + " ms | " + $viewStats.Avg + " ms |")
    if ($viewSteadyStats) {
        [void]$md.AppendLine("| 稳态（预热后） | " + $viewSteadyStats.Count + " | " + $viewSteadyStats.P50 + " ms | " + $viewSteadyStats.P95 + " ms | " + $viewSteadyStats.Max + " ms | " + $viewSteadyStats.Avg + " ms |")
    }
} else {
    [void]$md.AppendLine("没有 view built 日志（exe 可能是旧构建，或 NEOSONIA_PROFILE=1 没生效）。")
}
[void]$md.AppendLine("")
[void]$md.AppendLine("## 页面切换 page switch")
[void]$md.AppendLine("")
if ($switchStats) {
    [void]$md.AppendLine("| 页面 | 次数 | p50 | p95 | max |")
    [void]$md.AppendLine("| --- | --- | --- | --- | --- |")
    foreach ($key in ($switchStats.PerPage.Keys | Sort-Object)) {
        $s = $switchStats.PerPage[$key]
        [void]$md.AppendLine("| " + $key + " | " + $s.Count + " | " + $s.P50 + " ms | " + $s.P95 + " ms | " + $s.Max + " ms |")
    }
    [void]$md.AppendLine("| **全部** | " + $switchStats.Count + " | " + $switchStats.P50 + " ms | " + $switchStats.P95 + " ms | " + $switchStats.Max + " ms |")
} else {
    [void]$md.AppendLine("没有 page switch 日志（点击没生效，或 exe 是旧构建）。")
}
[void]$md.AppendLine("")
[void]$md.AppendLine("## 帧率")
[void]$md.AppendLine("")
if ($fpsStats) {
    [void]$md.AppendLine("| 样本 | min | avg | p50 | max |")
    [void]$md.AppendLine("| --- | --- | --- | --- | --- |")
    [void]$md.AppendLine("| " + $fpsStats.Samples + " | " + $fpsStats.Min + " | " + $fpsStats.Avg + " | " + $fpsStats.P50 + " | " + $fpsStats.Max + " |")
} else {
    [void]$md.AppendLine("没有 frame rate 日志（需要 NEOSONIA_PROFILE=1 与带性能日志的构建）。")
}
if ($measuredFps) { [void]$md.AppendLine(""); [void]$md.AppendLine("- 由 view built 行数估算的平均帧率：" + $measuredFps + " fps") }
[void]$md.AppendLine("")
[void]$md.AppendLine("## 判定（阈值可改脚本顶部）")
[void]$md.AppendLine("")
[void]$md.AppendLine("| 项 | 阈值 | 结论 |")
[void]$md.AppendLine("| --- | --- | --- |")
[void]$md.AppendLine("| 冷启动 | <= " + $thresholds.ColdStartMs + " ms | " + (Get-VerdictText $verdict.cold_start) + " |")
if ($viewForVerdict) {
    [void]$md.AppendLine("| view built p50（稳态） | <= " + $thresholds.ViewP50Ms + " ms | " + (Get-VerdictText $verdict.view_p50) + " |")
    [void]$md.AppendLine("| view built p95（稳态） | <= " + $thresholds.ViewP95Ms + " ms | " + (Get-VerdictText $verdict.view_p95) + " |")
}
if ($switchStats) {
    [void]$md.AppendLine("| page switch p95 | <= " + $thresholds.PageSwitchP95 + " ms | " + (Get-VerdictText $verdict.page_switch_p95) + " |")
}
if ($fpsForVerdict) {
    [void]$md.AppendLine("| 帧率 avg | >= " + $thresholds.MinFps + " fps | " + (Get-VerdictText $verdict.fps) + " |")
}
$mdPath = Join-Path $OutDir "perf-report.md"
Set-Content -Path $mdPath -Value $md.ToString() -Encoding UTF8

Write-Host "`n== 结果 =="
if ($viewStats) { Write-Host ("view built：" + $viewStats.Count + " 帧（含启动），p50=" + $viewStats.P50 + "ms p95=" + $viewStats.P95 + "ms max=" + $viewStats.Max + "ms") } else { Write-Host "view built：无数据（旧构建？）" }
if ($viewSteadyStats) { Write-Host ("  稳态：" + $viewSteadyStats.Count + " 帧，p50=" + $viewSteadyStats.P50 + "ms p95=" + $viewSteadyStats.P95 + "ms max=" + $viewSteadyStats.Max + "ms") }
if ($switchStats) { Write-Host ("page switch：" + $switchStats.Count + " 次，p50=" + $switchStats.P50 + "ms p95=" + $switchStats.P95 + "ms") } else { Write-Host "page switch：无数据" }
if ($fpsStats) { Write-Host ("frame rate：avg=" + $fpsStats.Avg + "fps（样本 " + $fpsStats.Samples + "）") } else { Write-Host "frame rate：无数据" }
Write-Host ("冷启动：" + $coldStartMs + " ms（日志差 " + $coldStartLogMs + " ms）")
Write-Host ("JSON -> " + $jsonPath)
Write-Host ("Markdown -> " + $mdPath)
if ($errorText) { exit 1 }
