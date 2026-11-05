//! OpenAI 兼容的聊天客户端（流式 SSE）。
//!
//! 支持任何 OpenAI 兼容端点（DeepSeek / OpenAI / Ollama / vLLM / OpenRouter ...）：
//! - `POST {base_url}/chat/completions`，`stream=true` 时按 SSE 解析；
//! - 解析 `delta.content`、`delta.reasoning_content`（DeepSeek 思考模式）、
//!   分片 `delta.tool_calls`（按 index 累加 id / name / arguments）、`usage`
//!   （含 `prompt_cache_hit_tokens` -> [`Usage::cached_tokens`]）；
//! - 非流式 [`LlmClient::chat_once`] 与 `GET {base_url}/models`。
//!
//! SSE 解析是手写的（不依赖 eventsource-stream），这样可以按任意 chunk 边界切分做单测。
//!
//! OWNER: core-llm 队友。公共签名已与 app crate 对齐，改动需同步通知 Lead。

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::config::ServerSettings;
use crate::model::{ChatMessage, FunctionCall, ToolCall, ToolSpec};

/// `base_url` 留空时的兜底地址。
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/v1";

/// 错误响应体只保留这么多字符，避免把整页 HTML 塞进错误信息。
const ERROR_BODY_CHARS: usize = 500;

/// TCP/TLS 建连超时。真正等模型的时间由 `request_timeout_secs` 控制。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// 用户取消流式输出时返回的错误标记。
#[derive(Debug)]
pub struct LlmCancelled;

impl std::fmt::Display for LlmCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "流式请求已被用户取消")
    }
}

impl std::error::Error for LlmCancelled {}

/// 判断一个 anyhow 错误是否来自"用户取消"。
pub fn is_cancelled_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<LlmCancelled>().is_some()
}

/// 是否属于"模型未响应"类可重试错误：超时/连接失败/读取流失败/HTTP 5xx/429/空流。
///
/// 鉴权失败（401/403）、参数错误（400）、内容审核等**不**重试，直接报给用户。
pub fn is_transient_error(error: &anyhow::Error) -> bool {
    if let Some(reqwest_error) = error.downcast_ref::<reqwest::Error>() {
        if reqwest_error.is_timeout() || reqwest_error.is_connect() || reqwest_error.is_body() {
            return true;
        }
    }
    let text = error.to_string().to_ascii_lowercase();
    if text.contains("http 5") || text.contains("http 429") {
        return true;
    }
    text.contains("请求模型服务失败")
        || text.contains("请求超时")
        || text.contains("读取流式响应失败")
        || text.contains("读取模型响应失败")
        || text.contains("模型返回了空的流式响应")
}

/// 流式增量。
#[derive(Clone, Debug)]
pub enum LlmDelta {
    Reasoning(String),
    Text(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// DeepSeek 的 `prompt_cache_hit_tokens` / OpenAI 的 `prompt_tokens_details.cached_tokens`。
    pub cached_tokens: u32,
}

/// 一次请求的输入。
#[derive(Clone, Debug, Default)]
pub struct LlmRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    /// 覆盖配置里的 max_tokens（None = 用配置）。
    pub max_tokens: Option<u32>,
    /// 覆盖温度。
    pub temperature: Option<f32>,
}

/// 一次请求的输出（流结束后汇总）。
#[derive(Clone, Debug, Default)]
pub struct LlmReply {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: String,
}

/// 聊天客户端。轻量，可 clone（内部共享 reqwest::Client）。
#[derive(Clone)]
pub struct LlmClient {
    settings: ServerSettings,
    http: reqwest::Client,
    api_key: String,
}

impl LlmClient {
    pub fn new(settings: ServerSettings, api_key: String) -> Self {
        let idle = Duration::from_secs(settings.request_timeout_secs.clamp(10, 3600));
        let mut builder = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            // read_timeout 是"多久没收到任何字节"的超时，而不是整个请求的总时长，
            // 这样思考模型长时间推理、长流式输出都不会被误杀。
            .read_timeout(idle);
        if is_local_endpoint(&settings.base_url) {
            // 本地 Ollama / vLLM / LM Studio 不应走系统代理。
            builder = builder.no_proxy();
        }
        let http = builder.build().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "构建 HTTP 客户端失败，退回默认客户端");
            reqwest::Client::new()
        });
        Self {
            settings,
            http,
            api_key: api_key.trim().to_string(),
        }
    }

    /// 用最新配置重建（UI 改了服务器/密钥后调用）。
    pub fn rebuild(&self, settings: ServerSettings, api_key: String) -> Self {
        Self::new(settings, api_key)
    }

    pub fn model(&self) -> &str {
        &self.settings.model
    }

    /// 是否已经拿到 API key（本地端点通常不需要）。
    pub fn has_api_key(&self) -> bool {
        !self.api_key.is_empty()
    }

    /// 规范化后的 API 根地址（去掉末尾 `/` 以及误填的 `/chat/completions`）。
    pub fn api_base(&self) -> String {
        normalize_base(&self.settings.base_url)
    }

    /// 流式请求。每次增量通过 `on_delta` 回调抛出。
    pub async fn chat_stream(
        &self,
        req: LlmRequest,
        on_delta: &mut (dyn FnMut(LlmDelta) + Send),
    ) -> anyhow::Result<LlmReply> {
        // 不需要取消的旧调用路径：用一个永远为 false 的标志。
        let never = std::sync::atomic::AtomicBool::new(false);
        self.chat_stream_cancellable(req, on_delta, &never).await
    }

    /// 可取消的流式请求：每收到一个增量/每个 chunk 前检查 `cancel`，置位后尽快返回
    /// [`LlmCancelled`]（agent 会把它当作"用户取消"处理）。
    pub async fn chat_stream_cancellable(
        &self,
        req: LlmRequest,
        on_delta: &mut (dyn FnMut(LlmDelta) + Send),
        cancel: &std::sync::atomic::AtomicBool,
    ) -> anyhow::Result<LlmReply> {
        use std::sync::atomic::Ordering;
        if cancel.load(Ordering::SeqCst) {
            return Err(anyhow::Error::new(LlmCancelled));
        }
        let body = self.build_body(&req, true);
        let rb = self
            .request(reqwest::Method::POST, "chat/completions")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        let base = self.api_base();
        let resp = send_checked(rb, &base).await?;

        // 有些兼容端点忽略 stream=true 直接返回完整 JSON，这里做兜底。
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.contains("application/json") || content_type.contains("text/json") {
            let raw = resp
                .text()
                .await
                .map_err(|e| anyhow!("读取模型响应失败: {e}"))?;
            if cancel.load(Ordering::SeqCst) {
                return Err(anyhow::Error::new(LlmCancelled));
            }
            let value: Value = serde_json::from_str(&raw).map_err(|e| {
                anyhow!(
                    "模型返回的不是合法 JSON（{e}）: {}",
                    head(&raw, ERROR_BODY_CHARS)
                )
            })?;
            let reply = parse_reply_json(&value)?;
            emit_whole_reply(&reply, on_delta);
            return Ok(reply);
        }

        let mut stream = resp.bytes_stream();
        let mut parser = SseParser::new();
        let mut acc = StreamAccumulator::default();
        let mut raw = String::new();
        while let Some(chunk) = stream.next().await {
            if cancel.load(Ordering::SeqCst) {
                return Err(anyhow::Error::new(LlmCancelled));
            }
            let bytes = chunk.map_err(|e| anyhow!("读取流式响应失败: {e}"))?;
            if raw.len() < 4096 {
                raw.push_str(&String::from_utf8_lossy(&bytes));
            }
            let mut done = false;
            for event in parser.push(&bytes) {
                if event.data.trim() == "[DONE]" {
                    done = true;
                    break;
                }
                acc.ingest(&event.data, on_delta)?;
                // 每处理一个 SSE 事件（通常是一次 delta）就检查取消。
                if cancel.load(Ordering::SeqCst) {
                    return Err(anyhow::Error::new(LlmCancelled));
                }
            }
            if done {
                break;
            }
        }

        // 没收到任何 SSE 事件时，可能对方其实返回了完整 JSON 或错误 JSON。
        if !acc.saw_data {
            if let Ok(value) = serde_json::from_str::<Value>(raw.trim()) {
                let reply = parse_reply_json(&value)?;
                emit_whole_reply(&reply, on_delta);
                return Ok(reply);
            }
        }
        acc.finish()
    }

    /// 非流式请求（子代理/摘要用）。
    pub async fn chat_once(&self, req: LlmRequest) -> anyhow::Result<LlmReply> {
        let body = self.build_body(&req, false);
        let timeout = Duration::from_secs(self.settings.request_timeout_secs.clamp(10, 3600));
        let rb = self
            .request(reqwest::Method::POST, "chat/completions")
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(timeout)
            .json(&body);
        let base = self.api_base();
        let resp = send_checked(rb, &base).await?;
        let raw = resp
            .text()
            .await
            .map_err(|e| anyhow!("读取模型响应失败: {e}"))?;
        let value: Value = serde_json::from_str(&raw).map_err(|e| {
            anyhow!(
                "模型返回的不是合法 JSON（{e}）: {}",
                head(&raw, ERROR_BODY_CHARS)
            )
        })?;
        parse_reply_json(&value)
    }

    /// 测试连通性并返回可用模型列表。
    pub async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        let timeout = Duration::from_secs(self.settings.request_timeout_secs.clamp(10, 120));
        let rb = self
            .request(reqwest::Method::GET, "models")
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(timeout);
        let base = self.api_base();
        let resp = send_checked(rb, &base).await?;
        let raw = resp
            .text()
            .await
            .map_err(|e| anyhow!("读取模型列表失败: {e}"))?;
        let value: Value = serde_json::from_str(&raw).map_err(|e| {
            anyhow!(
                "模型列表不是合法 JSON（{e}）: {}",
                head(&raw, ERROR_BODY_CHARS)
            )
        })?;
        let models = parse_models(&value);
        if models.is_empty() {
            bail!(
                "端点 {} 没有返回任何模型（响应片段: {}）。请检查 base_url 是否指向 OpenAI 兼容的 /v1 地址",
                self.api_base(),
                head(&raw, ERROR_BODY_CHARS)
            );
        }
        Ok(models)
    }

    /// 组装请求体。公开给单测（不给外部 crate 用）。
    fn build_body(&self, req: &LlmRequest, stream: bool) -> Value {
        let mut body = json!({
            "model": self.settings.model,
            "messages": req.messages.iter().map(ChatMessage::to_wire).collect::<Vec<_>>(),
            "stream": stream,
        });

        if !req.tools.is_empty() {
            body["tools"] = json!(req.tools.iter().map(ToolSpec::to_wire).collect::<Vec<_>>());
            body["tool_choice"] = json!("auto");
        }

        let temperature = req.temperature.unwrap_or(self.settings.temperature);
        if temperature.is_finite() {
            body["temperature"] = json!(temperature);
        }

        let max_tokens = req.max_tokens.unwrap_or(self.settings.max_tokens);
        if max_tokens > 0 {
            body["max_tokens"] = json!(max_tokens);
        }

        if stream {
            // OpenAI / DeepSeek 都支持：让最后一个 chunk 带 usage。
            body["stream_options"] = json!({ "include_usage": true });
        }

        if self.settings.thinking {
            // DeepSeek 思考模式专有扩展；其它兼容端点通常会忽略未知字段。
            body["thinking"] = json!({ "type": "enabled" });
        }

        let effort = self.settings.reasoning_effort.trim();
        if !effort.is_empty() {
            body["reasoning_effort"] = json!(effort);
        }

        body
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut rb = self
            .http
            .request(method, format!("{}/{}", self.api_base(), path))
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        if !self.api_key.is_empty() {
            rb = rb.bearer_auth(&self.api_key);
        }
        // 自定义头后写，允许覆盖 Authorization。
        for (k, v) in &self.settings.headers {
            let key = k.trim();
            let value = v.trim();
            if key.is_empty() || value.is_empty() {
                continue;
            }
            rb = rb.header(key, value);
        }
        rb
    }
}

/// 非流式回复按整段文本抛给回调（流式接口的降级路径用）。
fn emit_whole_reply(reply: &LlmReply, on_delta: &mut (dyn FnMut(LlmDelta) + Send)) {
    if !reply.reasoning.is_empty() {
        on_delta(LlmDelta::Reasoning(reply.reasoning.clone()));
    }
    if !reply.text.is_empty() {
        on_delta(LlmDelta::Text(reply.text.clone()));
    }
}

/// 规范化 `base_url`：去空、去尾斜杠、去掉误填的 `/chat/completions`。
fn normalize_base(raw: &str) -> String {
    let mut base = raw.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        return DEFAULT_BASE_URL.to_string();
    }
    for suffix in ["/chat/completions", "/completions"] {
        if base.ends_with(suffix) {
            base.truncate(base.len() - suffix.len());
        }
    }
    base.trim_end_matches('/').to_string()
}

/// 判断是否本地端点（本地端点不代理）。
fn is_local_endpoint(raw: &str) -> bool {
    let candidate = {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            DEFAULT_BASE_URL
        } else {
            trimmed
        }
    };
    if let Ok(url) = url::Url::parse(candidate) {
        if let Some(host) = url.host_str() {
            let h = host.to_ascii_lowercase();
            return h == "localhost"
                || h == "::1"
                || h == "[::1]"
                || h == "0.0.0.0"
                || h.starts_with("127.");
        }
        return false;
    }
    let lower = candidate.to_ascii_lowercase();
    lower.contains("://localhost") || lower.contains("://127.0.0.1") || lower.contains("://[::1]")
}

async fn send_checked(
    rb: reqwest::RequestBuilder,
    base: &str,
) -> anyhow::Result<reqwest::Response> {
    let resp = rb.send().await.map_err(|e| {
        let hint = if e.is_timeout() {
            "；请求超时，请调大 server.request_timeout_secs 或检查网络"
        } else if e.is_connect() {
            "；连接失败，请检查 server.base_url、网络与代理设置"
        } else {
            ""
        };
        anyhow!("请求模型服务失败（{base}）: {e}{hint}")
    })?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "模型服务返回 HTTP {}（{}）: {}",
            status.as_u16(),
            base,
            head(&body, ERROR_BODY_CHARS)
        );
    }
    Ok(resp)
}

fn head(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

fn as_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_i64().map(|n| n.max(0) as u64))
}

/// `usage` 对象 -> [`Usage`]，兼容 DeepSeek 与 OpenAI 两种缓存字段。
fn parse_usage(v: &Value) -> Usage {
    let get = |key: &str| -> u32 {
        v.get(key)
            .and_then(as_u64)
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32
    };
    let cached = v
        .get("prompt_cache_hit_tokens")
        .and_then(as_u64)
        .or_else(|| {
            v.pointer("/prompt_tokens_details/cached_tokens")
                .and_then(as_u64)
        })
        .or_else(|| v.get("cached_tokens").and_then(as_u64))
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32;
    Usage {
        prompt_tokens: get("prompt_tokens"),
        completion_tokens: get("completion_tokens"),
        cached_tokens: cached,
    }
}

/// 把 JSON 值转成字符串（部分端点会把 arguments 直接给成对象）。
fn json_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 文本字段可能是字符串，也可能是多模态 parts 数组。
fn text_fragment(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    if let Some(parts) = v.as_array() {
        let mut out = String::new();
        for part in parts {
            if let Some(t) = part.get("text").and_then(|x| x.as_str()) {
                out.push_str(t);
            } else if let Some(t) = part.get("content").and_then(|x| x.as_str()) {
                out.push_str(t);
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

/// 从错误响应里抠出人类可读的信息。
fn error_text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(msg) = v.get("message").and_then(|x| x.as_str()) {
        return msg.to_string();
    }
    if let Some(code) = v.get("code") {
        if let Some(s) = code.as_str() {
            if !s.is_empty() {
                return format!(
                    "{} ({s})",
                    v.get("message").map(json_text).unwrap_or_default()
                );
            }
        }
    }
    if let Some(inner) = v.get("error") {
        if inner.is_object() || inner.is_string() {
            return error_text(inner);
        }
    }
    v.to_string()
}

/// 解析一条完整的非流式响应。
fn parse_reply_json(v: &Value) -> anyhow::Result<LlmReply> {
    if let Some(err) = v.get("error") {
        if !err.is_null() {
            bail!("模型服务返回错误: {}", error_text(err));
        }
    }
    // 局部 null 值：让 `unwrap_or(&null_value)` 的借用活到函数结束，避免临时值问题。
    let null_value = Value::Null;
    let choice = v.pointer("/choices/0").ok_or_else(|| {
        anyhow!(
            "模型响应缺少 choices[0]: {}",
            head(&v.to_string(), ERROR_BODY_CHARS)
        )
    })?;
    let message = choice.get("message").unwrap_or(&null_value);
    let text = message
        .get("content")
        .and_then(text_fragment)
        .unwrap_or_default();
    let reasoning = message
        .get("reasoning_content")
        .and_then(text_fragment)
        .or_else(|| message.get("reasoning").and_then(text_fragment))
        .unwrap_or_default();

    let mut tool_calls = Vec::new();
    if let Some(arr) = message.get("tool_calls").and_then(|x| x.as_array()) {
        for (index, tc) in arr.iter().enumerate() {
            let id = tc
                .get("id")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("call_{}_{}", index, crate::util::short_id()));
            let function = tc.get("function").unwrap_or(&null_value);
            let name = function
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = function.get("arguments").map(json_text).unwrap_or_default();
            tool_calls.push(ToolCall {
                id,
                call_type: "function".to_string(),
                function: FunctionCall { name, arguments },
            });
        }
    }

    Ok(LlmReply {
        text,
        reasoning,
        tool_calls,
        usage: v.get("usage").map(parse_usage).unwrap_or_default(),
        finish_reason: choice
            .get("finish_reason")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// 解析 `/models` 响应，兼容 OpenAI `data` 与 Ollama `models` 两种形状。
fn parse_models(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(arr) = v.get("data").and_then(|x| x.as_array()) {
        for item in arr {
            if let Some(id) = item.get("id").and_then(|x| x.as_str()) {
                push_model(&mut out, id);
            } else if let Some(id) = item.get("name").and_then(|x| x.as_str()) {
                push_model(&mut out, id);
            }
        }
    }
    if out.is_empty() {
        if let Some(arr) = v.get("models").and_then(|x| x.as_array()) {
            for item in arr {
                if let Some(id) = item.get("id").and_then(|x| x.as_str()) {
                    push_model(&mut out, id);
                } else if let Some(name) = item.get("name").and_then(|x| x.as_str()) {
                    push_model(&mut out, name);
                } else if let Some(name) = item.as_str() {
                    push_model(&mut out, name);
                }
            }
        }
    }
    if out.is_empty() {
        if let Some(arr) = v.as_array() {
            for item in arr {
                if let Some(name) = item.as_str() {
                    push_model(&mut out, name);
                } else if let Some(name) = item.get("id").and_then(|x| x.as_str()) {
                    push_model(&mut out, name);
                }
            }
        }
    }
    out
}

fn push_model(out: &mut Vec<String>, name: &str) {
    let name = name.trim();
    if name.is_empty() || out.iter().any(|existing| existing == name) {
        return;
    }
    out.push(name.to_string());
}

// ============================================================
// SSE 解析
// ============================================================

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SseEvent {
    /// `event:` 字段（为空 = 默认 message 事件）。
    #[allow(dead_code)]
    event: String,
    /// 多行 `data:` 用 `\n` 拼接后的内容。
    data: String,
}

/// 增量式 SSE 解析器：输入字节块，输出完整事件。
///
/// 自己维护字节缓冲与"半行"状态，能处理任意 chunk 切割（包括把一个 UTF-8 字符切成两半，
/// 以及 `data:` 行与空行落在不同 chunk 里的情况）。
#[derive(Default)]
struct SseParser {
    buf: Vec<u8>,
    event_name: String,
    data_lines: Vec<String>,
}

impl SseParser {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut consumed = 0usize;

        let mut idx = 0usize;
        while idx < self.buf.len() {
            if self.buf[idx] != b'\n' {
                idx += 1;
                continue;
            }
            let raw = &self.buf[consumed..idx];
            let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
            let line = String::from_utf8_lossy(raw).to_string();
            parse_sse_line(&line, &mut self.event_name, &mut self.data_lines, &mut out);
            consumed = idx + 1;
            idx += 1;
        }
        if consumed > 0 {
            self.buf.drain(..consumed);
        }
        out
    }
}

fn parse_sse_line(
    line: &str,
    event_name: &mut String,
    data_lines: &mut Vec<String>,
    out: &mut Vec<SseEvent>,
) {
    if line.is_empty() {
        if data_lines.is_empty() {
            event_name.clear();
            return;
        }
        out.push(SseEvent {
            event: std::mem::take(event_name),
            data: data_lines.join("\n"),
        });
        data_lines.clear();
        return;
    }
    if line.starts_with(':') {
        // 注释 / keep-alive
        return;
    }
    let (field, value) = match line.split_once(':') {
        Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
        None => (line, ""),
    };
    match field {
        "data" => data_lines.push(value.to_string()),
        "event" => *event_name = value.to_string(),
        _ => {}
    }
}

// ============================================================
// 流式增量累加
// ============================================================

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl PartialToolCall {
    fn is_empty(&self) -> bool {
        self.id.is_empty() && self.name.is_empty() && self.arguments.is_empty()
    }
}

#[derive(Default)]
struct StreamAccumulator {
    text: String,
    reasoning: String,
    tools: BTreeMap<i64, PartialToolCall>,
    usage: Usage,
    finish_reason: String,
    saw_data: bool,
    bad_json: Option<String>,
}

impl StreamAccumulator {
    /// 吃掉一条 `data:` 的 JSON。
    fn ingest(
        &mut self,
        data: &str,
        on_delta: &mut (dyn FnMut(LlmDelta) + Send),
    ) -> anyhow::Result<()> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(());
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                if self.bad_json.is_none() {
                    self.bad_json = Some(format!("{e}: {}", head(data, ERROR_BODY_CHARS)));
                }
                return Ok(());
            }
        };
        self.saw_data = true;

        if let Some(err) = value.get("error") {
            if !err.is_null() {
                bail!("模型服务在流中返回错误: {}", error_text(err));
            }
        }

        if let Some(usage) = value.get("usage") {
            if usage.is_object() {
                self.usage = parse_usage(usage);
            }
        }

        let Some(choices) = value.get("choices").and_then(|x| x.as_array()) else {
            return Ok(());
        };
        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                if let Some(reasoning) = delta
                    .get("reasoning_content")
                    .and_then(text_fragment)
                    .or_else(|| delta.get("reasoning").and_then(text_fragment))
                {
                    if !reasoning.is_empty() {
                        self.reasoning.push_str(&reasoning);
                        on_delta(LlmDelta::Reasoning(reasoning));
                    }
                }
                if let Some(text) = delta.get("content").and_then(text_fragment) {
                    if !text.is_empty() {
                        self.text.push_str(&text);
                        on_delta(LlmDelta::Text(text));
                    }
                }
                if let Some(calls) = delta.get("tool_calls").and_then(|x| x.as_array()) {
                    for (position, call) in calls.iter().enumerate() {
                        let index = call
                            .get("index")
                            .and_then(|x| x.as_i64())
                            .unwrap_or(position as i64);
                        let partial = self.tools.entry(index).or_default();
                        if let Some(id) = call.get("id").and_then(|x| x.as_str()) {
                            partial.id.push_str(id);
                        }
                        if let Some(function) = call.get("function") {
                            if let Some(name) = function.get("name").and_then(|x| x.as_str()) {
                                partial.name.push_str(name);
                            }
                            if let Some(arguments) = function.get("arguments") {
                                partial.arguments.push_str(&json_text(arguments));
                            }
                        }
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(|x| x.as_str()) {
                if !reason.is_empty() {
                    self.finish_reason = reason.to_string();
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> anyhow::Result<LlmReply> {
        if !self.saw_data {
            if let Some(bad) = &self.bad_json {
                bail!("模型流式响应无法解析（{bad}）");
            }
            bail!(
                "模型返回了空的流式响应（没有收到任何 data 事件）。\
                 请确认 server.base_url 指向 OpenAI 兼容的 /v1 端点，并检查 API key 与网络"
            );
        }
        let mut tool_calls = Vec::new();
        for (index, partial) in self.tools {
            if partial.is_empty() {
                continue;
            }
            let id = if partial.id.is_empty() {
                format!("call_{index}_{}", crate::util::short_id())
            } else {
                partial.id
            };
            tool_calls.push(ToolCall {
                id,
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: partial.name,
                    arguments: partial.arguments,
                },
            });
        }
        Ok(LlmReply {
            text: self.text,
            reasoning: self.reasoning,
            tool_calls,
            usage: self.usage,
            finish_reason: self.finish_reason,
        })
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;

    fn collect_deltas(reply: &mut LlmReply, deltas: &[LlmDelta]) -> (String, String) {
        let mut text = String::new();
        let mut reasoning = String::new();
        for d in deltas {
            match d {
                LlmDelta::Text(t) => text.push_str(t),
                LlmDelta::Reasoning(r) => reasoning.push_str(r),
            }
        }
        reply.text = text.clone();
        reply.reasoning = reasoning.clone();
        (text, reasoning)
    }

    #[test]
    fn sse_parser_handles_arbitrary_chunk_splits() {
        let raw = "event: message\ndata: {\"a\":1}\n\ndata: {\"b\":2}\n\ndata: [DONE]\n\n";
        // 逐字节喂入，模拟最坏情况的网络切分。
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for b in raw.as_bytes() {
            events.extend(parser.push(&[*b]));
        }
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[0].event, "message");
        assert_eq!(events[1].data, "{\"b\":2}");
        assert_eq!(events[2].data, "[DONE]");
    }

    #[test]
    fn sse_parser_multiline_crlf_and_comments() {
        let mut parser = SseParser::new();
        let events = parser.push(b": keep-alive\r\ndata: line1\r\ndata: line2\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");

        // 多字节 UTF-8 被切断时必须能正确拼回。
        let mut parser = SseParser::new();
        let bytes = "data: 中文\n\n".as_bytes();
        let mut events = parser.push(&bytes[..7]);
        events.extend(parser.push(&bytes[7..]));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "中文");
    }

    fn ingest_json(acc: &mut StreamAccumulator, body: &str) -> Vec<LlmDelta> {
        let mut deltas: Vec<LlmDelta> = Vec::new();
        let mut cb = |d: LlmDelta| deltas.push(d);
        acc.ingest(body, &mut cb).unwrap();
        deltas
    }

    #[test]
    fn accumulator_assembles_reasoning_text_and_tool_calls() {
        let mut acc = StreamAccumulator::default();
        // DeepSeek 思考模式 + 分片 tool_calls（index 0 和 1）。
        ingest_json(
            &mut acc,
            r#"{"choices":[{"delta":{"role":"assistant","reasoning_content":"先看目录。"}}]}"#,
        );
        ingest_json(&mut acc, r#"{"choices":[{"delta":{"content":"好的，"}}]}"#);
        ingest_json(
            &mut acc,
            r#"{"choices":[{"delta":{"content":"我来查。"}}]}"#,
        );
        ingest_json(
            &mut acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read_","arguments":"{\"pa"}}]}}]}"#,
        );
        ingest_json(
            &mut acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"file","arguments":"th\":\"a.txt\"}"}}]}}]}"#,
        );
        ingest_json(
            &mut acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"list_dir","arguments":"{}"}}]}}]}"#,
        );
        ingest_json(
            &mut acc,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":120,"completion_tokens":30,"prompt_cache_hit_tokens":64}}"#,
        );

        let reply = acc.finish().unwrap();
        assert_eq!(reply.reasoning, "先看目录。");
        assert_eq!(reply.text, "好的，我来查。");
        assert_eq!(reply.finish_reason, "tool_calls");
        assert_eq!(reply.usage.prompt_tokens, 120);
        assert_eq!(reply.usage.completion_tokens, 30);
        assert_eq!(reply.usage.cached_tokens, 64);
        assert_eq!(reply.tool_calls.len(), 2);
        assert_eq!(reply.tool_calls[0].id, "call_a");
        assert_eq!(reply.tool_calls[0].function.name, "read_file");
        assert_eq!(
            reply.tool_calls[0].function.arguments,
            "{\"path\":\"a.txt\"}"
        );
        assert_eq!(reply.tool_calls[1].function.name, "list_dir");
    }

    #[test]
    fn accumulator_reports_stream_errors() {
        let mut acc = StreamAccumulator::default();
        let mut cb = |_d: LlmDelta| {};
        let err = acc
            .ingest(r#"{"error":{"message":"invalid api key"}}"#, &mut cb)
            .unwrap_err();
        assert!(err.to_string().contains("invalid api key"));
    }

    #[test]
    fn accumulator_empty_stream_is_an_error() {
        let err = StreamAccumulator::default().finish().unwrap_err();
        assert!(err.to_string().contains("空的流式响应"));
    }

    #[test]
    fn usage_maps_openai_cached_tokens() {
        let usage = parse_usage(&json!({
            "prompt_tokens": 10,
            "completion_tokens": 2,
            "prompt_tokens_details": { "cached_tokens": 7 }
        }));
        assert_eq!(usage.cached_tokens, 7);
    }

    fn client() -> LlmClient {
        let mut settings = ServerSettings::default();
        settings.base_url = "https://api.deepseek.com/v1/".into();
        settings.model = "deepseek-chat".into();
        settings.thinking = true;
        settings.reasoning_effort = "high".into();
        LlmClient::new(settings, "sk-test".into())
    }

    #[test]
    fn transient_error_classification() {
        // 网络/超时/5xx/429/空流 → 可重试。
        assert!(is_transient_error(&anyhow::anyhow!(
            "请求模型服务失败（https://api.deepseek.com）: error sending request；请求超时"
        )));
        assert!(is_transient_error(&anyhow::anyhow!(
            "模型服务返回 HTTP 503（https://api.deepseek.com）: upstream error"
        )));
        assert!(is_transient_error(&anyhow::anyhow!(
            "模型服务返回 HTTP 429（https://api.deepseek.com）: rate limited"
        )));
        assert!(is_transient_error(&anyhow::anyhow!(
            "模型返回了空的流式响应（没有收到任何 data 事件）。"
        )));
        // 鉴权/参数/内容错误 → 不重试。
        assert!(!is_transient_error(&anyhow::anyhow!(
            "模型服务返回 HTTP 401（https://api.deepseek.com）: authentication fails"
        )));
        assert!(!is_transient_error(&anyhow::anyhow!(
            "模型服务返回 HTTP 400（https://api.deepseek.com）: invalid request"
        )));
        assert!(!is_transient_error(&anyhow::anyhow!(
            "模型返回的不是合法 JSON（expected value）"
        )));
    }

    #[test]
    fn base_url_normalization() {
        assert_eq!(
            normalize_base("https://api.deepseek.com/v1/"),
            "https://api.deepseek.com/v1"
        );
        assert_eq!(
            normalize_base("https://api.deepseek.com/v1/chat/completions"),
            "https://api.deepseek.com/v1"
        );
        assert_eq!(normalize_base("  "), DEFAULT_BASE_URL);
        assert_eq!(client().api_base(), "https://api.deepseek.com/v1");
    }

    #[test]
    fn request_body_carries_thinking_tools_and_usage_flag() {
        let llm = client();
        let req = LlmRequest {
            messages: vec![
                ChatMessage::system("sys"),
                ChatMessage::user("hi"),
                ChatMessage::assistant_with_calls(
                    None,
                    Some("think".into()),
                    vec![ToolCall {
                        id: "c1".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "read_file".into(),
                            arguments: "{}".into(),
                        },
                    }],
                ),
            ],
            tools: vec![ToolSpec::new(
                "read_file",
                "读文件",
                json!({"type":"object","properties":{"path":{"type":"string"}}}),
            )],
            max_tokens: Some(100),
            temperature: Some(0.2),
        };
        let body = llm.build_body(&req, true);
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["messages"][2]["reasoning_content"], "think");
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "c1");
        assert_eq!(body["messages"][0]["role"], "system");

        let non_stream = llm.build_body(&req, false);
        assert!(non_stream.get("stream_options").is_none());
        assert_eq!(non_stream["stream"], false);
    }

    #[test]
    fn parse_models_handles_both_shapes() {
        let openai =
            json!({"object":"list","data":[{"id":"deepseek-chat"},{"id":"deepseek-reasoner"}]});
        assert_eq!(
            parse_models(&openai),
            vec!["deepseek-chat".to_string(), "deepseek-reasoner".to_string()]
        );
        let ollama = json!({"models":[{"name":"qwen2.5:7b"},{"id":"llama3"}]});
        assert_eq!(
            parse_models(&ollama),
            vec!["qwen2.5:7b".to_string(), "llama3".to_string()]
        );
        assert!(parse_models(&json!({})).is_empty());
    }

    #[test]
    fn non_stream_reply_parsing() {
        let value = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "结果如下",
                    "reasoning_content": "先想一下",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "run_command", "arguments": "{\"command\":\"dir\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 9, "completion_tokens": 3, "prompt_cache_hit_tokens": 4 }
        });
        let reply = parse_reply_json(&value).unwrap();
        assert_eq!(reply.text, "结果如下");
        assert_eq!(reply.reasoning, "先想一下");
        assert_eq!(reply.usage.cached_tokens, 4);
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].function.name, "run_command");
        assert_eq!(reply.finish_reason, "tool_calls");
    }

    #[test]
    fn image_message_wire_shape_in_request_body() {
        let llm = client();
        let image =
            crate::model::ImagePart::png("aGVsbG8=".into(), 10, 20, Some("shot.png".into()));
        let req = LlmRequest {
            messages: vec![ChatMessage::user_with_images("请看这张图", vec![image])],
            ..Default::default()
        };
        let body = llm.build_body(&req, true);
        let content = &body["messages"][0]["content"];
        assert!(content.is_array(), "带图片的消息 content 应是数组");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "请看这张图");
        assert_eq!(content[1]["type"], "image_url");
        let url = content[1]["image_url"]["url"].as_str().unwrap_or_default();
        assert!(url.starts_with("data:image/png;base64,aGVsbG8="));
        // 不带图片的消息仍是普通字符串 content。
        let plain = LlmRequest {
            messages: vec![ChatMessage::user("纯文本")],
            ..Default::default()
        };
        let body = llm.build_body(&plain, false);
        assert!(body["messages"][0]["content"].is_string());
    }

    #[test]
    fn collect_deltas_helper_works() {
        let mut reply = LlmReply::default();
        let (text, reasoning) = collect_deltas(
            &mut reply,
            &[
                LlmDelta::Reasoning("想".into()),
                LlmDelta::Text("答".into()),
            ],
        );
        assert_eq!(text, "答");
        assert_eq!(reasoning, "想");
        assert_eq!(reply.text, "答");
        assert_eq!(Role::Assistant.as_str(), "assistant");
    }
}
