# web-remote 真机验证脚本（不属于产品代码）。
#
# 前置：已编译探针 —— cargo build --offline -p neosonia-web --bin web_probe
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File crates\neosonia-web\scripts\verify.ps1 [-Port 8799] [-SkipScreenshot]
#
# v0.6 重点：**全部用探针自己 advertise 的本机 LAN IP 访问**（不是 127.0.0.1），
# 验证「手机连不上」的根因（绑定地址/QR 地址）确实修好。
#
# 产出：logs/screenshot-web*.png、logs/verify-web.log、logs/web-probe-<port>.log。

param(
    [int]$Port = 8799,
    [switch]$SkipScreenshot
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8

$root = 'E:\NeoSonia'
$logDir = Join-Path $root 'logs'
$exe = Join-Path $root 'target\debug\web_probe.exe'
$wsProbe = Join-Path $PSScriptRoot 'ws_probe.mjs'
$spaCoreTest = Join-Path $PSScriptRoot 'test-spa-core.cjs'
$spaMdTest = Join-Path $PSScriptRoot 'test-spa-markdown.cjs'
$screenshotTool = Join-Path $env:USERPROFILE '.dsh\tools\firefox-screenshot.mjs'
$reportPath = Join-Path $logDir 'web-probe.json'

New-Item -ItemType Directory -Force -Path $logDir | Out-Null
if (-not (Test-Path $exe)) {
    throw "探针未编译：$exe`n先运行：cd $root; `$env:CARGO_TARGET_DIR='$root\target'; cargo build --offline -p neosonia-web --bin web_probe"
}

function Write-Section([string]$title) {
    Write-Host ''
    Write-Host ("=== " + $title + " ===") -ForegroundColor Cyan
}

function Show-Body($resp) {
    $t = ($resp.Body -join "`n").Trim()
    if ($t.Length -gt 1800) { $t = $t.Substring(0, 1800) + ' ...[截断]' }
    Write-Host "HTTP $($resp.Status)"
    if ($t) { Write-Host $t }
}

function Start-Probe([int]$probePort) {
    $out = Join-Path $logDir "web-probe-$probePort.log"
    $err = Join-Path $logDir "web-probe-$probePort.err.log"
    foreach ($p in @($reportPath, $out, $err)) { if (Test-Path $p) { Remove-Item $p -Force } }
    # 探针默认 --bind 0.0.0.0 --advertise auto（自动探测本机 LAN IP）。
    $proc = Start-Process -FilePath $exe -ArgumentList @('--port', "$probePort", '--seconds', '900') `
        -RedirectStandardOutput $out -RedirectStandardError $err -PassThru -WindowStyle Hidden
    $deadline = (Get-Date).AddSeconds(120)
    while (-not (Test-Path $reportPath)) {
        if ($proc.HasExited) { throw "探针 $probePort 提前退出（code $($proc.ExitCode)）。看 $out / $err" }
        if ((Get-Date) -gt $deadline) { throw "探针 $probePort 启动超时，看 $out" }
        Start-Sleep -Milliseconds 300
    }
    $info = Get-Content $reportPath -Raw | ConvertFrom-Json
    Write-Host "探针 pid=$($proc.Id) 监听=$($info.bind) 访问地址=$($info.advertise_url) 配对码=$($info.code)" -ForegroundColor Green
    return [pscustomobject]@{ Proc = $proc; Info = $info; Out = $out; Err = $err }
}

function Stop-Probe($handle) {
    if ($handle -and $handle.Proc -and -not $handle.Proc.HasExited) {
        Stop-Process -Id $handle.Proc.Id -Force -ErrorAction SilentlyContinue
        Write-Host "已停止探针 pid=$($handle.Proc.Id)"
    }
}

function Invoke-Curl {
    param(
        [string]$Method = 'GET',
        [string]$Url,
        [object]$Body = $null,
        [hashtable]$Headers = @{}
    )
    $curlArgs = @('-sS', '-X', $Method, $Url, '-w', "`n__STATUS__%{http_code}")
    foreach ($k in $Headers.Keys) { $curlArgs += @('-H', "$k`: $($Headers[$k])") }
    $tmp = $null
    if ($null -ne $Body) {
        $json = if ($Body -is [string]) { $Body } else { $Body | ConvertTo-Json -Compress }
        $tmp = [System.IO.Path]::GetTempFileName()
        $utf8 = New-Object System.Text.UTF8Encoding($false)
        [System.IO.File]::WriteAllText($tmp, $json, $utf8)
        $curlArgs += @('-H', 'Content-Type: application/json', '--data-binary', "@$tmp")
    }
    try {
        $raw = & curl.exe @curlArgs 2>&1 | Out-String
    } finally {
        if ($tmp) { Remove-Item $tmp -Force -ErrorAction SilentlyContinue }
    }
    $status = 0
    $m = [regex]::Match($raw, '__STATUS__(\d+)\s*$')
    if ($m.Success) { $status = [int]$m.Groups[1].Value }
    $text = [regex]::Replace($raw, "`n__STATUS__\d+\s*$", '')
    return [pscustomobject]@{ Status = $status; Body = $text }
}

function Shot([string]$name, [string]$url, [string]$out, [string]$waitForJs) {
    $path = Join-Path $logDir $out
    $shotArgs = @('--url', $url, '--out', $path, '--width', '430', '--height', '900', '--wait', '1500', '--wait-timeout', '40000', '--json')
    if ($waitForJs) { $shotArgs += @('--wait-for-js', $waitForJs) } else { $shotArgs += @('--wait-for', '#pair-btn') }
    & node $screenshotTool @shotArgs
    if ($LASTEXITCODE -ne 0) {
        # 机器负载高时 BiDi evaluate 偶发超时：等 1 秒重试一次（同一个探针还在跑）。
        Write-Host "截图失败：$name，重试一次…" -ForegroundColor Yellow
        Start-Sleep -Seconds 1
        & node $screenshotTool @shotArgs
        if ($LASTEXITCODE -ne 0) { Write-Host "截图仍失败：$name" -ForegroundColor Red }
    }
    Write-Host "$name -> $path"
}

# ---------- 阶段 A 截图表达式（扫码后自动配对，无需点击按钮） ----------

$jsPairHelp = @'
(() => {
  const details = document.getElementById('pair-help');
  if (!details) return false;
  if (!details.open) { details.open = true; return false; }
  const origin = document.getElementById('pair-origin');
  const warn = document.getElementById('pair-loopback-warn');
  if (!origin || origin.textContent === '—') return false;
  const originText = origin.textContent || '';
  if (originText.indexOf('127.0.0.1') >= 0 || originText.indexOf('localhost') >= 0) return false;
  if (warn && !warn.classList.contains('hidden')) return false;
  const toast = document.getElementById('toast');
  if (toast) toast.classList.add('hidden');
  return true;
})()
'@

$jsAutoChat = @'
(() => {
  const pair = document.getElementById('pair-view');
  if (pair && !pair.classList.contains('hidden')) return false;
  const app = document.getElementById('app-view');
  if (!app || app.classList.contains('hidden')) return false;
  const chat = document.getElementById('chat-view');
  const msg = document.getElementById('messages');
  if (!chat || chat.classList.contains('hidden') || !msg) return false;
  if (document.querySelectorAll('#messages > *').length < 2) return false;
  // v0.7：自动打开的是「上传附件测试」会话，等附件卡 + 图片真的加载出来。
  const cards = document.querySelectorAll('#messages .attach-card');
  if (cards.length < 1) return false;
  const imgs = document.querySelectorAll('#messages .attach-card img');
  for (let i = 0; i < imgs.length; i++) {
    if (!imgs[i].complete || !imgs[i].naturalWidth) return false;
  }
  return (msg.scrollHeight - msg.scrollTop - msg.clientHeight) < 40;
})()
'@

$jsEarlier = @'
(() => {
  const pair = document.getElementById('pair-view');
  if (pair && !pair.classList.contains('hidden')) return false;
  const app = document.getElementById('app-view');
  if (!app || app.classList.contains('hidden')) return false;
  // 切到「长会话分页测试」会话（不是自动打开的那个）。
  if (!window.__openedLong) {
    const items = Array.from(document.querySelectorAll('#session-list .session-item'));
    const target = items.find(function (it) { return (it.textContent || '').indexOf('长会话分页测试') >= 0; });
    if (!target) return false;
    window.__openedLong = 1;
    target.click();
    return false;
  }
  const chat = document.getElementById('chat-view');
  if (!chat || chat.classList.contains('hidden')) return false;
  const msg = document.getElementById('messages');
  const btn = document.querySelector('#messages .earlier-wrap button');
  if (!msg || !btn) return false;
  if (msg.scrollTop > 0) { msg.scrollTop = 0; return false; }
  return document.querySelectorAll('#messages > *').length >= 2;
})()
'@

$jsMarkdown = @'
(() => {
  const pair = document.getElementById('pair-view');
  if (pair && !pair.classList.contains('hidden')) return false;
  const app = document.getElementById('app-view');
  if (!app || app.classList.contains('hidden')) return false;
  if (!window.__openedMarkdown) {
    const items = Array.from(document.querySelectorAll('#session-list .session-item'));
    const target = items.find(function (it) { return (it.textContent || '').indexOf('表格公式渲染测试') >= 0; });
    if (!target) return false;
    window.__openedMarkdown = 1;
    target.click();
    return false;
  }
  const chat = document.getElementById('chat-view');
  const msg = document.getElementById('messages');
  if (!chat || chat.classList.contains('hidden') || !msg) return false;
  const table = document.querySelector('#messages .table-wrap table');
  const blockMath = document.querySelector('#messages .math-block');
  const inlineMath = document.querySelector('#messages .math');
  const code = document.querySelector('#messages .code-block');
  const lang = document.querySelector('#messages .code-lang');
  const copy = document.querySelector('#messages .code-copy');
  if (!(table && blockMath && inlineMath && code && lang && copy)) return false;
  if (msg.scrollTop > 0) { msg.scrollTop = 0; return false; }
  const toast = document.getElementById('toast');
  if (toast) toast.classList.add('hidden');
  return true;
})()
'@

$jsCode = @'
(() => {
  const pair = document.getElementById('pair-view');
  if (pair && !pair.classList.contains('hidden')) return false;
  const app = document.getElementById('app-view');
  if (!app || app.classList.contains('hidden')) return false;
  if (!window.__openedMarkdown) {
    const items = Array.from(document.querySelectorAll('#session-list .session-item'));
    const target = items.find(function (it) { return (it.textContent || '').indexOf('表格公式渲染测试') >= 0; });
    if (!target) return false;
    window.__openedMarkdown = 1;
    target.click();
    return false;
  }
  const chat = document.getElementById('chat-view');
  const msg = document.getElementById('messages');
  if (!chat || chat.classList.contains('hidden') || !msg) return false;
  const block = document.querySelector('#messages .code-block');
  if (!block) return false;
  block.scrollIntoView({ block: 'center' });
  const lang = document.querySelector('#messages .code-lang');
  const copy = document.querySelector('#messages .code-copy');
  if (!(lang && copy)) return false;
  const toast = document.getElementById('toast');
  if (toast) toast.classList.add('hidden');
  return true;
})()
'@

$jsDeviceRename = @'
(() => {
  const pair = document.getElementById('pair-view');
  if (pair && !pair.classList.contains('hidden')) return false;
  const app = document.getElementById('app-view');
  if (!app || app.classList.contains('hidden')) return false;
  if (document.querySelectorAll('#session-list .session-item').length < 1) return false;
  const dev = document.getElementById('device-btn');
  if (dev && !dev.dataset.k) { dev.dataset.k = '1'; dev.click(); }
  const view = document.getElementById('device-view');
  if (!view || view.classList.contains('hidden')) return false;
  const edits = document.querySelectorAll('#device-list .device-row-edit input');
  if (edits.length < 1) return false;
  view.scrollTop = view.scrollHeight;
  const toast = document.getElementById('toast');
  if (toast) toast.classList.add('hidden');
  return true;
})()
'@

$jsProvider = @'
(() => {
  const pair = document.getElementById('pair-view');
  if (pair && !pair.classList.contains('hidden')) return false;
  const app = document.getElementById('app-view');
  if (!app || app.classList.contains('hidden')) return false;
  const ball = document.getElementById('provider-ball');
  if (!ball || ball.classList.contains('hidden')) return false;
  if ((ball.className || '').indexOf('ind-') < 0) return false;
  // 截图里只留状态小球：隐藏旁边的连接状态点，避免两个圆点混淆。
  const conn = document.getElementById('conn-dot');
  if (conn) conn.style.display = 'none';
  const toast = document.getElementById('toast');
  if (toast) toast.classList.add('hidden');
  return true;
})()
'@

$jsProviderDetail = @'
(() => {
  const pair = document.getElementById('pair-view');
  if (pair && !pair.classList.contains('hidden')) return false;
  const app = document.getElementById('app-view');
  if (!app || app.classList.contains('hidden')) return false;
  if (document.querySelectorAll('#session-list .session-item').length < 1) return false;
  const dev = document.getElementById('device-btn');
  if (dev && !dev.dataset.k) { dev.dataset.k = '1'; dev.click(); }
  const view = document.getElementById('device-view');
  if (!view || view.classList.contains('hidden')) return false;
  const ball = document.getElementById('provider-ball');
  if (!ball || ball.classList.contains('hidden')) return false;
  const row = document.getElementById('srv-provider');
  if (!row || !row.textContent || row.textContent === '—') return false;
  row.scrollIntoView({ block: 'center' });
  const conn = document.getElementById('conn-dot');
  if (conn) conn.style.display = 'none';
  const toast = document.getElementById('toast');
  if (toast) toast.classList.add('hidden');
  return true;
})()
'@

$jsUploadBar = @'
(() => {
  const pair = document.getElementById('pair-view');
  if (pair && !pair.classList.contains('hidden')) return false;
  const app = document.getElementById('app-view');
  if (!app || app.classList.contains('hidden')) return false;
  const tools = document.getElementById('composer-tools');
  if (!tools || tools.classList.contains('hidden')) return false;
  const btn = document.getElementById('attach-btn');
  if (!btn) return false;
  const rect = btn.getBoundingClientRect();
  if (rect.width < 20 || rect.height < 12) return false;
  const ball = document.getElementById('provider-ball');
  if (!ball || ball.classList.contains('hidden')) return false;
  const progress = document.getElementById('attach-progress');
  if (!progress) return false;
  const conn = document.getElementById('conn-dot');
  if (conn) conn.style.display = 'none';
  const toast = document.getElementById('toast');
  if (toast) toast.classList.add('hidden');
  return true;
})()
'@

$jsLinks = @'
(() => {
  const pair = document.getElementById('pair-view');
  if (pair && !pair.classList.contains('hidden')) return false;
  const app = document.getElementById('app-view');
  if (!app || app.classList.contains('hidden')) return false;
  if (!window.__openedMarkdown) {
    const items = Array.from(document.querySelectorAll('#session-list .session-item'));
    const target = items.find(function (it) { return (it.textContent || '').indexOf('表格公式渲染测试') >= 0; });
    if (!target) return false;
    window.__openedMarkdown = 1;
    target.click();
    return false;
  }
  const chat = document.getElementById('chat-view');
  const msg = document.getElementById('messages');
  if (!chat || chat.classList.contains('hidden') || !msg) return false;
  const items = document.querySelectorAll('#messages ol li');
  const links = document.querySelectorAll('#messages a');
  const files = document.querySelectorAll('#messages .file-link');
  if (items.length < 5 || links.length < 2 || files.length < 1) return false;
  items[0].scrollIntoView({ block: 'center' });
  const toast = document.getElementById('toast');
  if (toast) toast.classList.add('hidden');
  return true;
})()
'@

$jsReview = @'
(() => {
  const pair = document.getElementById('pair-view');
  if (pair && !pair.classList.contains('hidden')) return false;
  const app = document.getElementById('app-view');
  if (!app || app.classList.contains('hidden')) return false;
  const bar = document.getElementById('review-bar');
  if (!bar || bar.classList.contains('hidden')) return false;
  const toggle = document.getElementById('review-toggle');
  if (toggle && !toggle.dataset.k) { toggle.dataset.k = '1'; toggle.click(); }
  const list = document.getElementById('review-list');
  return !!(list && !list.classList.contains('hidden') && document.querySelectorAll('#review-list .review-item').length >= 1);
})()
'@

$jsReconnect = @'
(() => {
  const pair = document.getElementById('pair-view');
  if (pair && !pair.classList.contains('hidden')) return false;
  const app = document.getElementById('app-view');
  if (!app || app.classList.contains('hidden')) return false;
  const msg = document.getElementById('messages');
  if (!msg || document.querySelectorAll('#messages > *').length < 2) return false;
  if (window.__neosoniaDebug && !window.__offlineTriggered) {
    window.__offlineTriggered = 1;
    window.__neosoniaDebug.simulateOffline();
  }
  const banner = document.getElementById('conn-banner');
  return !!(banner && !banner.classList.contains('hidden'));
})()
'@

$jsDevice = @'
(() => {
  const pair = document.getElementById('pair-view');
  if (pair && !pair.classList.contains('hidden')) return false;
  const app = document.getElementById('app-view');
  if (!app || app.classList.contains('hidden')) return false;
  if (document.querySelectorAll('#session-list .session-item').length < 1) return false;
  const dev = document.getElementById('device-btn');
  if (dev && !dev.dataset.k) { dev.dataset.k = '1'; dev.click(); }
  const view = document.getElementById('device-view');
  if (!view || view.classList.contains('hidden')) return false;
  const conn = document.getElementById('dev-conn');
  const bindNode = document.getElementById('srv-bind');
  const advertiseNode = document.getElementById('srv-advertise');
  const clientNode = document.getElementById('dev-client');
  const heartbeat = document.getElementById('dev-heartbeat');
  const sessions = document.getElementById('perf-sessions');
  const rows = document.querySelectorAll('#device-list .device-row');
  if (!(conn && (conn.textContent === '在线' || conn.textContent === '重连中'))) return false;
  if (!(bindNode && bindNode.textContent.indexOf('0.0.0.0') >= 0)) return false;
  if (!(advertiseNode && advertiseNode.textContent.indexOf('http://') === 0 && advertiseNode.textContent.indexOf('127.0.0.1') < 0)) return false;
  if (!(clientNode && clientNode.textContent !== '—')) return false;
  if (!(heartbeat && heartbeat.textContent !== '—')) return false;
  if (!(sessions && sessions.textContent !== '—')) return false;
  if (rows.length < 1) return false;
  // v0.7：设备列表每行有重命名输入框 + 按钮。
  if (document.querySelectorAll('#device-list .device-row-edit input').length < 1) return false;
  if (document.querySelectorAll('#device-list .device-row-edit button').length < 1) return false;
  const toast = document.getElementById('toast');
  if (toast) toast.classList.add('hidden');
  return true;
})()
'@

# ============================================================
# 阶段 A：SPA 截图（LAN URL；每张一个新探针 + 新配对码）
# ============================================================
if (-not $SkipScreenshot) {
    $shots = @(
        @{ name = '配对页（LAN 地址，手动输入）'; out = 'screenshot-web-pair.png'; path = '/pair'; js = '' },
        @{ name = '配对页排障折叠（LAN 地址/防火墙/AP 隔离）'; out = 'screenshot-web-pair-help.png'; path = '/pair'; js = $jsPairHelp },
        @{ name = '扫码自动配对后聊天页（上传附件卡）'; out = 'screenshot-web.png'; path = ''; js = $jsAutoChat },
        @{ name = 'Markdown 表格/公式/代码块'; out = 'screenshot-web-markdown.png'; path = ''; js = $jsMarkdown },
        @{ name = 'Markdown 代码块（语言标注/复制/横向滚动）'; out = 'screenshot-web-code.png'; path = ''; js = $jsCode },
        @{ name = '加载更早'; out = 'screenshot-web-earlier.png'; path = ''; js = $jsEarlier },
        @{ name = '审查警示条'; out = 'screenshot-web-review.png'; path = ''; js = $jsReview },
        @{ name = '断线重连提示'; out = 'screenshot-web-reconnect.png'; path = ''; js = $jsReconnect },
        @{ name = '设备页（监听/访问地址/客户端 IP/心跳/重命名）'; out = 'screenshot-web-device.png'; path = ''; js = $jsDevice },
        @{ name = '设备重命名入口'; out = 'screenshot-web-device-rename.png'; path = ''; js = $jsDeviceRename },
        @{ name = '顶栏 API 状态小球（DeepSeek）'; out = 'screenshot-web-provider.png'; path = ''; js = $jsProvider },
        @{ name = '设备页 API 状态（绿色也显示 detail）'; out = 'screenshot-web-provider-detail.png'; path = ''; js = $jsProviderDetail },
        @{ name = '输入框横栏（上传按钮 + API 球 + 进度占位）'; out = 'screenshot-web-toolbar.png'; path = ''; js = $jsUploadBar },
        @{ name = '链接与有序列表渲染'; out = 'screenshot-web-links.png'; path = ''; js = $jsLinks }
    )
    $shotIndex = 0
    foreach ($s in $shots) {
        $shotPort = $Port + 1 + $shotIndex
        $shotIndex++
        $handle = $null
        try {
            Write-Section "阶段 A$shotIndex：$($s.name)（探针 127.0.0.1:$shotPort）"
            $handle = Start-Probe $shotPort
            $script:lanBase = $handle.Info.advertise_url
            if ($s.path) { $url = "$($script:lanBase)$($s.path)" }
            else { $url = "$($script:lanBase)/pair#$($handle.Info.code)" }
            Shot $s.name $url $s.out $s.js
        } finally {
            Stop-Probe $handle
        }
    }
}

# ============================================================
# 阶段 B：SPA 单测 + API / WS（全部走 LAN IP）
# ============================================================
$apiHandle = $null
try {
    Write-Section '0. SPA 纯逻辑单测（Node：心跳/退避/配对文案）'
    & node $spaCoreTest
    if ($LASTEXITCODE -ne 0) { throw 'SPA 纯逻辑单测失败' }

    Write-Section '0b. Markdown/表格/公式渲染单测（Node：GFM 表格/轻量公式/代码块/附件标记）'
    & node $spaMdTest
    if ($LASTEXITCODE -ne 0) { throw 'Markdown 渲染单测失败' }

    Write-Section "B：启动探针（API 用）127.0.0.1:$Port"
    $apiHandle = Start-Probe $Port
    $info = $apiHandle.Info
    $script:base = "http://127.0.0.1:$($info.port)"
    $script:lanBase = $info.advertise_url
    Write-Host "监听=$($info.bind)  访问地址=$($info.advertise_url)  配对码=$($info.code)  长会话=$($info.long_conv_id)"

    Write-Section '1. LAN 地址可达性（本次修复核心）：curl http://<LAN_IP>:<port>/healthz'
    $health = Invoke-Curl -Url "$($script:lanBase)/healthz"
    Show-Body $health
    $healthJson = $null
    try { $healthJson = $health.Body | ConvertFrom-Json } catch {}
    Write-Host ("绑定是 0.0.0.0 = " + ($healthJson.bind -like '0.0.0.0:*'))
    Write-Host ("访问地址不是回环 = " + ($healthJson.advertise_url -notlike '*127.0.0.1*'))
    Write-Host ("lan_only = " + $healthJson.lan_only)

    Write-Section '1b. 对照：127.0.0.1 也能访问（本机回环）'
    $localHealth = Invoke-Curl -Url "$($script:base)/healthz"
    Write-Host "HTTP $($localHealth.Status) (bind=$($healthJson.bind))"

    Write-Section '2. POST /api/pair 错误配对码（应 401，走 LAN 地址）'
    Show-Body (Invoke-Curl -Method POST -Url "$($script:lanBase)/api/pair" -Body @{ code = 'ZZZZZZZZ'; name = 'Probe' })

    Write-Section '3. POST /api/pair 正确配对码（应 200 + 64 位 token + device）'
    $r = Invoke-Curl -Method POST -Url "$($script:lanBase)/api/pair" -Body @{
        code = $info.code
        name = 'web-probe 手机'
    } -Headers @{ 'User-Agent' = 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) NeoSoniaProbe' }
    Show-Body $r
    $pair = $r.Body | ConvertFrom-Json
    $token = $pair.token
    if (-not $token) { throw '配对响应缺少 token' }
    Write-Host "token 长度 $($token.Length)（64 = 32 字节 hex）"
    $auth = @{ Authorization = "Bearer $token" }

    Write-Section '4. POST /api/pair 再复用同一配对码（一次性，应 401）'
    Show-Body (Invoke-Curl -Method POST -Url "$($script:lanBase)/api/pair" -Body @{ code = $info.code; name = 'Replay' })

    Write-Section '5. GET /api/me（LAN 地址；bind/advertise_url/client_ip/provider_status）'
    $meResp = Invoke-Curl -Url "$($script:lanBase)/api/me" -Headers $auth
    Show-Body $meResp
    $meJson = $null
    try { $meJson = $meResp.Body | ConvertFrom-Json } catch {}
    $pstatus = $meJson.server.provider_status
    if ($null -eq $pstatus) {
        Write-Host 'provider_status 首次为 null（后台预热中，状态页较慢），最多等 12s 重试…' -ForegroundColor Yellow
        for ($i = 1; $i -le 6; $i++) {
            Start-Sleep -Seconds 2
            $meResp = Invoke-Curl -Url "$($script:lanBase)/api/me" -Headers $auth
            $meJson = $null
            try { $meJson = $meResp.Body | ConvertFrom-Json } catch {}
            $pstatus = $meJson.server.provider_status
            if ($null -ne $pstatus) { Write-Host "第 $i 次重试拿到状态" -ForegroundColor Green; break }
        }
    }
    if ($null -eq $pstatus) {
        Write-Host 'provider_status = null（未知/失败，SPA 隐藏小球）' -ForegroundColor Yellow
    } else {
        Write-Host ("provider_status = " + $pstatus.provider + ' / ' + $pstatus.indicator + ' / ' + $pstatus.label + ' / ' + $pstatus.url)
        Write-Host ("小球应为绿色 = " + ($pstatus.indicator -eq 'green'))
    }

    Write-Section '6. GET /api/sessions?limit=20（会话列表）'
    Show-Body (Invoke-Curl -Url "$($script:lanBase)/api/sessions?limit=20" -Headers $auth)

    Write-Section '7. GET /api/sessions/probe-conv-1?limit=50（消息：has_image/image_count + seq）'
    Show-Body (Invoke-Curl -Url "$($script:lanBase)/api/sessions/probe-conv-1?limit=50" -Headers $auth)

    Write-Section '8. 分页：probe-long 最后一页（limit=5）'
    $r = Invoke-Curl -Url "$($script:lanBase)/api/sessions/probe-long?limit=5" -Headers $auth
    $page1 = $r.Body | ConvertFrom-Json
    $seq1 = @($page1 | ForEach-Object { $_.seq })
    Write-Host "HTTP $($r.Status) 返回 $($page1.Count) 条，seq = $($seq1 -join ',')"
    $firstSeq = $seq1[0]

    Write-Section "9. 分页：before=$firstSeq 取更早一页（limit=5，seq 应连续不重叠）"
    $r2 = Invoke-Curl -Url "$($script:lanBase)/api/sessions/probe-long?before=$firstSeq&limit=5" -Headers $auth
    $page2 = $r2.Body | ConvertFrom-Json
    $seq2 = @($page2 | ForEach-Object { $_.seq })
    Write-Host "HTTP $($r2.Status) 返回 $($page2.Count) 条，seq = $($seq2 -join ',')"
    if ($page2.Count -gt 0 -and $seq2[-1] -eq ($firstSeq - 1)) {
        Write-Host '分页连续：更早一页最后一条 seq = 上一页第一条 seq - 1' -ForegroundColor Green
    } else {
        Write-Host '分页不连续！' -ForegroundColor Yellow
    }

    Write-Section '10. GET /api/review/pending（只读审查队列）'
    Show-Body (Invoke-Curl -Url "$($script:lanBase)/api/review/pending" -Headers $auth)

    Write-Section '11. GET /api/screenshot/probe-shot.png（鉴权 + PNG）'
    $shotOut = Join-Path $logDir 'web-probe-shot.png'
    if (Test-Path $shotOut) { Remove-Item $shotOut -Force }
    $curlArgs = @(
        '-sS', '-o', $shotOut, '-w', 'HTTP:%{http_code} TYPE:%{content_type} SIZE:%{size_download}',
        "$($script:lanBase)/api/screenshot/probe-shot.png", '-H', "Authorization: Bearer $token"
    )
    Write-Host ((& curl.exe @curlArgs 2>&1 | Out-String).Trim())

    Write-Section '12. GET /api/screenshot/..%2Fdevices.json（目录穿越，应 400）'
    Show-Body (Invoke-Curl -Url "$($script:lanBase)/api/screenshot/%2e%2e%2fdevices.json" -Headers $auth)

    Write-Section '10b. 附件上传：POST /api/upload → GET /api/uploads/{id} → chat attachments'
    $tmpText = Join-Path $logDir 'probe-upload.md'
    [System.IO.File]::WriteAllText($tmpText, "# 上传测试`n远程上传的文本附件。", (New-Object System.Text.UTF8Encoding($false)))
    $up = & curl.exe -sS -m 20 -X POST -H "Authorization: Bearer $token" -H "Content-Type: text/markdown" --data-binary "@$tmpText" "$($script:lanBase)/api/upload?name=probe-upload.md"
    Write-Host $up
    $upInfo = $up | ConvertFrom-Json
    if (-not $upInfo.id) { throw '上传响应缺少 id' }
    Write-Host "附件 id=$($upInfo.id) kind=$($upInfo.kind) size=$($upInfo.size) url=$($upInfo.url)"
    $dlPath = Join-Path $logDir 'probe-upload-download.md'
    $dl = & curl.exe -sS -m 20 -o $dlPath -w "HTTP:%{http_code} SIZE:%{size_download}" -H "Authorization: Bearer $token" "$($script:lanBase)$($upInfo.url)"
    Write-Host $dl
    $origSize = (Get-Item $tmpText).Length
    $dlSize = (Get-Item $dlPath).Length
    Write-Host ("回读大小一致 = " + ($origSize -eq $dlSize) + "（$origSize / $dlSize 字节）")

    # 图片上传（1x1 PNG → 服务端 kind=image）
    $pngPath = Join-Path $logDir 'probe-upload.png'
    [System.IO.File]::WriteAllBytes($pngPath, [Convert]::FromBase64String('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=='))
    $upImg = & curl.exe -sS -m 20 -X POST -H "Authorization: Bearer $token" -H "Content-Type: image/png" --data-binary "@$pngPath" "$($script:lanBase)/api/upload?name=probe-upload.png"
    $upImgInfo = $upImg | ConvertFrom-Json
    Write-Host "图片附件: $upImg"
    Write-Host ("图片 kind=image = " + ($upImgInfo.kind -eq 'image'))

    $chatBody = @{ text = '请查看我上传的附件'; attachments = @($upInfo.id, $upImgInfo.id) } | ConvertTo-Json -Compress
    $chatRes = Invoke-Curl -Method POST -Url "$($script:lanBase)/api/chat" -Headers $auth -Body $chatBody
    Show-Body $chatRes
    Remove-Item $tmpText, $dlPath, $pngPath -Force -ErrorAction SilentlyContinue

    Write-Section '10c. 设备重命名：POST /api/devices/{id}/name → GET /api/devices'
    $deviceId = $pair.device.id
    $rn = Invoke-Curl -Method POST -Url "$($script:lanBase)/api/devices/$deviceId/name" -Headers $auth -Body @{ name = '重命名测试手机' }
    Show-Body $rn
    $devicesNow = (Invoke-Curl -Url "$($script:lanBase)/api/devices" -Headers $auth).Body | ConvertFrom-Json
    $renamed = @($devicesNow | Where-Object { $_.name -eq '重命名测试手机' }).Count
    Write-Host ("设备重命名已生效 = " + ($renamed -ge 1))

    Write-Section '13. 静态资源缓存头（HTML no-cache / assets immutable + 版本号）'
    $htmlHeaders = (& curl.exe -sS -D - -o NUL "$($script:lanBase)/" 2>&1 | Out-String)
    $assetHeaders = (& curl.exe -sS -D - -o NUL "$($script:lanBase)/app.js?v=test" 2>&1 | Out-String)
    $mdHeaders = (& curl.exe -sS -D - -o NUL "$($script:lanBase)/md-core.js?v=test" 2>&1 | Out-String)
    Write-Host ("HTML cache-control no-cache = " + ($htmlHeaders -match 'cache-control: no-cache'))
    Write-Host ("app.js immutable = " + ($assetHeaders -match 'immutable'))
    Write-Host ("md-core.js immutable = " + ($mdHeaders -match 'immutable'))

    Write-Section '14. 安全头 + 移动端/排障/设备字段 HTML 钩子（GET /，LAN）'
    $page = (Invoke-Curl -Url "$($script:lanBase)/").Body
    Write-Host ("viewport = " + $page.Contains('name="viewport"'))
    Write-Host ("standalone(apple) = " + $page.Contains('apple-mobile-web-app-capable'))
    Write-Host ("排障折叠 id = " + $page.Contains('id="pair-help"'))
    Write-Host ("当前访问地址 id = " + $page.Contains('id="pair-origin"'))
    Write-Host ("127 回环警告 id = " + $page.Contains('id="pair-loopback-warn"'))
    Write-Host ("防火墙排障文案 = " + $page.Contains('netsh advfirewall firewall add rule'))
    Write-Host ("服务监听字段 id = " + $page.Contains('id="srv-bind"'))
    Write-Host ("访问地址字段 id = " + $page.Contains('id="srv-advertise"'))
    Write-Host ("客户端 IP 字段 id = " + $page.Contains('id="dev-client"'))
    Write-Host ("最后心跳字段 id = " + $page.Contains('id="dev-heartbeat"'))
    Write-Host ("API 状态小球 id = " + $page.Contains('id="provider-ball"'))
    Write-Host ("输入框横栏 id = " + $page.Contains('id="composer-tools"'))
    Write-Host ("横栏上传按钮 = " + ($page.Contains('id="attach-btn"')))
    Write-Host ("横栏进度占位 = " + $page.Contains('id="attach-progress"'))
    Write-Host ("API 状态字段 id = " + $page.Contains('id="srv-provider"'))
    Write-Host ("状态图例 = " + ($page.Contains('绿=正常') -and $page.Contains('红=严重中断')))
    Write-Host ("品牌 logo = " + ($page.Contains('class="brand-logo"') -and $page.Contains('/logo-64.png?v=')))
    Write-Host ("拖拽浮层 id = " + ($page.Contains('id="drop-overlay"') -and $page.Contains('id="drop-count"')))
    Write-Host ("md-core.js 引用 = " + $page.Contains('/md-core.js?v='))
    Write-Host ("附件按钮 id = " + $page.Contains('id="attach-btn"'))
    Write-Host ("文件选择 id = " + $page.Contains('id="file-input"'))
    Write-Host ("待发送附件条 id = " + $page.Contains('id="attach-strip"'))
    Write-Host ("页面无回环 URL 硬编码 = " + (-not $page.Contains('http://127.0.0.1')))
    Write-Host ("资源版本号 = " + $page.Contains('?v='))

    Write-Section '15. WebSocket（LAN 地址；hello + resume + pending_reviews + risk_review + pong/cancel）'
    $wsBase = $script:lanBase -replace '^http', 'ws'
    & node $wsProbe --url "$wsBase/ws?token=$token" --append "conv_id=probe-long&since_seq=240" --frames 8 --timeout 16000
    if ($LASTEXITCODE -ne 0) { Write-Host "WS 探针失败（code $LASTEXITCODE）" -ForegroundColor Yellow }

    Write-Section '16. 探针日志末尾'
    if (Test-Path $apiHandle.Out) { Get-Content $apiHandle.Out -Tail 10 | ForEach-Object { Write-Host $_ } }
} finally {
    Stop-Probe $apiHandle
}

Write-Host ''
Write-Host 'VERIFY_DONE' -ForegroundColor Green


