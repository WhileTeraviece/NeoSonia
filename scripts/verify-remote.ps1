# 局域网 Web 服务端到端验证（配对 → 鉴权 → 会话 → 聊天 → SPA 截图）
# 用法: powershell -ExecutionPolicy Bypass -File scripts\verify-remote.ps1
param(
    [string]$NeoHome = "E:\NeoSonia\sandbox",
    [int]$Port = 8787,
    [string]$OutDir = "E:\NeoSonia\logs\remote"
)

$ErrorActionPreference = "Continue"
New-Item -ItemType Directory -Force $OutDir | Out-Null
$exe = "E:\NeoSonia\target\debug\neosonia.exe"
if (-not (Test-Path $exe)) { Write-Host "缺少 exe: $exe"; exit 1 }

# 1) 准备 sandbox 配置：开启远程访问
$cfg = Join-Path $NeoHome "config\settings.toml"
if (-not (Test-Path $cfg)) { Copy-Item "E:\NeoSonia\sandbox\config\settings.toml" $cfg -ErrorAction SilentlyContinue }
$raw = Get-Content $cfg -Raw
if ($raw -notmatch '(?m)^\[remote\]') {
    $raw += @"

[remote]
enabled = true
bind = "0.0.0.0"
port = $Port
require_pairing = true
max_devices = 8
lan_only = true
pair_ttl_secs = 600
"@
} else {
    $raw = $raw -replace 'enabled = false', 'enabled = true'
}
Set-Content $cfg $raw -Encoding utf8

# 2) 重启 app（清日志）
Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800
$log = Join-Path $NeoHome "logs\neosonia.log"
Remove-Item $log -ErrorAction SilentlyContinue

$env:NEOSONIA_API_KEY = $env:NEOSONIA_API_KEY
$proc = Start-Process -FilePath $exe -ArgumentList "--home", $NeoHome -PassThru
Write-Host "app pid=$($proc.Id)，等待 Web 服务启动…"

$url = $null
$code = $null
for ($i = 0; $i -lt 80; $i++) {
    Start-Sleep -Milliseconds 500
    if (Test-Path $log) {
        $m = Select-String -Path $log -Pattern 'LAN web server started.*url="?([^ "]+)"?.*code="?([^ ")"]+)"?' | Select-Object -Last 1
        if ($m) {
            $url = $m.Matches[0].Groups[1].Value
            $code = $m.Matches[0].Groups[2].Value
            break
        }
    }
}
if (-not $url) { Write-Host "未在日志里找到 Web 服务启动行"; Get-Content $log -Tail 20 -ErrorAction SilentlyContinue; exit 2 }
Write-Host "LAN url = $url"
Write-Host "pair code = $code"

$base = "http://127.0.0.1:$Port"

# 3) 未鉴权访问应 401
$r = try { Invoke-WebRequest "$base/api/sessions" -UseBasicParsing -TimeoutSec 10 } catch { $_.Exception.Response }
Write-Host "未鉴权 GET /api/sessions -> $([int]$r.StatusCode)（期望 401）"

# 4) 首页 SPA
$resp = Invoke-WebRequest "$base/" -UseBasicParsing -TimeoutSec 10
Write-Host "GET / -> $($resp.StatusCode), $($resp.RawContentLength) bytes"
Set-Content (Join-Path $OutDir "spa-index.html") $resp.Content -Encoding utf8

# 5) 配对
$name = "verify-" + (Get-Date -Format "HHmmss")
$body = @{ code = $code; name = $name; user_agent = "verify-script" } | ConvertTo-Json -Compress
$pair = Invoke-RestMethod "$base/api/pair" -Method Post -ContentType "application/json" -Body $body -TimeoutSec 15
Write-Host "POST /api/pair -> token len=$($pair.token.Length) device=$($pair.device.id)"
$token = $pair.token
Set-Content (Join-Path $OutDir "token.txt") $token -Encoding utf8 -NoNewline
$headers = @{ Authorization = "Bearer $token" }

# 6) 配对码是一次性的：再用一次应失败
try {
    Invoke-RestMethod "$base/api/pair" -Method Post -ContentType "application/json" -Body $body -TimeoutSec 10 | Out-Null
    Write-Host "复用配对码 -> 竟然成功了（不符合预期）"
} catch {
    Write-Host "复用配对码 -> 被拒绝（符合预期）"
}

# 7) 鉴权后接口
$me = Invoke-RestMethod "$base/api/me" -Headers $headers -TimeoutSec 10
Write-Host "GET /api/me -> device=$($me.device.name) version=$($me.server.version) busy=$($me.server.busy)"
$sessions = Invoke-RestMethod "$base/api/sessions" -Headers $headers -TimeoutSec 10
Write-Host "GET /api/sessions -> $($sessions.Count) 个会话"
$me | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $OutDir "me.json") -Encoding utf8

# 8) 提交一条消息（会真正调用模型；短问句）。用 UTF-8 字节 + curl，避免 PS5.1 把中文编成问号。
$chatBody = @{ text = "用一句话说明你是运行在我电脑上的助手。" } | ConvertTo-Json -Compress
$chatFile = Join-Path $OutDir "chat-body.json"
[System.IO.File]::WriteAllText($chatFile, $chatBody, (New-Object System.Text.UTF8Encoding($false)))
$code2 = & curl.exe -s -o (Join-Path $OutDir "chat-resp.json") -w "%{http_code}" -X POST "$base/api/chat" `
    -H "Authorization: Bearer $token" -H "Content-Type: application/json; charset=utf-8" `
    --data-binary "@$chatFile" --max-time 20
Write-Host "POST /api/chat -> HTTP $code2 ($(Get-Content (Join-Path $OutDir 'chat-resp.json') -Raw -ErrorAction SilentlyContinue))"

# 9) WebSocket：连接并读前几个事件（node 26 自带 WebSocket）
$wsJs = Join-Path $OutDir "ws-probe.mjs"
@"
const token = process.argv[2], base = process.argv[3];
const ws = new WebSocket(base.replace('http','ws') + '/ws?token=' + token);
let n = 0;
const t = setTimeout(() => { console.log('WS timeout after ' + n + ' events'); process.exit(0); }, 20000);
ws.onmessage = (e) => {
  n++;
  const s = String(e.data).slice(0, 220);
  console.log('WS#' + n + ': ' + s);
  if (n >= 6) { clearTimeout(t); ws.close(); process.exit(0); }
};
ws.onerror = (e) => { console.log('WS error'); };
"@ | Set-Content $wsJs -Encoding utf8
Write-Host "WS 事件（前 6 条）："
& node $wsJs $token $base 2>&1 | Select-Object -First 8

# 10) SPA 截图（配对页 / 首页）
$ff = "$env:USERPROFILE\.dsh\tools\firefox-screenshot.mjs"
if (Test-Path $ff) {
    & node $ff --url "$base/pair#$code" --out (Join-Path $OutDir "spa-pair.png") --wait 1500 2>&1 | Select-Object -Last 2
    Write-Host "配对页截图: $(Join-Path $OutDir 'spa-pair.png')"
} else {
    Write-Host "找不到 firefox-screenshot 工具，跳过 SPA 截图"
}

# 10.5) 已配对聊天页截图（Edge + CDP 注入 token，走真实 SPA + 真实模型回复）
$chatShot = Join-Path (Split-Path $PSScriptRoot -Parent) "scripts\spa-chat-shot.mjs"
if ((Test-Path $chatShot) -and (Test-Path (Join-Path $OutDir "token.txt"))) {
    $tk = Get-Content (Join-Path $OutDir "token.txt") -Raw
    & node $chatShot $base "" (Join-Path $OutDir "spa-chat.png") 9444 $tk 2>&1 | Select-Object -Last 4
    Write-Host "聊天页截图: $(Join-Path $OutDir 'spa-chat.png')"
}

# 11) 收尾：保留 app 运行（便于手动查看），打印服务状态
Write-Host "`n=== 完成 ==="
Write-Host "app: http://127.0.0.1:$Port/   配对码: $code"
