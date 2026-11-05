//! Chromium DevTools Protocol（CDP）工具：列目标、执行 JS、点击网页元素、截图。
//!
//! 需要目标应用启动时打开调试端口，例如：
//! `msedge.exe --remote-debugging-port=9222 --user-data-dir=C:\temp\edgedbg`
//! 或 Electron 应用加 `--remote-debugging-port=9222`。
//!
//! 实现要点：
//! - `GET http://127.0.0.1:{port}/json/list` 拿 targets（不走系统代理：reqwest `.no_proxy()`）；
//! - `tungstenite::connect` 连 `webSocketDebuggerUrl`，消息 id 递增，读响应直到 id 匹配或 10 秒超时；
//! - `Runtime.evaluate` 用 `returnByValue + awaitPromise`；
//! - `Page.captureScreenshot` 的 base64 解码后存 PNG 到 `ctx.paths().screenshots`。
//!
//! 所有工具都会先试“显式 port > settings.tools.cdp_port > 9222/9223/9224/9333”，
//! 方便同时开着 Chrome 和 Electron 的机器。两平台实现相同（CDP 与平台无关）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use neosonia_core::model::{ToolCtx, ToolOutcome, ToolSpec};
use neosonia_core::registry::{Tool, ToolRegistry};
use serde_json::{Value, json};

/// 单条 CDP 命令等待响应的超时（契约要求 10 秒）。
const CDP_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_CDP_PORT: u16 = 9222;
const FALLBACK_PORTS: [u16; 3] = [9223, 9224, 9333];

// ============================================================
// 参数解析（模型经常把数字写成字符串，全部容错）
// ============================================================

fn arg_str(args: &Value, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

fn arg_usize(args: &Value, key: &str) -> Option<usize> {
    match args.get(key) {
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|v| v as usize)
            .or_else(|| n.as_f64().map(|v| v.max(0.0) as usize)),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok().map(|v| v as usize),
        _ => None,
    }
}

fn arg_u16(args: &Value, key: &str) -> Option<u16> {
    arg_usize(args, key).and_then(|v| u16::try_from(v).ok())
}

fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    match args.get(key) {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        },
        Some(Value::Number(n)) => Some(n.as_i64() != Some(0)),
        _ => None,
    }
}

fn limit_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str(&format!("\n... [已截断，原文 {total} 字符] ..."));
    out
}

fn quote(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    format!("\"{}\"", limit_chars(&cleaned, 160).replace('"', "'"))
}

// ============================================================
// targets 解析与选择（纯逻辑，可单测）
// ============================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CdpTarget {
    pub id: String,
    pub title: String,
    pub url: String,
    pub ws_url: String,
    pub target_type: String,
}

/// 解析 `/json/list` 的 JSON。没有 `webSocketDebuggerUrl` 的条目（如 browser target）跳过。
pub(crate) fn parse_targets(value: &Value) -> Vec<CdpTarget> {
    let items = match value {
        Value::Array(items) => items.as_slice(),
        Value::Object(map) => map
            .get("targets")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        _ => &[],
    };
    let mut out = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let str_of = |key: &str| {
            obj.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let ws_url = str_of("webSocketDebuggerUrl");
        if ws_url.trim().is_empty() {
            continue;
        }
        let target_type = obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("page")
            .to_string();
        out.push(CdpTarget {
            id: str_of("id"),
            title: str_of("title"),
            url: str_of("url"),
            ws_url,
            target_type,
        });
    }
    out
}

/// 选目标：
/// - `target` 为空 => 第一个 type=page 的目标（没有就第一个）；
/// - 纯数字 => 下标（0 基）；
/// - 其它 => 在 url / title 里做大小写不敏感子串匹配，取第一个。
pub(crate) fn pick_target_index(targets: &[CdpTarget], target: Option<&str>) -> Option<usize> {
    if targets.is_empty() {
        return None;
    }
    let Some(raw) = target.map(str::trim).filter(|s| !s.is_empty()) else {
        return targets
            .iter()
            .position(|t| t.target_type.eq_ignore_ascii_case("page"))
            .or(Some(0));
    };
    if let Ok(idx) = raw.parse::<usize>() {
        return if idx < targets.len() { Some(idx) } else { None };
    }
    let needle = raw.to_lowercase();
    targets.iter().position(|t| {
        t.url.to_lowercase().contains(&needle) || t.title.to_lowercase().contains(&needle)
    })
}

fn format_target_brief(index: usize, t: &CdpTarget) -> String {
    format!(
        "#{} [{}] {} url={}",
        index,
        t.target_type,
        quote(&t.title),
        quote(&t.url)
    )
}

fn format_target_list(port: u16, targets: &[CdpTarget]) -> String {
    let pages = targets
        .iter()
        .filter(|t| t.target_type.eq_ignore_ascii_case("page"))
        .count();
    let mut lines = vec![format!(
        "[ok] 127.0.0.1:{port} 可调试目标 {} 个（page={pages}）",
        targets.len()
    )];
    for (i, t) in targets.iter().enumerate() {
        lines.push(format_target_brief(i, t));
        lines.push(format!("    ws: {}", t.ws_url));
    }
    if targets.len() > 1 {
        lines.push("提示：其它 cdp_* 工具用 target=下标 或 target=url/标题子串 选择目标；不传默认第一个 page。".into());
    }
    lines.join("\n")
}

// ============================================================
// JavaScript 表达式构造（纯函数，可单测）
// ============================================================

fn json_string_literal(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

const CLICK_SELECTOR_JS: &str = r#"(() => {
  const sel = __SEL__;
  let el = null;
  try { el = document.querySelector(sel); } catch (e) { return { ok: false, reason: "invalid_selector", message: String(e) }; }
  if (!el) { return { ok: false, reason: "not_found", selector: sel }; }
  el.scrollIntoView({ block: "center", inline: "center" });
  const label = (el.innerText || el.value || el.getAttribute("aria-label") || "").slice(0, 120);
  el.click();
  return { ok: true, tag: String(el.tagName || ""), label: label };
})()"#;

const CLICK_TEXT_JS: &str = r#"(() => {
  const target = __TEXT__;
  const norm = (s) => String(s || "").replace(/\s+/g, " ").trim().toLowerCase();
  const needle = norm(target);
  const selector = 'button, a, [role="button"], [role="link"], [role="menuitem"], [role="tab"], input[type="button"], input[type="submit"], [onclick], summary, label';
  const items = Array.from(document.querySelectorAll(selector));
  let matched = 0;
  let clicked = null;
  for (const el of items) {
    const label = norm(el.innerText || el.getAttribute("aria-label") || el.value || el.title || "");
    if (!label) { continue; }
    if (label === needle || label.includes(needle)) {
      matched += 1;
      if (!clicked) {
        el.scrollIntoView({ block: "center", inline: "center" });
        el.click();
        clicked = { tag: String(el.tagName || ""), label: (el.innerText || el.value || "").slice(0, 120) };
      }
    }
  }
  return { ok: matched > 0, matched: matched, clicked: clicked, target: target };
})()"#;

/// 构造按 CSS selector 点击的 JS 表达式。
pub(crate) fn click_selector_expression(selector: &str) -> String {
    CLICK_SELECTOR_JS.replace("__SEL__", &json_string_literal(selector))
}

/// 构造按可见文本点击的 JS 表达式（遍历常见可点击元素，取第一个匹配）。
pub(crate) fn click_text_expression(text: &str) -> String {
    CLICK_TEXT_JS.replace("__TEXT__", &json_string_literal(text))
}

// ============================================================
// CDP 响应解析（纯逻辑，可单测）
// ============================================================

/// 从 `Runtime.evaluate` 的 result 对象里取返回值；有 exceptionDetails 时转成错误。
pub(crate) fn extract_eval_value(result: &Value) -> Result<Value, String> {
    // 注意：CDP 正常响应里也可能带 "exceptionDetails": null，必须当成“没有异常”，
    // 不能只看 key 是否存在（否则把成功结果误判成脚本异常）。
    if let Some(exc) = result.get("exceptionDetails").filter(|v| !v.is_null()) {
        let text = exc
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("脚本执行异常");
        let desc = exc
            .pointer("/exception/description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let line = exc.get("lineNumber").and_then(Value::as_i64);
        let mut msg = format!("JavaScript 执行异常：{text}");
        if let Some(l) = line {
            msg.push_str(&format!("（行 {l}）"));
        }
        if !desc.is_empty() {
            msg.push_str(&format!("\n{desc}"));
        }
        return Err(msg);
    }
    let remote = result.get("result").cloned().unwrap_or(Value::Null);
    if let Some(v) = remote.get("value") {
        return Ok(v.clone());
    }
    if let Some(desc) = remote.get("description").and_then(Value::as_str) {
        return Ok(Value::String(desc.to_string()));
    }
    if let Some(t) = remote.get("type").and_then(Value::as_str) {
        return Ok(Value::String(format!("<{t}>")));
    }
    Ok(Value::Null)
}

/// 把任意 JSON 值转成给模型看的文本。
pub(crate) fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn response_error(value: &Value) -> Option<String> {
    value.get("error").map(|err| {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        let data = err.get("data").and_then(Value::as_str).unwrap_or("");
        if data.is_empty() {
            format!("CDP 错误 {code}：{message}")
        } else {
            format!("CDP 错误 {code}：{message}（{data}）")
        }
    })
}

// ============================================================
// HTTP：枚举 targets
// ============================================================

/// 记住每个端口上一次成功的 base URL：某些机器上 localhost 只有 IPv4 或只有 IPv6
/// （例如调试端口只监听 `[::1]`），记住后后续调用就不用逐个探测了。
fn preferred_base_cache() -> &'static std::sync::Mutex<std::collections::HashMap<u16, String>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u16, String>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 候选 base URL：IPv6 回环优先（连不上会立刻失败，几乎零成本），
/// 然后契约要求的 127.0.0.1，最后 localhost（照顾只监听 localhost 的环境）。
fn candidates_for_port(port: u16) -> Vec<String> {
    let mut bases: Vec<String> = Vec::new();
    if let Some(cached) = preferred_base_cache()
        .lock()
        .ok()
        .and_then(|m| m.get(&port).cloned())
    {
        bases.push(cached);
    }
    for base in [
        format!("http://[::1]:{port}"),
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ] {
        if !bases.contains(&base) {
            bases.push(base);
        }
    }
    bases
}

fn remember_base(port: u16, base: &str) {
    if let Ok(mut map) = preferred_base_cache().lock() {
        map.insert(port, base.to_string());
    }
}

async fn fetch_targets_on_port(port: u16) -> Result<Vec<CdpTarget>, String> {
    // 连接超时短一点：端口没人监听时快速失败，不影响后面的候选地址。
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_millis(1_500))
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败：{e}"))?;

    let mut last_error = String::from("没有收到响应");
    for base in candidates_for_port(port) {
        for path in ["/json/list", "/json"] {
            let url = format!("{base}{path}");
            match client.get(&url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    if !status.is_success() {
                        last_error = format!("{url} 返回 HTTP {status}");
                        continue;
                    }
                    match serde_json::from_str::<Value>(&body) {
                        Ok(value) => {
                            remember_base(port, &base);
                            return Ok(parse_targets(&value));
                        }
                        Err(e) => {
                            last_error = format!(
                                "{url} 返回的不是 JSON（{e}）：{}",
                                limit_chars(body.trim(), 200)
                            );
                        }
                    }
                }
                Err(e) => {
                    last_error = format!("{url} 请求失败：{e}");
                }
            }
        }
    }
    Err(last_error)
}

/// 依次尝试：显式 port -> settings.tools.cdp_port -> 常见端口；返回第一个可用的端口和目标列表。
async fn discover_targets(
    ctx: &ToolCtx,
    explicit_port: Option<u16>,
) -> Result<(u16, Vec<CdpTarget>), String> {
    let mut ports: Vec<u16> = Vec::new();
    if let Some(p) = explicit_port {
        ports.push(p);
    }
    let configured = ctx.settings().tools.cdp_port;
    if configured > 0 && !ports.contains(&configured) {
        ports.push(configured);
    }
    if !ports.contains(&DEFAULT_CDP_PORT) {
        ports.push(DEFAULT_CDP_PORT);
    }
    for p in FALLBACK_PORTS {
        if !ports.contains(&p) {
            ports.push(p);
        }
    }

    let mut errors: Vec<String> = Vec::new();
    for port in &ports {
        if ctx.is_cancelled() {
            return Err("操作已取消".to_string());
        }
        match fetch_targets_on_port(*port).await {
            Ok(targets) => return Ok((*port, targets)),
            Err(e) => errors.push(format!("{port}: {e}")),
        }
    }

    Err(format!(
        "无法连接 Chromium 调试端口。尝试了端口 {ports:?}，全部失败。\n最近错误：{}\n\
修复建议：\n1) 启动目标浏览器/Electron 应用时加参数 --remote-debugging-port=9222（例如：\n   msedge.exe --remote-debugging-port=9222 --user-data-dir=C:\\temp\\edgedbg）；\n2) 确认设置里 tools.cdp_enabled=true 且 tools.cdp_port 与实际端口一致；\n3) 端口只监听 127.0.0.1 时本机可直连，若被防火墙/代理拦截请检查。",
        errors.last().cloned().unwrap_or_default()
    ))
}

/// 打开目标：返回 (端口, 目标)。
async fn open_target(ctx: &ToolCtx, args: &Value) -> Result<(u16, CdpTarget), String> {
    let explicit = arg_u16(args, "port");
    let (port, targets) = discover_targets(ctx, explicit).await?;
    if targets.is_empty() {
        return Err(format!(
            "端口 {port} 上没有可调试的页面目标（可能是 browser target 或页面已关闭）。\n请先打开一个标签页，或调用 cdp_targets 查看。"
        ));
    }
    let target_arg = arg_str(args, "target");
    match pick_target_index(&targets, target_arg.as_deref()) {
        Some(idx) => Ok((port, targets[idx].clone())),
        None => {
            let detail = targets
                .iter()
                .take(10)
                .enumerate()
                .map(|(i, t)| format_target_brief(i, t))
                .collect::<Vec<_>>()
                .join("\n");
            Err(format!(
                "没有匹配 target={} 的目标。端口 {port} 当前目标：\n{detail}\n可用 target=下标（0 基）或 url/标题子串。",
                target_arg.unwrap_or_default()
            ))
        }
    }
}

// ============================================================
// WebSocket：阻塞实现（跑在 spawn_blocking 线程里）
// ============================================================

fn send_json(
    ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    value: &Value,
) -> Result<(), String> {
    let text = serde_json::to_string(value).map_err(|e| format!("序列化 CDP 命令失败：{e}"))?;
    ws.send(tungstenite::Message::Text(text))
        .map_err(|e| format!("发送 CDP 命令失败：{e}"))
}

/// 同步发一条 CDP 命令并等匹配 id 的响应。
fn cdp_request_blocking(
    ws_url: &str,
    method: &str,
    params: Value,
    timeout: Duration,
    ctx: &ToolCtx,
    runtime_enable: bool,
) -> Result<Value, String> {
    let (mut ws, _response) = tungstenite::connect(ws_url).map_err(|e| {
        format!(
            "连接 CDP WebSocket 失败：{e}\n地址：{ws_url}\n提示：目标可能已关闭，重新调用 cdp_targets 确认。"
        )
    })?;

    // 读超时设成 500ms：主循环可以周期性检查总超时与用户取消。
    if let tungstenite::stream::MaybeTlsStream::Plain(stream) = ws.get_ref() {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    }

    // 契约要求先 Runtime.enable；它的响应 id=1，会被下面的循环忽略。
    let mut next_id: u64 = 1;
    if runtime_enable {
        send_json(
            &mut ws,
            &json!({"id": next_id, "method": "Runtime.enable", "params": {}}),
        )?;
        next_id += 1;
    }
    let target_id = next_id;
    send_json(
        &mut ws,
        &json!({"id": target_id, "method": method, "params": params}),
    )?;

    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err(format!(
                "等待 {method} 响应超时（{} ms）。页面可能卡住或调试端口无响应。",
                timeout.as_millis()
            ));
        }
        if ctx.is_cancelled() {
            let _ = ws.close(None);
            return Err("操作已取消".to_string());
        }
        match ws.read() {
            Ok(tungstenite::Message::Text(text)) => {
                let value: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                // 事件（没有 id）直接忽略。
                let Some(id) = value.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                if id != target_id {
                    continue;
                }
                if let Some(err) = response_error(&value) {
                    return Err(err);
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            Ok(tungstenite::Message::Ping(payload)) => {
                let _ = ws.send(tungstenite::Message::Pong(payload));
            }
            Ok(tungstenite::Message::Close(_)) => {
                return Err(
                    "CDP WebSocket 被目标关闭（页面可能已关闭或刷新）。请重新调用 cdp_targets 确认目标。"
                        .into(),
                );
            }
            Ok(_) => {
                // Binary / Pong：忽略，继续等待真正的响应。
            }
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => {
                return Err("CDP WebSocket 连接已关闭。目标页面可能已关闭。".into());
            }
            Err(e) => return Err(format!("读取 CDP 响应失败：{e}")),
        }
    }
}

/// 在阻塞线程里执行 CDP 命令，返回命令的 `result` 对象。
async fn ws_request(
    ctx: &ToolCtx,
    ws_url: &str,
    method: &str,
    params: Value,
    runtime_enable: bool,
) -> Result<Value, String> {
    let ctx = ctx.clone();
    let ws_url = ws_url.to_string();
    let method = method.to_string();
    let joined = tokio::task::spawn_blocking(move || {
        cdp_request_blocking(
            &ws_url,
            &method,
            params,
            Duration::from_millis(CDP_TIMEOUT_MS),
            &ctx,
            runtime_enable,
        )
    })
    .await;
    match joined {
        Ok(result) => result,
        Err(e) => Err(format!("CDP 工作线程异常：{e}")),
    }
}

// ============================================================
// 工具规格
// ============================================================

fn spec_cdp_targets() -> ToolSpec {
    ToolSpec::new(
        "cdp_targets",
        "列出 Chromium 系应用（Chrome / Edge / Electron）通过 DevTools 协议暴露的页面目标（标题、url、WebSocket 调试地址）。\
需要目标进程启动时带 --remote-debugging-port（默认 9222）。\
port 不传时依次尝试 settings.tools.cdp_port、9222、9223、9224、9333。\
什么时候用：确认调试端口是否可用、拿到页面列表后给 cdp_eval / cdp_click / cdp_screenshot 选 target。",
        json!({
            "type": "object",
            "properties": {
                "port": {"type": "integer", "minimum": 1, "maximum": 65535, "description": "调试端口，默认读取设置（9222）"}
            },
            "required": []
        }),
    )
}

fn spec_cdp_eval() -> ToolSpec {
    ToolSpec::new(
        "cdp_eval",
        "在网页里执行 JavaScript 表达式并返回结果（Runtime.evaluate，returnByValue=true、awaitPromise=true）。\
可以用来读 DOM：document.title、document.querySelector('.btn')?.innerText、location.href，\
也可以把页面数据整理成 JSON 返回。返回结果按值序列化；脚本抛异常时返回错误原因。\
注意：表达式是单条 JS 表达式（IIFE 可以）；DOM 很大时先用 querySelector 缩小范围，不要返回整个 document。",
        json!({
            "type": "object",
            "properties": {
                "expression": {"type": "string", "description": "JavaScript 表达式，例如 document.title 或 (() => [...document.querySelectorAll('a')].map(a => a.href))()"},
                "target": {"type": ["string", "integer"], "description": "目标页面：不传默认第一个 page；可传下标（0 基）或 url/标题子串"},
                "port": {"type": "integer", "minimum": 1, "maximum": 65535, "description": "调试端口，默认读取设置（9222）"}
            },
            "required": ["expression"]
        }),
    )
}

fn spec_cdp_click() -> ToolSpec {
    ToolSpec::new(
        "cdp_click",
        "在网页里点击元素。selector 模式用 document.querySelector(sel) 找到元素并 .click()；\
text 模式遍历 button/a/[role=button]/菜单项/标签页等可点击元素，按可见文本（innerText/aria-label/value/title）\
大小写不敏感子串匹配后点击第一个。返回匹配数量与被点击元素的 tag/文本。\
selector 与 text 二选一，selector 优先。适合点网页按钮；桌面原生控件请用 ui_click。",
        json!({
            "type": "object",
            "properties": {
                "selector": {"type": "string", "description": "CSS selector，例如 button.submit 或 #login"},
                "text": {"type": "string", "description": "按钮/链接可见文本（与 selector 二选一）"},
                "target": {"type": ["string", "integer"], "description": "目标页面：不传默认第一个 page；可传下标或 url/标题子串"},
                "port": {"type": "integer", "minimum": 1, "maximum": 65535, "description": "调试端口"}
            },
            "required": []
        }),
    )
}

fn spec_cdp_screenshot() -> ToolSpec {
    ToolSpec::new(
        "cdp_screenshot",
        "对网页目标截图（Page.captureScreenshot，PNG），保存到 data/screenshots 目录并返回绝对路径与像素尺寸。\
full_page=true 时用 captureBeyondViewport 截取整页（长页面会很大）。\
什么时候用：需要看网页当前渲染结果时；桌面整体截图请用 screenshot 工具。",
        json!({
            "type": "object",
            "properties": {
                "target": {"type": ["string", "integer"], "description": "目标页面：不传默认第一个 page；可传下标或 url/标题子串"},
                "full_page": {"type": "boolean", "description": "是否截整页（默认 false，只截当前视口）"},
                "port": {"type": "integer", "minimum": 1, "maximum": 65535, "description": "调试端口"}
            },
            "required": []
        }),
    )
}

// ============================================================
// 工具实现
// ============================================================

struct CdpTargetsTool;
struct CdpEvalTool;
struct CdpClickTool;
struct CdpScreenshotTool;

#[async_trait]
impl Tool for CdpTargetsTool {
    fn spec(&self) -> ToolSpec {
        spec_cdp_targets()
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        let explicit = arg_u16(&args, "port");
        match discover_targets(ctx, explicit).await {
            Ok((port, targets)) => {
                let content = if targets.is_empty() {
                    format!(
                        "[ok] 127.0.0.1:{port} 可连接，但当前没有可调试的页面目标（打开一个标签页后重试）。"
                    )
                } else {
                    format_target_list(port, &targets)
                };
                ToolOutcome::ok(limit_chars(&content, 18_000))
                    .with_display(format!("CDP 端口 {port}，{} 个目标", targets.len()))
            }
            Err(e) => ToolOutcome::err(e).with_display("连接 CDP 失败"),
        }
    }
}

#[async_trait]
impl Tool for CdpEvalTool {
    fn spec(&self) -> ToolSpec {
        spec_cdp_eval()
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        let Some(expression) = arg_str(&args, "expression") else {
            return ToolOutcome::err("缺少必填参数 expression（要执行的 JavaScript 表达式）。");
        };
        let (port, target) = match open_target(ctx, &args).await {
            Ok(v) => v,
            Err(e) => return ToolOutcome::err(e),
        };
        ctx.phase("正在执行网页脚本");

        let params = json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": true,
        });
        let result = match ws_request(ctx, &target.ws_url, "Runtime.evaluate", params, true).await {
            Ok(r) => r,
            Err(e) => {
                return ToolOutcome::err(format!(
                    "{e}\n目标：{} (端口 {port})",
                    quote(&target.url)
                ));
            }
        };
        let value = match extract_eval_value(&result) {
            Ok(v) => v,
            Err(e) => {
                return ToolOutcome::err(format!(
                    "{e}\n表达式：{}\n目标：{}",
                    limit_chars(&expression, 500),
                    quote(&target.url)
                ));
            }
        };
        let text = value_to_text(&value);
        let content = format!(
            "[ok] 目标 [{}] {}（端口 {port}）\n结果类型：{}\n值：\n{}",
            target.target_type,
            quote(&target.title),
            type_name(&value),
            limit_chars(&text, 18_000)
        );
        ToolOutcome::ok(content)
            .with_display(format!("eval 完成，返回 {} 字符", text.chars().count()))
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[async_trait]
impl Tool for CdpClickTool {
    fn spec(&self) -> ToolSpec {
        spec_cdp_click()
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        let selector = arg_str(&args, "selector");
        let text = arg_str(&args, "text");
        let expression = if let Some(sel) = selector.as_deref() {
            click_selector_expression(sel)
        } else if let Some(t) = text.as_deref() {
            click_text_expression(t)
        } else {
            return ToolOutcome::err(
                "缺少定位参数：请给 selector（CSS 选择器）或 text（按钮可见文本）。",
            );
        };
        let (port, target) = match open_target(ctx, &args).await {
            Ok(v) => v,
            Err(e) => return ToolOutcome::err(e),
        };
        ctx.phase("正在点击网页元素");

        let params = json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": true,
        });
        let result = match ws_request(ctx, &target.ws_url, "Runtime.evaluate", params, true).await {
            Ok(r) => r,
            Err(e) => {
                return ToolOutcome::err(format!(
                    "{e}\n目标：{}（端口 {port}）\n提示：页面可能已跳转，重新调用 cdp_targets 后再试。",
                    quote(&target.url)
                ));
            }
        };
        let value = match extract_eval_value(&result) {
            Ok(v) => v,
            Err(e) => {
                return ToolOutcome::err(format!(
                    "{e}\n目标：{}\n表达式：{}",
                    quote(&target.url),
                    limit_chars(&expression, 500)
                ));
            }
        };
        let clicked_ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if clicked_ok {
            let tag = value.get("tag").and_then(Value::as_str).unwrap_or("");
            let label = value.get("label").and_then(Value::as_str).unwrap_or("");
            let matched = value.get("matched").and_then(Value::as_u64).unwrap_or(1);
            ToolOutcome::ok(format!(
                "[ok] 已点击目标 [{}] {} 里的元素（端口 {port}）\n方式：{}\n元素：{} {}\n匹配数：{matched}\n目标 url：{}",
                target.target_type,
                quote(&target.title),
                if selector.is_some() { format!("selector={}", quote(selector.as_deref().unwrap_or(""))) } else { format!("text={}", quote(text.as_deref().unwrap_or(""))) },
                tag,
                quote(label),
                quote(&target.url)
            ))
            .with_display(format!("已点击 {label}"))
        } else {
            let reason = value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("not_found");
            let message = value.get("message").and_then(Value::as_str).unwrap_or("");
            let matched = value.get("matched").and_then(Value::as_u64).unwrap_or(0);
            ToolOutcome::err(format!(
                "[err] 没有点击任何元素（reason={reason}，匹配数={matched}）。\n目标：{}（端口 {port}）\n{}提示：先用 cdp_eval 查 document.querySelectorAll(...) 确认选择器/文本；文本模式匹配 innerText 或 aria-label。",
                quote(&target.url),
                if message.is_empty() { String::new() } else { format!("脚本说明：{message}\n") }
            ))
            .with_display("点击目标未找到")
        }
    }
}

#[async_trait]
impl Tool for CdpScreenshotTool {
    fn spec(&self) -> ToolSpec {
        spec_cdp_screenshot()
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        let full_page = arg_bool(&args, "full_page").unwrap_or(false);
        let (port, target) = match open_target(ctx, &args).await {
            Ok(v) => v,
            Err(e) => return ToolOutcome::err(e),
        };
        ctx.phase("正在截取网页画面");

        let params = json!({
            "format": "png",
            "captureBeyondViewport": full_page,
            "fromSurface": true,
        });
        let result = match ws_request(ctx, &target.ws_url, "Page.captureScreenshot", params, false)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return ToolOutcome::err(format!(
                    "{e}\n目标：{}（端口 {port}）\n提示：页面可能已关闭；可重新调用 cdp_targets 确认。",
                    quote(&target.url)
                ));
            }
        };
        let Some(data_b64) = result.get("data").and_then(Value::as_str) else {
            return ToolOutcome::err("Page.captureScreenshot 没有返回 data 字段（协议异常）。");
        };
        let bytes = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
            Ok(b) => b,
            Err(e) => return ToolOutcome::err(format!("截图 base64 解码失败：{e}")),
        };
        if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
            return ToolOutcome::err(format!(
                "截图数据不是合法 PNG（前 16 字节：{:02X?}）。目标可能不支持截图或返回了错误页。",
                &bytes[..bytes.len().min(16)]
            ));
        }

        let paths = ctx.paths();
        if let Err(e) = std::fs::create_dir_all(&paths.screenshots) {
            return ToolOutcome::err(format!(
                "创建截图目录失败：{}（{e}）",
                paths.screenshots.display()
            ));
        }
        let file = paths
            .screenshots
            .join(format!("cdp_{}.png", neosonia_core::util::now_ms()));
        if let Err(e) = std::fs::write(&file, &bytes) {
            return ToolOutcome::err(format!("写入截图文件失败：{}（{e}）", file.display()));
        }

        let dims = {
            use image::GenericImageView as _;
            image::load_from_memory(&bytes)
                .ok()
                .map(|img| img.dimensions())
                .map(|(w, h)| format!("{w}x{h}"))
                .unwrap_or_else(|| "未知".to_string())
        };
        // `[screenshot:<绝对路径>]` 标记：core 的视觉输入（phase2）靠它把截图塞进下一轮模型请求。
        let content = format!(
            "[ok] 已保存网页截图\n[screenshot:{}]\n路径：{}\n字节：{}\n尺寸：{} 像素\n模式：{}\n目标 [{}] {}\n目标 url：{}\n端口：{port}",
            file.display(),
            file.display(),
            bytes.len(),
            dims,
            if full_page {
                "整页（captureBeyondViewport）"
            } else {
                "当前视口"
            },
            target.target_type,
            quote(&target.title),
            quote(&target.url)
        );
        ToolOutcome::ok(content).with_display(format!("网页截图已保存：{}", file.display()))
    }
}

/// 把这些工具注册进注册表（由 `neosonia_tools::register_all` / `tool_catalog` 调用）。
pub fn register(reg: &ToolRegistry) {
    reg.register(Arc::new(CdpTargetsTool));
    reg.register(Arc::new(CdpEvalTool));
    reg.register(Arc::new(CdpClickTool));
    reg.register(Arc::new(CdpScreenshotTool));
}

// ============================================================
// 单测（纯逻辑，不连浏览器）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_LIST: &str = r#"[
        {"description":"","devtoolsFrontendUrl":"/devtools/inspector.html?ws=127.0.0.1:9222/devtools/page/AAA",
         "id":"AAA","title":"NeoSonia 测试页","type":"page","url":"https://example.com/app",
         "webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/AAA"},
        {"description":"","id":"BBB","title":"后台页","type":"background_page","url":"chrome-extension://bbb",
         "webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/BBB"},
        {"description":"","id":"CCC","title":"无 ws 的浏览器目标","type":"browser","url":"http://localhost"}
    ]"#;

    #[test]
    fn parses_targets_and_skips_entries_without_ws() {
        let value: Value = serde_json::from_str(SAMPLE_LIST).unwrap();
        let targets = parse_targets(&value);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].id, "AAA");
        assert_eq!(targets[0].target_type, "page");
        assert_eq!(targets[0].url, "https://example.com/app");
        assert!(targets[0].ws_url.contains("/devtools/page/AAA"));
    }

    #[test]
    fn picks_target_by_default_index_and_substring() {
        let value: Value = serde_json::from_str(SAMPLE_LIST).unwrap();
        let targets = parse_targets(&value);
        // 默认第一个 page
        assert_eq!(pick_target_index(&targets, None), Some(0));
        // 下标选择
        assert_eq!(pick_target_index(&targets, Some("1")), Some(1));
        assert_eq!(pick_target_index(&targets, Some("9")), None);
        // url / 标题子串（大小写不敏感）
        assert_eq!(pick_target_index(&targets, Some("EVIL")), None);
        assert_eq!(pick_target_index(&targets, Some("example.com")), Some(0));
        assert_eq!(pick_target_index(&targets, Some("后台页")), Some(1));
        // 空列表
        assert_eq!(pick_target_index(&[], None), None);
    }

    #[test]
    fn eval_result_extraction() {
        let ok: Value = serde_json::from_str(
            r#"{"result":{"type":"string","value":"NeoSonia"},"exceptionDetails":null}"#,
        )
        .unwrap();
        assert_eq!(
            extract_eval_value(&ok).unwrap(),
            Value::String("NeoSonia".into())
        );

        let obj: Value =
            serde_json::from_str(r#"{"result":{"type":"object","value":{"a":1}}}"#).unwrap();
        assert_eq!(extract_eval_value(&obj).unwrap()["a"], 1);

        let err: Value = serde_json::from_str(
            r#"{"result":{"type":"object"},"exceptionDetails":{"text":"Uncaught","lineNumber":3,"exception":{"description":"ReferenceError: foo is not defined"}}}"#,
        )
        .unwrap();
        let msg = extract_eval_value(&err).unwrap_err();
        assert!(msg.contains("JavaScript 执行异常"));
        assert!(msg.contains("ReferenceError"));

        let undef: Value = serde_json::from_str(r#"{"result":{"type":"undefined"}}"#).unwrap();
        assert_eq!(
            extract_eval_value(&undef).unwrap(),
            Value::String("<undefined>".into())
        );
    }

    #[test]
    fn js_expressions_are_well_formed() {
        let expr = click_selector_expression("button.submit[data-x=\"y\"]");
        assert!(expr.contains("document.querySelector(sel)"));
        // 选择器里的双引号被 JSON 转义，不会提前结束 JS 字符串
        assert!(expr.contains(r#"button.submit[data-x=\"y\"]"#));
        assert!(expr.starts_with("(() => {"));
        assert!(expr.ends_with("})()"));

        let text_expr = click_text_expression("提交 订单");
        assert!(text_expr.contains("document.querySelectorAll(selector)"));
        assert!(text_expr.contains("提交 订单"));
        assert!(text_expr.contains("el.click()"));
    }

    #[test]
    fn value_to_text_forms() {
        assert_eq!(value_to_text(&Value::String("hi".into())), "hi");
        assert_eq!(value_to_text(&Value::Null), "null");
        assert!(value_to_text(&json!({"a":1})).contains("\"a\""));
    }

    #[test]
    fn register_exposes_all_four_tools() {
        let reg = ToolRegistry::new();
        register(&reg);
        let names = reg.names();
        for expected in ["cdp_targets", "cdp_eval", "cdp_click", "cdp_screenshot"] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing tool {expected}"
            );
        }
        assert_eq!(names.len(), 4);
        for spec in reg.specs() {
            assert_eq!(spec.parameters["type"], "object");
            assert!(!spec.description.is_empty());
        }
    }
}

// ============================================================
// 真实浏览器集成测试（需要本机开着调试端口，默认 #[ignore]）
// ============================================================

#[cfg(test)]
mod real_browser_tests {
    use std::sync::Arc;

    use neosonia_core::config::{Paths, Settings};
    use neosonia_core::model::{
        AgentEvent, AgentHost, MemoryApi, MemoryItem, MemoryStats, SubagentRequest, ToolCtx,
    };
    use neosonia_core::registry::ToolRegistry;
    use parking_lot::RwLock;
    use serde_json::json;

    use super::register;

    struct NullMemory;

    impl MemoryApi for NullMemory {
        fn remember(&self, _c: &str, _t: &[String], _i: f64, _s: &str) -> anyhow::Result<i64> {
            Ok(0)
        }
        fn recall(&self, _q: &str, _l: usize) -> anyhow::Result<Vec<MemoryItem>> {
            Ok(Vec::new())
        }
        fn recent(&self, _l: usize) -> anyhow::Result<Vec<MemoryItem>> {
            Ok(Vec::new())
        }
        fn all(&self, _k: Option<&str>, _l: usize) -> anyhow::Result<Vec<MemoryItem>> {
            Ok(Vec::new())
        }
        fn forget(&self, _id: i64) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn update_importance(&self, _id: i64, _v: f64) -> anyhow::Result<()> {
            Ok(())
        }
        fn get_block(&self, _k: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn set_block(&self, _k: &str, _v: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn blocks(&self) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
            Ok(Default::default())
        }
        fn log_conversation(&self, _r: &str, _c: &str, _t: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn stats(&self) -> anyhow::Result<MemoryStats> {
            Ok(MemoryStats::default())
        }
    }

    struct TestHost {
        settings: Arc<RwLock<Settings>>,
        paths: Paths,
        memory: Arc<dyn MemoryApi>,
    }

    impl AgentHost for TestHost {
        fn settings(&self) -> Arc<RwLock<Settings>> {
            self.settings.clone()
        }
        fn paths(&self) -> Paths {
            self.paths.clone()
        }
        fn memory(&self) -> Arc<dyn MemoryApi> {
            self.memory.clone()
        }
        fn emit(&self, _event: AgentEvent) {}
        fn spawn_subagent(&self, _req: SubagentRequest) -> anyhow::Result<String> {
            anyhow::bail!("测试宿主不支持子代理")
        }
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    fn test_ctx(port: u16) -> ToolCtx {
        let mut settings = Settings::default();
        settings.tools.cdp_enabled = true;
        settings.tools.cdp_port = port;
        let paths = Paths::from_root(std::env::temp_dir().join("neosonia-automation-test"));
        let host = Arc::new(TestHost {
            settings: Arc::new(RwLock::new(settings)),
            paths,
            memory: Arc::new(NullMemory),
        });
        ToolCtx::new(host, "turn-test", "call-test")
    }

    /// 集成测试用的调试端口：默认 9222，可用环境变量 NEOSONIA_CDP_TEST_PORT 覆盖
    /// （例如本机同时开着别的浏览器、或者想指向 headless 实例时）。
    fn test_port() -> u16 {
        std::env::var("NEOSONIA_CDP_TEST_PORT")
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .filter(|p| *p > 0)
            .unwrap_or(9222)
    }

    /// 从 cdp_targets 的输出里挑一个可执行 JS 的目标参数：
    /// 优先 http(s) 的 page（用 URL 片段做 target），否则第一个 page 的下标。
    /// 这样不依赖目标浏览器当前刚好开着一个 about:blank 标签页。
    fn pick_cdp_target_arg(targets_output: &str) -> String {
        let mut fallback: Option<String> = None;
        for line in targets_output.lines() {
            if !line.contains("[page]") {
                continue;
            }
            if fallback.is_none() {
                if let Some(pos) = line.find('#') {
                    let digits: String = line[pos + 1..]
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    if !digits.is_empty() {
                        fallback = Some(digits);
                    }
                }
            }
            if let Some(pos) = line.find("url=\"") {
                let rest = &line[pos + "url=\"".len()..];
                let end = rest.find('"').unwrap_or(rest.len());
                let url = &rest[..end];
                if url.starts_with("http://") || url.starts_with("https://") {
                    let frag: String = url.chars().take(48).collect();
                    let frag = frag.trim_end_matches(['.', '…', ' ']).to_string();
                    if !frag.is_empty() {
                        return frag;
                    }
                }
            }
        }
        fallback.unwrap_or_else(|| "about:blank".to_string())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "需要本机开着调试端口的 Edge/Chrome（默认 9222，可用 NEOSONIA_CDP_TEST_PORT 覆盖）"]
    async fn real_cdp_tools_smoke() {
        let port = test_port();
        let ctx = test_ctx(port);
        let reg = ToolRegistry::new();
        register(&reg);
        assert_eq!(reg.len(), 4);

        let out = reg
            .call(&ctx, "cdp_targets", &json!({ "port": port }))
            .await;
        println!(
            "=== cdp_targets（端口 {port}）===\nok={}\n{}",
            out.ok, out.content
        );
        assert!(out.ok, "cdp_targets 失败：{}", out.content);

        // 目标浏览器可能开着一堆扩展页/没有 about:blank，动态挑一个可执行 JS 的页面。
        let target_hint = pick_cdp_target_arg(&out.content);
        println!("[verify] CDP 测试目标 target={target_hint}");

        let eval = {
            let reg = reg.clone();
            let ctx = ctx.clone();
            let target = target_hint.clone();
            move |expr: &str| {
                let reg = reg.clone();
                let ctx = ctx.clone();
                let expr = expr.to_string();
                let target = target.clone();
                async move {
                    reg.call(
                        &ctx,
                        "cdp_eval",
                        &json!({ "expression": expr, "target": target, "port": port }),
                    )
                    .await
                }
            }
        };

        let out =
            eval("(() => { document.title = 'NeoSonia CDP 测试'; return document.title; })()")
                .await;
        println!("=== cdp_eval set title ===\nok={}\n{}", out.ok, out.content);
        assert!(out.ok, "cdp_eval 失败：{}", out.content);
        assert!(out.content.contains("NeoSonia CDP 测试"));

        let setup = eval(
            "(() => { document.querySelectorAll('#neo-test-btn').forEach((el) => el.remove()); window.__neo_clicked = 0; const b = document.createElement('button'); b.id = 'neo-test-btn'; b.textContent = '自动化测试按钮'; b.addEventListener('click', () => { window.__neo_clicked = (window.__neo_clicked || 0) + 1; }); document.body.appendChild(b); return 'ready'; })()",
        )
        .await;
        assert!(setup.ok, "创建测试按钮失败：{}", setup.content);

        let out = reg
            .call(
                &ctx,
                "cdp_click",
                &json!({ "selector": "#neo-test-btn", "target": target_hint, "port": port }),
            )
            .await;
        println!("=== cdp_click selector ===\nok={}\n{}", out.ok, out.content);
        assert!(out.ok, "cdp_click(selector) 失败：{}", out.content);

        let count = eval("window.__neo_clicked || 0").await;
        println!(
            "=== cdp_eval click count ===\nok={}\n{}",
            count.ok, count.content
        );
        assert!(count.ok);
        // 页面在多次测试间会保留状态，所以 setup 里已经把计数清 0；这里严格解析“值：”后的数字。
        let value_after_click: i64 = count
            .content
            .rsplit("值：")
            .next()
            .map(|v| v.trim().to_string())
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(-1);
        assert_eq!(value_after_click, 1, "点击未生效：{}", count.content);

        let out = reg
            .call(
                &ctx,
                "cdp_click",
                &json!({ "text": "自动化测试按钮", "target": target_hint, "port": port }),
            )
            .await;
        println!("=== cdp_click text ===\nok={}\n{}", out.ok, out.content);
        assert!(out.ok, "cdp_click(text) 失败：{}", out.content);

        let out = reg
            .call(
                &ctx,
                "cdp_screenshot",
                &json!({ "target": target_hint, "full_page": false, "port": port }),
            )
            .await;
        println!("=== cdp_screenshot ===\nok={}\n{}", out.ok, out.content);
        assert!(out.ok, "cdp_screenshot 失败：{}", out.content);
        let path_line = out
            .content
            .lines()
            .find(|l| l.starts_with("路径："))
            .expect("截图结果缺少路径行");
        let path = path_line.trim_start_matches("路径：").trim();
        let bytes = std::fs::read(path).expect("截图文件读不到");
        assert!(bytes.len() > 1000, "截图文件太小：{}", bytes.len());
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "截图不是 PNG");
        println!("截图文件：{}（{} 字节）", path, bytes.len());
    }
}
