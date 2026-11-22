//! DeepSeek 官方「文件解析 / OCR / 文档问答」工具（仅 DeepSeek 官方端点可用）。
//!
//! 文档：
//! - 文件解析：<https://api-docs.deepseek.com/zh-cn/guides/document_parsing>
//! - OCR：<https://api-docs.deepseek.com/zh-cn/guides/ocr>
//!
//! 三个工具：
//! - `deepseek_file_upload`：上传本地文件到 Files API（`POST {root}/files`），
//!   multipart 里 `purpose` 必须排在 `file` 之前；返回 `file_id`。
//! - `deepseek_ocr`：调用 OCR API（`POST {root}/v1/tools/ocr`），按页返回文本。
//! - `deepseek_document_qa`：在 Chat Completions 里用 `{"type":"file","file_id":...}`
//!   引用已上传文件，让 `deepseek-flash` 基于文档回答。
//!
//! 注册条件：base_url 为空或 host 为 `api.deepseek.com`，且能解析到非空 API Key
//! （`settings.server.api_key` 支持 `env:NAME` / `${NAME}` 形式）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use neosonia_core::config::Settings;
use neosonia_core::model::{ToolCtx, ToolOutcome};
use neosonia_core::registry::ToolRegistry;
use neosonia_core::util::{human_bytes, truncate_chars};
use serde_json::{json, Value};

use crate::{arg_str, arg_str_req, arg_u64};

/// DeepSeek 官方 API 根地址（不含 `/v1`）。
const DEFAULT_API_ROOT: &str = "https://api.deepseek.com";
/// 单个文档最大 100 MiB。
const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
/// 单页 OCR 文本在工具输出里的上限（字符）。
const MAX_PAGE_CHARS: usize = 4000;
/// 一次请求最多引用的文档数（官方限制 20）。
const MAX_FILE_IDS: usize = 20;
/// 上传/OCR 请求超时（含上传+解析）。
const UPLOAD_OCR_TIMEOUT_SECS: u64 = 180;
/// 文档问答请求超时。
const QA_TIMEOUT_SECS: u64 = 120;

// ============================================================
// 端点 / URL / API Key
// ============================================================

/// 是否是 DeepSeek 官方端点（空 base_url 视为官方默认）。
pub(crate) fn is_deepseek_endpoint(base_url: &str) -> bool {
    let raw = base_url.trim();
    if raw.is_empty() {
        return true;
    }
    match url::Url::parse(raw) {
        Ok(parsed) => parsed
            .host_str()
            .map(|host| host.eq_ignore_ascii_case("api.deepseek.com"))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// 去掉末尾 `/` 以及 `/v1`、`/chat/completions` 后缀，得到 API 根地址。
fn api_root(base_url: &str) -> String {
    let mut root = base_url.trim().trim_end_matches('/').to_string();
    loop {
        let before = root.clone();
        for suffix in ["/chat/completions", "/v1"] {
            if let Some(stripped) = root.strip_suffix(suffix) {
                root = stripped.trim_end_matches('/').to_string();
            }
        }
        if root == before {
            break;
        }
    }
    if root.is_empty() {
        DEFAULT_API_ROOT.to_string()
    } else {
        root
    }
}

/// 拼出完整 API URL；`path` 以 `/` 开头。
fn api_url(base_url: &str, path: &str) -> String {
    format!("{}{}", api_root(base_url), path)
}

/// Chat Completions 地址（显式补 `/v1`，避免 base_url 已带 `/v1` 时出现 `/v1/v1`）。
fn chat_completions_url(base_url: &str) -> String {
    format!("{}/v1/chat/completions", api_root(base_url))
}

/// 展开 `env:NAME` / `${NAME}` 形式的密钥；普通字符串原样返回。
fn expand_env_secret(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let name = if let Some(name) = raw.strip_prefix("env:") {
        Some(name.trim().to_string())
    } else if let Some(name) = raw
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
    {
        Some(name.trim().to_string())
    } else {
        None
    };
    if let Some(name) = name {
        if name.is_empty() {
            return None;
        }
        return std::env::var(&name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
    }
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

/// 实际使用的 API Key：优先当前生效配置，其次环境变量；空/`env:` 未解析成功视为没有。
fn resolve_api_key(settings: &Settings) -> Option<String> {
    let server = settings.effective_server();
    let raw = server.api_key.trim();
    if !raw.is_empty() {
        return expand_env_secret(raw);
    }
    for name in ["NEOSONIA_API_KEY", "DEEPSEEK_API_KEY"] {
        if let Ok(value) = std::env::var(name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

// ============================================================
// multipart 构造（reqwest 未开 multipart feature，手写）
// ============================================================

fn sanitize_multipart_filename(filename: &str) -> String {
    filename
        .chars()
        .map(|ch| match ch {
            '"' => '\'',
            '\r' | '\n' => ' ',
            _ => ch,
        })
        .collect()
}

fn content_type_for(filename: &str) -> &'static str {
    let extension = Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xls" => "application/vnd.ms-excel",
        "epub" => "application/epub+zip",
        _ => "application/octet-stream",
    }
}

/// 手写 multipart body：先写 `text_fields`（purpose 等），最后写文件字段。
fn build_multipart_body(
    boundary: &str,
    text_fields: &[(String, String)],
    filename: &str,
    file_bytes: &[u8],
) -> Vec<u8> {
    let content_type = content_type_for(filename);
    let filename = sanitize_multipart_filename(filename);
    let mut body = Vec::with_capacity(file_bytes.len() + 1024);
    for (name, value) in text_fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn new_boundary() -> String {
    format!("----NeoSoniaBoundary{}", neosonia_core::util::short_id())
}

// ============================================================
// 参数校验
// ============================================================

fn parse_upload_purpose(raw: Option<&str>) -> Result<String, String> {
    match raw.map(|value| value.trim().to_ascii_lowercase()) {
        None => Ok("text_recognition".to_string()),
        Some(value) if value.is_empty() => Ok("text_recognition".to_string()),
        Some(value) if value == "text_recognition" => Ok("text_recognition".to_string()),
        Some(value) if value == "rich_layout_recognition" => {
            Ok("rich_layout_recognition".to_string())
        }
        Some(value) if value == "user_data" => Ok("user_data".to_string()),
        Some(other) => Err(format!(
            "purpose 只能是 text_recognition / rich_layout_recognition / user_data，收到 {other:?}。"
        )),
    }
}

fn parse_ocr_purpose(raw: Option<&str>) -> Result<String, String> {
    match raw.map(|value| value.trim().to_ascii_lowercase()) {
        None => Ok("text_recognition".to_string()),
        Some(value) if value.is_empty() || value == "text_recognition" => {
            Ok("text_recognition".to_string())
        }
        Some(value) if value == "rich_layout_recognition" => {
            Ok("rich_layout_recognition".to_string())
        }
        Some(other) => Err(format!(
            "OCR 的 purpose 只能是 text_recognition 或 rich_layout_recognition，收到 {other:?}。"
        )),
    }
}

fn parse_expires_seconds(raw: Option<u64>) -> Result<Option<u64>, String> {
    match raw {
        None => Ok(None),
        Some(seconds) if (3600..=2_592_000).contains(&seconds) => Ok(Some(seconds)),
        Some(seconds) => Err(format!(
            "expires_seconds 取值范围是 3600-2592000（1 小时到 30 天），收到 {seconds}。"
        )),
    }
}

fn parse_max_pages(args: &Value) -> u64 {
    arg_u64(args, "max_pages", 20).clamp(1, 200)
}

/// 收集 file_id/file_ids（合并、去重、最多 20 个）。
fn collect_file_ids(args: &Value) -> Result<Vec<String>, String> {
    let mut ids: Vec<String> = Vec::new();
    let mut push = |raw: &Value| -> Result<(), String> {
        let Some(text) = raw.as_str() else {
            return Err("file_id 必须是字符串。".to_string());
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err("file_id 不能为空字符串。".to_string());
        }
        if !ids.iter().any(|existing| existing == &text) {
            ids.push(text);
        }
        Ok(())
    };
    match args.get("file_id") {
        None | Some(Value::Null) => {}
        Some(value) => push(value)?,
    }
    match args.get("file_ids") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for item in items {
                push(item)?;
            }
        }
        Some(_) => return Err("file_ids 必须是字符串数组。".to_string()),
    }
    if ids.is_empty() {
        return Err(
            "缺少文档：请传 file_id（单文件）或 file_ids（数组），先用 deepseek_file_upload 上传。"
                .to_string(),
        );
    }
    if ids.len() > MAX_FILE_IDS {
        return Err(format!(
            "一次最多引用 {MAX_FILE_IDS} 个文档（收到 {} 个）；请分批提问。",
            ids.len()
        ));
    }
    Ok(ids)
}

// ============================================================
// 响应解析（纯函数，便于单测）
// ============================================================

#[derive(Debug, Clone, PartialEq, Eq)]
struct UploadInfo {
    id: String,
    bytes: u64,
    filename: String,
    purpose: String,
    expires_at: Option<i64>,
}

fn error_message_of(payload: &Value) -> Option<String> {
    payload
        .get("error")
        .and_then(|error| {
            error
                .get("message")
                .and_then(|message| message.as_str())
                .or_else(|| error.as_str())
        })
        .map(|message| message.to_string())
}

fn http_error_message(operation: &str, status: u16, body: &str) -> String {
    let body = body.trim();
    format!(
        "{operation}失败：HTTP {status}；响应：{}",
        truncate_chars(body, 500)
    )
}

fn parse_upload_response(body: &str) -> Result<UploadInfo, String> {
    let payload: Value = serde_json::from_str(body.trim()).map_err(|error| {
        format!(
            "DeepSeek Files API 返回无法解析的响应：{error}；前 500 字符：{}",
            truncate_chars(body.trim(), 500)
        )
    })?;
    if let Some(message) = error_message_of(&payload) {
        return Err(format!("DeepSeek 文件上传失败：{message}"));
    }
    let id = payload
        .get("id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "DeepSeek 文件上传响应缺少 id。".to_string())?
        .to_string();
    let bytes = payload
        .get("bytes")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let filename = payload
        .get("filename")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let purpose = payload
        .get("purpose")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let expires_at = payload
        .get("expires_at")
        .and_then(|value| value.as_i64());
    Ok(UploadInfo {
        id,
        bytes,
        filename,
        purpose,
        expires_at,
    })
}

fn render_usage(usage: Option<&Value>) -> String {
    let Some(usage) = usage else {
        return "（未返回）".to_string();
    };
    let mut parts: Vec<String> = Vec::new();
    for key in ["input_tokens", "output_tokens", "total_tokens"] {
        if let Some(value) = usage.get(key).and_then(|value| value.as_u64()) {
            parts.push(format!("{key}={value}"));
        }
    }
    if parts.is_empty() {
        truncate_chars(&usage.to_string(), 300)
    } else {
        parts.join("，")
    }
}

fn truncate_page_text(text: &str) -> String {
    let chars = text.chars().count();
    if chars <= MAX_PAGE_CHARS {
        text.to_string()
    } else {
        format!(
            "{}\n…（本页 {chars} 字符，超过 {MAX_PAGE_CHARS} 已截断；需要完整文本请拆分文件或用 OCR API 单独处理该页）",
            truncate_chars(text, MAX_PAGE_CHARS)
        )
    }
}

fn parse_ocr_response(body: &str, max_pages: usize) -> Result<String, String> {
    // 识别期间服务端持续发换行保活；前导换行属于 JSON 空白，trim 后再解析。
    let trimmed = body.trim();
    let payload: Value = serde_json::from_str(trimmed).map_err(|error| {
        format!(
            "DeepSeek OCR 返回无法解析的响应：{error}；前 500 字符：{}",
            truncate_chars(trimmed, 500)
        )
    })?;
    if let Some(message) = error_message_of(&payload) {
        return Err(format!("DeepSeek OCR 失败：{message}"));
    }
    let page_count = payload
        .get("page_count")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let pages = payload
        .get("pages")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let max_pages = max_pages.clamp(1, 200);
    let purpose = payload
        .get("purpose")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let mut out = String::from("[ok] DeepSeek OCR 完成\n");
    if !purpose.is_empty() {
        out.push_str(&format!("purpose：{purpose}\n"));
    }
    out.push_str(&format!("页数：{page_count}\n"));
    out.push_str(&format!(
        "usage：{}\n",
        render_usage(payload.get("usage"))
    ));
    let mut shown = 0usize;
    for (position, page) in pages.iter().take(max_pages).enumerate() {
        let index = page
            .get("index")
            .and_then(|value| value.as_u64())
            .unwrap_or(position as u64);
        let finish_reason = page
            .get("finish_reason")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let text = page.get("text").and_then(|value| value.as_str()).unwrap_or("");
        out.push_str(&format!(
            "\n--- 第 {} 页（index={index}，finish_reason={finish_reason}）---\n",
            shown + 1
        ));
        if finish_reason == "length" {
            out.push_str("（警告：该页输出达到上限，文本不完整）\n");
        }
        out.push_str(&truncate_page_text(text));
        out.push('\n');
        shown += 1;
    }
    if pages.is_empty() {
        out.push_str("\n（响应里没有 pages 数组；请检查文件/服务状态）\n");
    } else if page_count as usize > shown {
        out.push_str(&format!(
            "\n（还有 {} 页未显示；可调大 max_pages，最多 200）\n",
            page_count as usize - shown
        ));
    }
    Ok(out)
}

fn extract_chat_answer(payload: &Value) -> Result<String, String> {
    let content = payload
        .pointer("/choices/0/message/content")
        .ok_or_else(|| "DeepSeek 问答响应缺少 choices[0].message.content。".to_string())?;
    if let Some(text) = content.as_str() {
        return Ok(text.to_string());
    }
    if let Some(items) = content.as_array() {
        let mut parts: Vec<String> = Vec::new();
        for item in items {
            if let Some(text) = item.get("text").and_then(|value| value.as_str()) {
                parts.push(text.to_string());
            }
        }
        if !parts.is_empty() {
            return Ok(parts.join("\n"));
        }
    }
    Err("DeepSeek 问答响应的 content 格式无法识别。".to_string())
}

fn parse_chat_response(body: &str) -> Result<(String, String), String> {
    let payload: Value = serde_json::from_str(body.trim()).map_err(|error| {
        format!(
            "DeepSeek Chat 返回无法解析的响应：{error}；前 500 字符：{}",
            truncate_chars(body.trim(), 500)
        )
    })?;
    if let Some(message) = error_message_of(&payload) {
        return Err(format!("DeepSeek 文档问答失败：{message}"));
    }
    let answer = extract_chat_answer(&payload)?;
    let usage = render_usage(payload.get("usage"));
    Ok((answer, usage))
}

// ============================================================
// HTTP
// ============================================================

fn build_client(settings: &Settings, timeout_secs: u64) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(20))
        .user_agent("neosonia-tools deepseek-files");
    let proxy = settings.web.proxy.trim();
    if !proxy.is_empty() {
        let proxy = reqwest::Proxy::all(proxy)
            .map_err(|error| format!("代理地址非法（{proxy}）：{error}"))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|error| format!("创建 HTTP 客户端失败：{error}"))
}

fn require_official(settings: &Settings) -> Result<String, ToolOutcome> {
    if !is_deepseek_endpoint(&settings.server.base_url) {
        return Err(ToolOutcome::err(format!(
            "当前模型端点不是 DeepSeek 官方端点（base_url={}）。\
             这三个工具只在 base_url 为空或 api.deepseek.com 时可用。",
            settings.server.base_url
        )));
    }
    match resolve_api_key(settings) {
        Some(api_key) => Ok(api_key),
        None => Err(ToolOutcome::err(
            "没有可用的 DeepSeek API Key：请在设置里填写 server.api_key（支持 env:DEEPSEEK_API_KEY），\
             或设置环境变量 DEEPSEEK_API_KEY。"
                .to_string(),
        )),
    }
}

async fn read_file_for_upload(ctx: &ToolCtx, raw_path: &str) -> Result<PathBuf, ToolOutcome> {
    let path = crate::resolve_read_path(ctx, raw_path)?;
    let metadata = std::fs::metadata(&path).map_err(|error| {
        ToolOutcome::err(format!(
            "读取文件信息失败（{}）：{}",
            path.display(),
            crate::io_error_hint(&error)
        ))
    })?;
    if !metadata.is_file() {
        return Err(ToolOutcome::err(format!(
            "不是普通文件：{}。请传具体文件（PDF/Word/PPT/图片/文本等）。",
            path.display()
        )));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(ToolOutcome::err(format!(
            "文件太大：{}（{}），DeepSeek 单个文档上限 100 MiB。",
            human_bytes(metadata.len()),
            path.display()
        )));
    }
    Ok(path)
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "upload.bin".to_string())
}

// ============================================================
// 工具 1：上传并解析文件
// ============================================================

async fn deepseek_file_upload(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let settings = ctx.settings();
    let api_key = match require_official(&settings) {
        Ok(key) => key,
        Err(outcome) => return outcome,
    };
    let raw_path = match arg_str_req(&args, "path") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let purpose = match parse_upload_purpose(arg_str(&args, "purpose").as_deref()) {
        Ok(purpose) => purpose,
        Err(error) => return ToolOutcome::err(error),
    };
    let expires_seconds = match parse_expires_seconds(
        args.get("expires_seconds")
            .filter(|value| !value.is_null())
            .map(|_| arg_u64(&args, "expires_seconds", 0)),
    ) {
        Ok(value) => value,
        Err(error) => return ToolOutcome::err(error),
    };
    let path = match read_file_for_upload(ctx, &raw_path).await {
        Ok(path) => path,
        Err(outcome) => return outcome,
    };
    let filename = file_name_of(&path);
    if filename.len() > 512 {
        return ToolOutcome::err(format!(
            "文件名超过 512 字节（{} 字节），请先改名再上传。",
            filename.len()
        ));
    }
    ctx.phase(format!("正在上传并解析：{filename}"));
    let path_for_read = path.clone();
    let read_result = tokio::task::spawn_blocking(move || std::fs::read(&path_for_read)).await;
    let bytes = match read_result {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            return ToolOutcome::err(format!(
                "读取文件失败（{}）：{}",
                path.display(),
                crate::io_error_hint(&error)
            ))
        }
        Err(error) => return ToolOutcome::err(format!("读取文件任务异常：{error}")),
    };

    let mut fields: Vec<(String, String)> = vec![("purpose".to_string(), purpose.clone())];
    if let Some(seconds) = expires_seconds {
        fields.push(("expires_after[anchor]".to_string(), "created_at".to_string()));
        fields.push(("expires_after[seconds]".to_string(), seconds.to_string()));
    }
    let boundary = new_boundary();
    let body = build_multipart_body(&boundary, &fields, &filename, &bytes);
    let url = api_url(&settings.server.base_url, "/files");
    let client = match build_client(&settings, UPLOAD_OCR_TIMEOUT_SECS) {
        Ok(client) => client,
        Err(error) => return ToolOutcome::err(error),
    };
    let response = client
        .post(&url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {api_key}"),
        )
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return ToolOutcome::err(format!(
                "上传请求失败（{url}）：{error}。请检查网络/代理与超时（上传+解析最多 180 秒）。"
            ))
        }
    };
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return ToolOutcome::err(http_error_message("DeepSeek 文件上传", status, &text));
    }
    let info = match parse_upload_response(&text) {
        Ok(info) => info,
        Err(error) => return ToolOutcome::err(error),
    };
    let expires_text = match info.expires_at {
        Some(timestamp) => chrono::DateTime::from_timestamp(timestamp, 0)
            .map(|time| {
                time.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| timestamp.to_string()),
        None => "永久有效（未设置 expires_after）".to_string(),
    };
    ToolOutcome::ok(format!(
        "[ok] 文件已上传并解析完成\nfile_id：{}\n文件名：{}\n大小：{}\n用途：{}\n有效期：{}\n\n\
         接下来可用 deepseek_document_qa(file_id=\"{}\", question=\"…\") 基于该文档提问。",
        info.id,
        if info.filename.is_empty() { filename.clone() } else { info.filename.clone() },
        if info.bytes > 0 {
            human_bytes(info.bytes)
        } else {
            human_bytes(bytes.len() as u64)
        },
        if info.purpose.is_empty() { purpose.clone() } else { info.purpose.clone() },
        expires_text,
        info.id
    ))
    .with_display(format!("上传完成 {}", truncate_chars(&info.id, 24)))
}

// ============================================================
// 工具 2：OCR
// ============================================================

async fn deepseek_ocr(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let settings = ctx.settings();
    let api_key = match require_official(&settings) {
        Ok(key) => key,
        Err(outcome) => return outcome,
    };
    let raw_path = match arg_str_req(&args, "path") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let purpose = match parse_ocr_purpose(arg_str(&args, "purpose").as_deref()) {
        Ok(purpose) => purpose,
        Err(error) => return ToolOutcome::err(error),
    };
    let max_pages = parse_max_pages(&args) as usize;
    let path = match read_file_for_upload(ctx, &raw_path).await {
        Ok(path) => path,
        Err(outcome) => return outcome,
    };
    let filename = file_name_of(&path);
    ctx.phase(format!("正在 OCR：{filename}"));
    let path_for_read = path.clone();
    let read_result = tokio::task::spawn_blocking(move || std::fs::read(&path_for_read)).await;
    let bytes = match read_result {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            return ToolOutcome::err(format!(
                "读取文件失败（{}）：{}",
                path.display(),
                crate::io_error_hint(&error)
            ))
        }
        Err(error) => return ToolOutcome::err(format!("读取文件任务异常：{error}")),
    };
    // 官方要求：purpose 必须在 file 之前。
    let fields: Vec<(String, String)> = vec![("purpose".to_string(), purpose.clone())];
    let boundary = new_boundary();
    let body = build_multipart_body(&boundary, &fields, &filename, &bytes);
    let url = api_url(&settings.server.base_url, "/v1/tools/ocr");
    let client = match build_client(&settings, UPLOAD_OCR_TIMEOUT_SECS) {
        Ok(client) => client,
        Err(error) => return ToolOutcome::err(error),
    };
    let response = client
        .post(&url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {api_key}"),
        )
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return ToolOutcome::err(format!(
                "OCR 请求失败（{url}）：{error}。请检查网络/代理；OCR 总时长上限 180 秒。"
            ))
        }
    };
    let status = response.status().as_u16();
    let text = match response.text().await {
        Ok(text) => text,
        Err(error) => return ToolOutcome::err(format!("读取 OCR 响应失败：{error}")),
    };
    if !(200..300).contains(&status) {
        return ToolOutcome::err(http_error_message("DeepSeek OCR", status, &text));
    }
    match parse_ocr_response(&text, max_pages) {
        Ok(rendered) => ToolOutcome::ok(rendered)
            .with_display(format!("OCR 完成（{} 页上限）", max_pages)),
        Err(error) => ToolOutcome::err(error),
    }
}

// ============================================================
// 工具 3：文档问答
// ============================================================

async fn deepseek_document_qa(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let settings = ctx.settings();
    let api_key = match require_official(&settings) {
        Ok(key) => key,
        Err(outcome) => return outcome,
    };
    let question = match arg_str_req(&args, "question") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let file_ids = match collect_file_ids(&args) {
        Ok(ids) => ids,
        Err(error) => return ToolOutcome::err(error),
    };
    let model = arg_str(&args, "model")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "deepseek-flash".to_string());
    let mut content: Vec<Value> = Vec::with_capacity(file_ids.len() + 1);
    content.push(json!({"type": "text", "text": question}));
    for file_id in &file_ids {
        content.push(json!({"type": "file", "file_id": file_id}));
    }
    let request_body = json!({
        "model": model,
        "messages": [{"role": "user", "content": content}],
        "stream": false
    });
    let url = chat_completions_url(&settings.server.base_url);
    ctx.phase(format!("正在基于 {} 个文档提问…", file_ids.len()));
    let client = match build_client(&settings, QA_TIMEOUT_SECS) {
        Ok(client) => client,
        Err(error) => return ToolOutcome::err(error),
    };
    let response = client
        .post(&url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {api_key}"),
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&request_body)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return ToolOutcome::err(format!(
                "文档问答请求失败（{url}）：{error}。请检查网络/超时（最多 120 秒）。"
            ))
        }
    };
    let status = response.status().as_u16();
    let text = match response.text().await {
        Ok(text) => text,
        Err(error) => return ToolOutcome::err(format!("读取文档问答响应失败：{error}")),
    };
    if !(200..300).contains(&status) {
        return ToolOutcome::err(http_error_message("DeepSeek 文档问答", status, &text));
    }
    match parse_chat_response(&text) {
        Ok((answer, usage)) => ToolOutcome::ok(format!(
            "[ok] 文档问答完成\n模型：{model}\n引用文档：{} 个（{}）\nusage：{usage}\n\n回答：\n{}",
            file_ids.len(),
            file_ids
                .iter()
                .map(|id| truncate_chars(id, 40))
                .collect::<Vec<_>>()
                .join(", "),
            answer
        ))
        .with_display("文档问答完成".to_string()),
        Err(error) => ToolOutcome::err(error),
    }
}

// ============================================================
// 注册
// ============================================================

/// 无条件注册三个工具（`tool_catalog` 用，便于文档/测试看到完整列表）。
pub fn register(reg: &ToolRegistry) {
    reg.register(Arc::new(DeepseekFileUpload));
    reg.register(Arc::new(DeepseekOcr));
    reg.register(Arc::new(DeepseekDocumentQa));
}

/// 只有 DeepSeek 官方端点 + 有可用 API Key 时才注册（返回是否注册）。
pub fn register_if_available(reg: &ToolRegistry, settings: &Settings) -> bool {
    if !is_deepseek_endpoint(&settings.server.base_url) {
        return false;
    }
    if resolve_api_key(settings).is_none() {
        return false;
    }
    register(reg);
    true
}

// ============================================================
// 工具声明
// ============================================================

tool_impl!(
    DeepseekFileUpload,
    "deepseek_file_upload",
    "把本地文件上传到 DeepSeek Files API 并完成解析，返回 `file_id`；之后用 deepseek_document_qa 基于文档提问。\n\
     支持 PDF、Word（DOC/DOCX）、PowerPoint（PPT/PPTX）、图片（PNG/JPEG/WebP/GIF/BMP/TIFF）等；单文件上限 100 MiB。\n\
     purpose：text_recognition（默认，只提取文字）/ rich_layout_recognition（文字+图，适合图表报告）/ user_data（纯文本/表格/电子书/图片，不计解析费）。\n\
     上传时文件在上传过程中完成解析，解析成功才返回 file_id（最长 180 秒，注意超时）。\n\
     仅当模型端点是 DeepSeek 官方 api.deepseek.com 且配置了 API Key 时可用。",
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "本地文件路径（工作目录内相对路径或绝对路径）"},
            "purpose": {"type": "string", "enum": ["text_recognition", "rich_layout_recognition", "user_data"], "description": "解析方式，默认 text_recognition"},
            "expires_seconds": {"type": "integer", "description": "文件有效期秒数（可选，3600-2592000 = 1 小时到 30 天；不传=永久）"}
        },
        "required": ["path"]
    }),
    deepseek_file_upload
);

tool_impl!(
    DeepseekOcr,
    "deepseek_ocr",
    "调用 DeepSeek OCR API 识别文件里的文字，按页返回文本（不经过对话模型、不保存到 Files API）。\n\
     支持 PDF、Word、PowerPoint、图片；单文件上限 100 MiB、最多 1000 页、总时长 180 秒。\n\
     purpose：text_recognition（默认）/ rich_layout_recognition（逐页识别，保留 <｜ref｜>/<｜box｜> 等定位标记）。\n\
     一次最多展示 max_pages 页（默认 20，最多 200），每页超过 4000 字符会截断并注明。\n\
     仅当模型端点是 DeepSeek 官方 api.deepseek.com 且配置了 API Key 时可用。",
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "本地文件路径"},
            "purpose": {"type": "string", "enum": ["text_recognition", "rich_layout_recognition"], "description": "识别方式，默认 text_recognition"},
            "max_pages": {"type": "integer", "description": "最多展示多少页，默认 20，范围 1-200"}
        },
        "required": ["path"]
    }),
    deepseek_ocr
);

tool_impl!(
    DeepseekDocumentQa,
    "deepseek_document_qa",
    "基于已上传的 DeepSeek 文档提问（带 file_id 的对话，模型 deepseek-flash 默认）。\n\
     先调用 deepseek_file_upload 得到 file_id；一次最多引用 20 个文档（file_id 或 file_ids）。\n\
     回答会附 usage；需要页码/图表定位时先 deepseek_ocr（rich_layout_recognition）再提问。\n\
     仅当模型端点是 DeepSeek 官方 api.deepseek.com 且配置了 API Key 时可用。",
    json!({
        "type": "object",
        "properties": {
            "question": {"type": "string", "description": "要问文档的问题"},
            "file_id": {"type": "string", "description": "单个文档的 file_id（deepseek_file_upload 返回）"},
            "file_ids": {"type": "array", "items": {"type": "string"}, "description": "多个文档的 file_id 数组（最多 20 个）"},
            "model": {"type": "string", "description": "模型名，默认 deepseek-flash"}
        },
        "required": ["question"]
    }),
    deepseek_document_qa
);

// ============================================================
// 测试（纯函数，不联网）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_settings(base_url: &str, api_key: &str) -> Settings {
        let mut settings = Settings::default();
        settings.server.base_url = base_url.to_string();
        settings.server.api_key = api_key.to_string();
        settings
    }

    #[test]
    fn endpoint_detection() {
        assert!(is_deepseek_endpoint(""));
        assert!(is_deepseek_endpoint("   "));
        assert!(is_deepseek_endpoint("https://api.deepseek.com"));
        assert!(is_deepseek_endpoint("https://api.deepseek.com/"));
        assert!(is_deepseek_endpoint("https://api.deepseek.com/v1"));
        assert!(is_deepseek_endpoint("https://API.DeepSeek.com/v1/"));
        assert!(!is_deepseek_endpoint("https://api.openai.com/v1"));
        assert!(!is_deepseek_endpoint("https://api.openai.com"));
        assert!(!is_deepseek_endpoint("http://localhost:11434/v1"));
        assert!(!is_deepseek_endpoint("https://ollama.com/v1"));
        assert!(!is_deepseek_endpoint("https://proxy.example.com/v1"));
        assert!(!is_deepseek_endpoint("not a url"));
    }

    #[test]
    fn url_joining_avoids_double_v1() {
        let root_cases = [
            "https://api.deepseek.com",
            "https://api.deepseek.com/",
            "https://api.deepseek.com/v1",
            "https://api.deepseek.com/v1/",
            "https://api.deepseek.com/v1/chat/completions",
        ];
        for base in root_cases {
            assert_eq!(api_root(base), "https://api.deepseek.com", "base={base}");
            assert_eq!(
                api_url(base, "/files"),
                "https://api.deepseek.com/files",
                "base={base}"
            );
            assert_eq!(
                api_url(base, "/v1/tools/ocr"),
                "https://api.deepseek.com/v1/tools/ocr",
                "base={base}"
            );
            let chat = chat_completions_url(base);
            assert_eq!(chat, "https://api.deepseek.com/v1/chat/completions", "base={base}");
            assert!(!chat.contains("/v1/v1"), "base={base} chat={chat}");
        }
        // 空 base_url 用官方默认
        assert_eq!(api_root(""), "https://api.deepseek.com");
        assert_eq!(api_url("", "/files"), "https://api.deepseek.com/files");
        assert_eq!(
            chat_completions_url(""),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn multipart_body_order_and_binary() {
        let boundary = "TestBoundary123";
        let fields = vec![
            ("purpose".to_string(), "text_recognition".to_string()),
            ("expires_after[anchor]".to_string(), "created_at".to_string()),
            ("expires_after[seconds]".to_string(), "3600".to_string()),
        ];
        // 故意包含非 UTF-8 字节和 \r\n，确认二进制原样保留
        let file_bytes: Vec<u8> = vec![0x00, 0x01, 0xFF, b'\r', b'\n', b'h', b'i', 0x80];
        let body = build_multipart_body(boundary, &fields, "report.pdf", &file_bytes);
        let text_like = String::from_utf8_lossy(&body).to_string();
        // 字段顺序：purpose 必须在 file 之前
        let purpose_pos = text_like.find("name=\"purpose\"").expect("应有 purpose 字段");
        let expires_pos = text_like
            .find("name=\"expires_after[anchor]\"")
            .expect("应有 expires_after[anchor]");
        let file_pos = text_like
            .find("name=\"file\"; filename=\"report.pdf\"")
            .expect("应有 file 字段");
        assert!(purpose_pos < file_pos, "purpose 必须在 file 之前");
        assert!(expires_pos < file_pos);
        // filename + content-type
        assert!(text_like.contains("filename=\"report.pdf\""));
        assert!(text_like.contains("Content-Type: application/pdf"));
        // body 以 boundary 开头、以 --boundary-- 结尾
        assert!(text_like.starts_with(&format!("--{boundary}\r\n")));
        assert!(text_like.ends_with(&format!("\r\n--{boundary}--\r\n")));
        // 二进制原样在 body 中间
        let marker = b"\r\nContent-Type: application/pdf\r\n\r\n";
        let start = body
            .windows(marker.len())
            .position(|window| window == marker)
            .expect("应能找到文件头结束位置")
            + marker.len();
        assert_eq!(&body[start..start + file_bytes.len()], file_bytes.as_slice());
        // 文件名里的引号/换行被清洗，避免 header 注入
        let evil = build_multipart_body(boundary, &[], "a\"b\r\nc.pdf", b"x");
        let evil_text = String::from_utf8_lossy(&evil).to_string();
        assert!(evil_text.contains("filename=\"a'b  c.pdf\""), "{evil_text}");
    }

    #[test]
    fn content_type_mapping() {
        assert_eq!(content_type_for("a.PDF"), "application/pdf");
        assert_eq!(content_type_for("a.docx"), "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
        assert_eq!(content_type_for("a.pptx"), "application/vnd.openxmlformats-officedocument.presentationml.presentation");
        assert_eq!(content_type_for("a.png"), "image/png");
        assert_eq!(content_type_for("a.jpeg"), "image/jpeg");
        assert_eq!(content_type_for("a.tiff"), "image/tiff");
        assert_eq!(content_type_for("a.txt"), "text/plain");
        assert_eq!(content_type_for("a.unknown"), "application/octet-stream");
        assert_eq!(content_type_for("no_extension"), "application/octet-stream");
    }

    #[test]
    fn purpose_and_expiry_validation() {
        assert_eq!(parse_upload_purpose(None).unwrap(), "text_recognition");
        assert_eq!(
            parse_upload_purpose(Some("rich_layout_recognition")).unwrap(),
            "rich_layout_recognition"
        );
        assert_eq!(parse_upload_purpose(Some("USER_DATA")).unwrap(), "user_data");
        assert!(parse_upload_purpose(Some("bogus")).is_err());
        assert_eq!(parse_ocr_purpose(None).unwrap(), "text_recognition");
        assert_eq!(
            parse_ocr_purpose(Some("rich_layout_recognition")).unwrap(),
            "rich_layout_recognition"
        );
        assert!(parse_ocr_purpose(Some("user_data")).is_err(), "OCR 不支持 user_data");
        assert_eq!(parse_expires_seconds(None).unwrap(), None);
        assert_eq!(parse_expires_seconds(Some(3600)).unwrap(), Some(3600));
        assert_eq!(parse_expires_seconds(Some(2_592_000)).unwrap(), Some(2_592_000));
        assert!(parse_expires_seconds(Some(3599)).is_err());
        assert!(parse_expires_seconds(Some(2_592_001)).is_err());
        // max_pages 默认/clamp
        assert_eq!(parse_max_pages(&json!({})), 20);
        assert_eq!(parse_max_pages(&json!({"max_pages": 0})), 1);
        assert_eq!(parse_max_pages(&json!({"max_pages": 999})), 200);
        assert_eq!(parse_max_pages(&json!({"max_pages": 7})), 7);
    }

    #[test]
    fn api_key_resolution() {
        // 普通字符串
        let settings = test_settings("https://api.deepseek.com", "  sk-literal-123  ");
        assert_eq!(resolve_api_key(&settings).as_deref(), Some("sk-literal-123"));
        // env:NAME（用唯一变量名避免并行测试冲突）
        let var = format!("NEOSONIA_TEST_DS_KEY_{}", std::process::id());
        unsafe {
            std::env::set_var(&var, "  sk-from-env  ");
        }
        let settings = test_settings("https://api.deepseek.com", &format!("env:{var}"));
        assert_eq!(resolve_api_key(&settings).as_deref(), Some("sk-from-env"));
        // ${NAME}
        let settings = test_settings("https://api.deepseek.com", &format!("${{{var}}}"));
        assert_eq!(resolve_api_key(&settings).as_deref(), Some("sk-from-env"));
        unsafe {
            std::env::remove_var(&var);
        }
        // env 变量不存在 → None（不应把 "env:xxx" 当成 key 发出去）
        let settings = test_settings(
            "https://api.deepseek.com",
            &format!("env:NEOSONIA_TEST_MISSING_{}", std::process::id()),
        );
        assert!(resolve_api_key(&settings).is_none());
        // expand_env_secret 空 url 形式
        assert!(expand_env_secret("   ").is_none());
        assert_eq!(expand_env_secret("plain").as_deref(), Some("plain"));
        assert!(expand_env_secret("env:").is_none());
        assert!(expand_env_secret("${}").is_none());
    }

    #[test]
    fn upload_response_parsing() {
        let body = r#"{"id":"file-api-abc123","object":"file","bytes":1048576,"created_at":1762790000,"filename":"report.pdf","purpose":"text_recognition"}"#;
        let info = parse_upload_response(body).expect("应能解析");
        assert_eq!(info.id, "file-api-abc123");
        assert_eq!(info.bytes, 1_048_576);
        assert_eq!(info.filename, "report.pdf");
        assert_eq!(info.purpose, "text_recognition");
        assert!(info.expires_at.is_none());
        // 带 expires_at
        let info = parse_upload_response(
            r#"{"id":"file-api-x","bytes":1,"filename":"a.png","purpose":"user_data","expires_at":1762793600}"#,
        )
        .unwrap();
        assert_eq!(info.expires_at, Some(1762793600));
        // 错误对象
        let error = parse_upload_response(r#"{"error":{"message":"file is too large","type":"invalid_request_error"}}"#)
            .expect_err("应返回错误");
        assert!(error.contains("file is too large"), "{error}");
        // 非法 JSON / 缺 id
        assert!(parse_upload_response("not json").is_err());
        assert!(parse_upload_response(r#"{"bytes":1}"#).is_err());
    }

    #[test]
    fn ocr_response_parsing_trim_and_errors() {
        // 服务端保活会发前导换行；trim 后再解析
        let body = "\n\n\n{\"id\":\"ocr-1\",\"object\":\"ocr.result\",\"page_count\":2,\"purpose\":\"text_recognition\",\"pages\":[{\"index\":0,\"text\":\"第一页 <｜ref｜>标题<｜/ref｜>\",\"finish_reason\":\"stop\"},{\"index\":1,\"text\":\"第二页\",\"finish_reason\":\"length\"}],\"usage\":{\"input_tokens\":10,\"output_tokens\":20,\"total_tokens\":30}}\n";
        let rendered = parse_ocr_response(body, 20).expect("应能解析");
        println!("{rendered}");
        assert!(rendered.contains("页数：2"), "{rendered}");
        assert!(rendered.contains("input_tokens=10"), "{rendered}");
        assert!(rendered.contains("output_tokens=20"), "{rendered}");
        assert!(rendered.contains("total_tokens=30"), "{rendered}");
        assert!(rendered.contains("<｜ref｜>标题<｜/ref｜>"), "保留 OCR 标记: {rendered}");
        assert!(rendered.contains("index=0"), "{rendered}");
        assert!(rendered.contains("该页输出达到上限，文本不完整"), "{rendered}");
        // error 对象（识别后失败通常 200 + error）
        let error = parse_ocr_response(
            r#"{"error":{"message":"OCR did not finish in time","type":"gateway_timeout_error","code":"gateway_timeout_error"}}"#,
            20,
        )
        .expect_err("应返回错误");
        assert!(error.contains("OCR did not finish in time"), "{error}");
        assert!(error.starts_with("DeepSeek OCR 失败"), "{error}");
        // 非 JSON
        assert!(parse_ocr_response("<html>bad gateway</html>", 20).is_err());
        // 没有任何 pages
        let rendered = parse_ocr_response(r#"{"page_count":0,"pages":[]}"#, 20).unwrap();
        assert!(rendered.contains("没有 pages 数组"), "{rendered}");
    }

    #[test]
    fn ocr_page_truncation_and_limits() {
        let long_text = "字".repeat(MAX_PAGE_CHARS + 100);
        let pages: Vec<Value> = (0..5)
            .map(|index| json!({"index": index, "text": long_text, "finish_reason": "stop"}))
            .collect();
        let body = json!({"page_count": 5, "pages": pages, "usage": {"total_tokens": 1}}).to_string();
        let rendered = parse_ocr_response(&body, 2).unwrap();
        assert!(rendered.contains("第 1 页"), "{rendered}");
        assert!(rendered.contains("第 2 页"), "{rendered}");
        assert!(!rendered.contains("第 3 页"), "max_pages=2 只显示前 2 页: {rendered}");
        assert!(rendered.contains("还有 3 页未显示"), "{rendered}");
        assert!(rendered.contains("已截断"), "超长单页应截断: {rendered}");
        assert!(rendered.chars().count() < 12_000, "输出不应无限长");
        // max_pages clamp 到 200
        let pages: Vec<Value> = (0..3)
            .map(|index| json!({"index": index, "text": "t", "finish_reason": "stop"}))
            .collect();
        let body = json!({"page_count": 3, "pages": pages}).to_string();
        let rendered = parse_ocr_response(&body, 999).unwrap();
        assert!(rendered.contains("第 3 页"), "{rendered}");
    }

    #[test]
    fn chat_response_parsing() {
        let body = r#"{"id":"chat-1","choices":[{"index":0,"message":{"role":"assistant","content":"文档要点是：年度营收增长 20%。"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}}"#;
        let (answer, usage) = parse_chat_response(body).expect("应能解析");
        assert_eq!(answer, "文档要点是：年度营收增长 20%。");
        assert!(usage.contains("total_tokens"), "usage 不应为空: {usage}");
        // content 数组形式
        let body = r#"{"choices":[{"message":{"content":[{"type":"text","text":"A"},{"type":"text","text":"B"}]}}]}"#;
        let (answer, _) = parse_chat_response(body).unwrap();
        assert_eq!(answer, "A\nB");
        // 错误对象
        let error = parse_chat_response(r#"{"error":{"message":"context too long"}}"#)
            .expect_err("应报错");
        assert!(error.contains("context too long"), "{error}");
        assert!(parse_chat_response("not json").is_err());
        assert!(parse_chat_response(r#"{"choices":[]}"#).is_err());
    }

    #[test]
    fn file_id_collection_limits() {
        let ids: Vec<Value> = (0..20).map(|i| json!(format!("file-api-{i}"))).collect();
        let collected = collect_file_ids(&json!({"file_ids": ids})).unwrap();
        assert_eq!(collected.len(), 20);
        // 21 个超限
        let ids: Vec<Value> = (0..21).map(|i| json!(format!("file-api-{i}"))).collect();
        let error = collect_file_ids(&json!({"file_ids": ids})).expect_err("应超限");
        assert!(error.contains("最多引用 20 个文档"), "{error}");
        // 单 file_id
        assert_eq!(
            collect_file_ids(&json!({"file_id": "file-api-single"})).unwrap(),
            vec!["file-api-single".to_string()]
        );
        // 合并 + 去重
        let collected = collect_file_ids(&json!({
            "file_id": "file-api-a",
            "file_ids": ["file-api-b", "file-api-a"]
        }))
        .unwrap();
        assert_eq!(collected, vec!["file-api-a".to_string(), "file-api-b".to_string()]);
        // 空/缺参数/类型错
        assert!(collect_file_ids(&json!({})).is_err());
        assert!(collect_file_ids(&json!({"file_id": ""})).is_err());
        assert!(collect_file_ids(&json!({"file_ids": "not-array"})).is_err());
        assert!(collect_file_ids(&json!({"file_ids": [1, 2]})).is_err());
    }

    #[test]
    fn registration_conditions() {
        // 官方端点 + key → 注册 3 个
        let reg = ToolRegistry::new();
        let settings = test_settings("https://api.deepseek.com", "sk-test");
        assert!(register_if_available(&reg, &settings));
        let names = reg.names();
        for expected in [
            "deepseek_file_upload",
            "deepseek_ocr",
            "deepseek_document_qa",
        ] {
            assert!(names.iter().any(|name| name == expected), "缺少 {expected}");
        }
        assert_eq!(names.len(), 3);
        // 官方端点但无 key → 不注册（用不存在的 env: 变量，避免本机环境变量干扰）
        let reg = ToolRegistry::new();
        let missing_key = format!("env:NEOSONIA_TEST_SURELY_MISSING_{}", std::process::id());
        let settings = test_settings("https://api.deepseek.com", &missing_key);
        assert!(!register_if_available(&reg, &settings));
        assert!(!reg
            .names()
            .iter()
            .any(|name| name == "deepseek_file_upload"));
        // 非官方端点（即使有 key）→ 不注册
        let reg = ToolRegistry::new();
        let settings = test_settings("https://api.openai.com/v1", "sk-test");
        assert!(!register_if_available(&reg, &settings));
        // base_url 空 = 官方默认，有 key 时注册
        let reg = ToolRegistry::new();
        let settings = test_settings("", "sk-test");
        assert!(register_if_available(&reg, &settings));
        // 无条件 register() 始终 3 个（catalog 用）
        let reg = ToolRegistry::new();
        register(&reg);
        assert_eq!(reg.names().len(), 3);
    }

    #[test]
    fn http_error_message_includes_status_and_body() {
        let message = http_error_message("DeepSeek 文件上传", 413, "file is too large: max 100 MiB");
        assert!(message.contains("HTTP 413"), "{message}");
        assert!(message.contains("file is too large"), "{message}");
        assert!(message.contains("DeepSeek 文件上传失败"), "{message}");
        let long_body = "x".repeat(2000);
        let message = http_error_message("DeepSeek OCR", 500, &long_body);
        assert!(message.chars().count() < 700, "响应体应截断到 500 字符");
    }

    /// 真实联调（需要 `NEOSONIA_TEST_DEEPSEEK_KEY`，会真实上传文件并计费）。
    /// 运行：`NEOSONIA_TEST_DEEPSEEK_KEY=sk-... cargo test --offline -p neosonia-tools -- --ignored --nocapture deepseek`
    #[tokio::test]
    #[ignore]
    async fn verify_real_deepseek_files_ocr_qa() {
        let Ok(key) = std::env::var("NEOSONIA_TEST_DEEPSEEK_KEY") else {
            println!("[skip] 未设置 NEOSONIA_TEST_DEEPSEEK_KEY，跳过真实联调");
            return;
        };
        let key = key.trim().to_string();
        if key.is_empty() {
            println!("[skip] NEOSONIA_TEST_DEEPSEEK_KEY 为空，跳过真实联调");
            return;
        }
        let root = crate::test_support::temp_root("deepseek-files-live");
        let (ctx, host) = crate::test_support::fake_ctx(&root);
        {
            let mut settings = host.settings.write();
            settings.server.base_url = "https://api.deepseek.com".to_string();
            settings.server.api_key = key;
        }
        // 1) 上传一个纯文本文件（user_data 不计解析费）
        let text_path = root.join("neosonia_file_qa_test.txt");
        std::fs::write(
            &text_path,
            "NeoSonia 文件解析联调测试。机密关键词：蓝色海豚-7391。请记住这个关键词。",
        )
        .expect("写测试文本文件");
        let upload = deepseek_file_upload(
            &ctx,
            json!({"path": text_path.to_string_lossy(), "purpose": "user_data"}),
        )
        .await;
        println!("=== deepseek_file_upload ===\n{}", upload.content);
        assert!(upload.ok, "{}", upload.content);
        let file_id = upload
            .content
            .lines()
            .find_map(|line| line.strip_prefix("file_id："))
            .map(|value| value.trim().to_string())
            .expect("上传结果应包含 file_id");
        // 2) 基于文档提问
        let qa = deepseek_document_qa(
            &ctx,
            json!({"file_id": file_id, "question": "这个文件里的机密关键词是什么？"}),
        )
        .await;
        println!("=== deepseek_document_qa ===\n{}", qa.content);
        assert!(qa.ok, "{}", qa.content);
        assert!(!qa.content.trim().is_empty());
        // 3) OCR 一张白底图片（验证接口链路；空图可能只返回 1 页空文本）
        let png_path = root.join("neosonia_ocr_test.png");
        let image = image::RgbaImage::from_pixel(600, 200, image::Rgba([255, 255, 255, 255]));
        image.save(&png_path).expect("写测试 PNG");
        let ocr = deepseek_ocr(
            &ctx,
            json!({"path": png_path.to_string_lossy(), "purpose": "text_recognition", "max_pages": 2}),
        )
        .await;
        println!("=== deepseek_ocr ===\n{}", ocr.content);
        assert!(ocr.ok, "{}", ocr.content);
        assert!(ocr.content.contains("页数："), "{}", ocr.content);
    }
}
