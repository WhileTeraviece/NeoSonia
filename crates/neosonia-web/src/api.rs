//! HTTP API handler。
//!
//! 除 `POST /api/pair` 与 `/healthz` 外，所有 `/api/*` 都经过 [`crate::auth::require_device`]
//! 鉴权（handler 里用 `Extension<AuthDevice>` 取当前设备）。
//!
//! SQLite / 引擎的同步调用放 `spawn_blocking`，避免阻塞 axum 的 async worker。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Extension, Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use neosonia_core::model::ImagePart;
use neosonia_core::{ChatMessage, ConversationHit, ConversationStore, util::now_ms};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::VERSION;
use crate::auth::{AuthDevice, reject};
use crate::server::Inner;

// ============================================================
// 响应工具
// ============================================================

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    let message = message.into();
    reject(status, &message)
}

fn json_ok<T: Serialize>(value: T) -> Response {
    match serde_json::to_value(value) {
        Ok(v) => Json(v).into_response(),
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("序列化响应失败：{e}"),
        ),
    }
}

fn ok_value(value: Value) -> Response {
    Json(value).into_response()
}

/// 把同步的 SQLite/引擎调用放到阻塞线程池里执行。
async fn run_blocking<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{e}")),
        Err(e) => Err(format!("后台任务执行失败：{e}")),
    }
}

fn device_addr_text(addr: &SocketAddr) -> String {
    addr.ip().to_string()
}

// ============================================================
// 健康检查
// ============================================================

/// `/healthz` 的 JSON（拆成纯函数便于单测）。
fn healthz_value(bind: &str, advertise_url: &str, lan_ip: &str, lan_only: bool) -> Value {
    json!({
        "ok": true,
        "service": "neosonia-web",
        "version": VERSION,
        "bind": bind,
        "advertise_url": advertise_url,
        "lan_ip": lan_ip,
        "lan_only": lan_only,
        "ts_ms": now_ms(),
    })
}

/// `GET /healthz`（公开）：手机排障时先访问它，可确认服务绑在 LAN 地址而不是 127.0.0.1。
pub async fn healthz(State(inner): State<Arc<Inner>>) -> Response {
    let lan_only = inner.settings.read().remote.lan_only;
    ok_value(healthz_value(
        &inner.bind_text(),
        &inner.lan_base_url(),
        &inner.advertise_host(),
        lan_only,
    ))
}

// ============================================================
// 配对
// ============================================================

#[derive(Debug, Deserialize)]
pub struct PairReq {
    /// 桌面端显示的 8 位配对码（可带连字符/空格）。
    #[serde(default)]
    pub code: String,
    /// 设备名（用户填写）。
    #[serde(default)]
    pub name: Option<String>,
    /// 兼容字段：没填 name 时用这个。
    #[serde(default)]
    pub name_ua: Option<String>,
}

/// 配对码校验失败的文案（客户端按这些关键词分类：错误/已使用/已过期）。
fn pair_check_message(check: crate::pairing::PairCheck) -> &'static str {
    use crate::pairing::PairCheck;
    match check {
        PairCheck::Ok => "配对成功。",
        PairCheck::Used => "配对码已被使用过（一次性）。请在桌面端刷新配对码后重试。",
        PairCheck::Expired => "配对码已过期。请在桌面端刷新配对码后重试。",
        PairCheck::Mismatch => "配对码不正确，请核对后重试。",
    }
}

/// `POST /api/pair {code, name, name_ua?}` -> `{token, device}`。
pub async fn pair(
    State(inner): State<Arc<Inner>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<PairReq>,
) -> Response {
    let remote = inner.settings.read().remote.clone();
    if inner.devices.count() as u32 >= remote.max_devices {
        return json_error(
            StatusCode::CONFLICT,
            format!(
                "已信任设备数达到上限（{}）。请先在桌面端「远程访问」页撤销一台旧设备。",
                remote.max_devices
            ),
        );
    }
    if remote.require_pairing {
        let check = inner.pairing.consume(&req.code);
        if check != crate::pairing::PairCheck::Ok {
            return json_error(StatusCode::UNAUTHORIZED, pair_check_message(check));
        }
    }

    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let name = req
        .name
        .as_deref()
        .or(req.name_ua.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| guess_device_name(&ua));
    let addr = device_addr_text(&peer);

    match inner.devices.pair(&name, &ua, &addr, remote.max_devices) {
        Ok((token, device)) => {
            tracing::info!(device = %device.id, "远程设备配对成功");
            ok_value(json!({ "token": token, "device": device }))
        }
        Err(crate::devices::PairError::TooMany(max)) => json_error(
            StatusCode::CONFLICT,
            format!("已信任设备数达到上限（{max}）。请先撤销一台旧设备。"),
        ),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn guess_device_name(ua: &str) -> String {
    let ua = ua.to_ascii_lowercase();
    if ua.contains("iphone") {
        "iPhone".to_string()
    } else if ua.contains("ipad") {
        "iPad".to_string()
    } else if ua.contains("android") {
        "Android 设备".to_string()
    } else if ua.contains("mac os x") || ua.contains("macintosh") {
        "Mac".to_string()
    } else if ua.contains("windows") {
        "Windows 电脑".to_string()
    } else if ua.contains("linux") {
        "Linux 设备".to_string()
    } else {
        "远程设备".to_string()
    }
}

// ============================================================
// 当前设备 / 服务信息
// ============================================================

/// `/api/me` 的 mode 字段（`agent.mode` 的 id + 中文 label）。
fn mode_fields(settings: &neosonia_core::Settings) -> (String, String) {
    let mode = settings.agent.mode;
    (mode.id().to_string(), mode.label().to_string())
}

/// API 厂商状态（v0.7 收尾）：给 SPA 顶栏小球用的精简 DTO。
#[derive(Clone, Debug, Serialize)]
pub struct ProviderStatusDto {
    pub provider: String,
    /// `green` / `yellow` / `orange` / `red`
    pub indicator: String,
    /// 中文状态："正常" / "轻微故障" / "部分中断" / "严重中断"
    pub label: String,
    /// 厂商状态页 URL（点击小球打开）。
    pub url: String,
    /// 细节（故障组件/原因），可为空。
    pub detail: String,
}

/// 把 core 的 ProviderStatus 转成 DTO；unknown/空的返回 None（SPA 隐藏小球）。
fn provider_status_dto(
    status: neosonia_core::provider_status::ProviderStatus,
) -> Option<ProviderStatusDto> {
    let indicator = status.indicator.trim().to_ascii_lowercase();
    if indicator.is_empty() || indicator == "unknown" {
        return None;
    }
    Some(ProviderStatusDto {
        provider: status.provider,
        indicator,
        label: status.label,
        url: status.url,
        detail: status.detail,
    })
}

/// 查一次厂商状态：3s 超时（core 内部 60s 缓存 + 单飞），失败/未知返回 None。
/// **不要**让 /api/me 因为状态页网络问题变慢或报错。
async fn query_provider_status(settings: &neosonia_core::Settings) -> Option<ProviderStatusDto> {
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        neosonia_core::provider_status::current_status(settings),
    )
    .await
    .ok()??;
    provider_status_dto(status)
}

/// `GET /api/me` -> `{device, server:{hostname, version, busy, model, mode, mode_label, bind, advertise_url, lan_ip, lan_url, client_ip, pairing_active, provider_status}}`。
pub async fn me(
    State(inner): State<Arc<Inner>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Extension(auth): Extension<AuthDevice>,
) -> Response {
    // 注意：不能拿着 parking_lot guard 跨 await，先 clone 一份 settings。
    let settings = inner.settings.read().clone();
    let (model, missing_key, (mode, mode_label)) = (
        settings.server.model.clone(),
        settings.effective_api_key().is_empty(),
        mode_fields(&settings),
    );
    // v0.7：厂商 API 状态（失败/未知 = null，前端隐藏小球）。
    let provider_status = query_provider_status(&settings).await;
    let busy = inner.engine.is_busy();
    let pairing_active = inner.pairing.view().is_active();
    let pending_reviews = crate::review::review_hub().len();
    let bind = inner.bind_text();
    let advertise_url = inner.lan_base_url();
    let lan_ip = inner.advertise_host();
    ok_value(json!({
        "device": auth.0,
        "server": {
            "hostname": crate::net::hostname(),
            "version": VERSION,
            "busy": busy,
            "model": model,
            "missing_api_key": missing_key,
            "mode": mode,
            "mode_label": mode_label,
            // v0.6：排障用。bind 必须是 0.0.0.0（或 LAN IP），不能是 127.0.0.1。
            "bind": bind,
            "advertise_url": advertise_url,
            "lan_ip": lan_ip,
            "lan_url": advertise_url,
            // 当前这次请求来自哪个 IP（手机应该是 192.168.x.x 之类私有地址）。
            "client_ip": peer.ip().to_string(),
            "pairing_active": pairing_active,
            "review_pending": pending_reviews,
            // v0.7：{provider, indicator, label, url, detail} | null
            "provider_status": provider_status,
        }
    }))
}

/// `GET /api/review/pending` -> `Vec<ReviewItem>`。
///
/// **只读**：远程端只能看到有哪些操作在等桌面端审查，不能批准。
pub async fn review_pending() -> Response {
    json_ok(crate::review::review_hub().pending())
}

/// `GET /api/devices`：已信任设备列表（多出来的便利接口，SPA 设备页用）。
pub async fn devices(State(inner): State<Arc<Inner>>) -> Response {
    json_ok(inner.devices.list())
}

// ============================================================
// 会话历史
// ============================================================

#[derive(Debug, Deserialize)]
pub struct SessionsQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /api/sessions?q=` -> `Vec<ConversationSummary>`。
pub async fn sessions(
    State(inner): State<Arc<Inner>>,
    Query(query): Query<SessionsQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let q = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let store: Arc<ConversationStore> = inner.conversations.clone();
    match run_blocking(move || store.list(q.as_deref(), limit)).await {
        Ok(list) => json_ok(list),
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("读取会话列表失败：{e}"),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct MessagesQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    /// 只取 seq 小于该值的更早消息（不传 = 最后 limit 条，行为与旧版一致）。
    #[serde(default)]
    pub before: Option<i64>,
}

/// 会话消息 DTO：图片只回 `has_image` / `image_count`，**不传 base64**。
#[derive(Debug, Serialize)]
pub struct SessionMessage {
    pub id: String,
    /// 会话内 0 基序号（新增字段；旧客户端忽略）。分页时把它作为 `before` 传回。
    pub seq: i64,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<neosonia_core::ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_ok: Option<bool>,
    pub created_at_ms: i64,
    pub has_image: bool,
    pub image_count: usize,
}

fn to_session_message(seq: i64, msg: &ChatMessage) -> SessionMessage {
    SessionMessage {
        id: msg.id.clone(),
        seq,
        role: msg.role.as_str().to_string(),
        content: msg.content.clone(),
        reasoning_content: msg.reasoning_content.clone(),
        tool_calls: msg.tool_calls.clone(),
        tool_call_id: msg.tool_call_id.clone(),
        name: msg.name.clone(),
        tool_ok: msg.tool_ok,
        created_at_ms: msg.created_at_ms,
        has_image: msg.has_images(),
        image_count: msg.images.len(),
    }
}

/// 分页默认条数：首屏最多 200（只渲染最近 200 条），"加载更早"一页 100 条。
fn messages_limit(limit: Option<usize>, has_before: bool) -> usize {
    limit
        .unwrap_or(if has_before { 100 } else { 200 })
        .clamp(1, 500)
}

/// `GET /api/sessions/{id}?before=<seq>&limit=` -> `Vec<SessionMessage>`（seq 升序）。
///
/// 不传 `before` 时返回最后 `limit` 条（默认 200），与旧版行为兼容；
/// 传 `before=<最小 seq>` 时返回更早的一页（默认 100 条）。
pub async fn session_messages(
    State(inner): State<Arc<Inner>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<MessagesQuery>,
) -> Response {
    if id.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "会话 id 不能为空。");
    }
    let limit = messages_limit(query.limit, query.before.is_some());
    let before = query.before;
    let store: Arc<ConversationStore> = inner.conversations.clone();
    match run_blocking(move || store.messages_before(&id, before, limit)).await {
        Ok(rows) => json_ok(
            rows.iter()
                .map(|(seq, msg)| to_session_message(*seq, msg))
                .collect::<Vec<_>>(),
        ),
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("读取会话消息失败：{e}"),
        ),
    }
}

/// `POST /api/sessions/{id}/delete`.
pub async fn session_delete(
    State(inner): State<Arc<Inner>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    if id.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "会话 id 不能为空。");
    }
    let store: Arc<ConversationStore> = inner.conversations.clone();
    match run_blocking(move || store.delete(&id)).await {
        Ok(deleted) => ok_value(json!({ "deleted": deleted })),
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("删除会话失败：{e}"),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /api/search?q=` -> `Vec<ConversationHit>`。
pub async fn search(
    State(inner): State<Arc<Inner>>,
    Query(query): Query<SearchQuery>,
) -> Response {
    let q = query.q.as_deref().map(str::trim).unwrap_or("").to_string();
    if q.is_empty() {
        return json_ok(Vec::<ConversationHit>::new());
    }
    let limit = query.limit.unwrap_or(30).clamp(1, 200);
    let store: Arc<ConversationStore> = inner.conversations.clone();
    match run_blocking(move || store.search_messages(&q, limit)).await {
        Ok(hits) => json_ok(hits),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("搜索失败：{e}")),
    }
}

// ============================================================
// 聊天
// ============================================================

#[derive(Debug, Deserialize)]
pub struct ChatReq {
    #[serde(default)]
    pub text: String,
    /// 指定把这条消息发到哪个历史会话（不传 = 沿用引擎当前会话）。
    #[serde(default)]
    pub conv_id: Option<String>,
    /// v0.7：先 `POST /api/upload` 拿到的附件 id 列表（≤8 个）。
    #[serde(default)]
    pub attachments: Vec<String>,
}

/// 把上传的图片读成 `ImagePart`（长边 ≤1536，PNG 编码）；失败返回 None。
fn load_image_part(entry: &crate::uploads::UploadEntry) -> Option<ImagePart> {
    use base64::Engine as _;
    let img = image::open(&entry.path).ok()?;
    let (w, h) = (img.width(), img.height());
    let max_edge = 1536u32;
    let scaled = if w.max(h) > max_edge {
        let ratio = max_edge as f32 / w.max(h) as f32;
        let nw = ((w as f32 * ratio).round() as u32).max(1);
        let nh = ((h as f32 * ratio).round() as u32).max(1);
        img.resize(nw, nh, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut buf);
    scaled.write_to(&mut cursor, image::ImageFormat::Png).ok()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
    Some(ImagePart::png(
        b64,
        scaled.width(),
        scaled.height(),
        Some(entry.info.name.clone()),
    ))
}

/// 为模型生成附件说明 prompt + 图片输入。
fn build_attachments(
    paths: &neosonia_core::Paths,
    ids: &[String],
) -> anyhow::Result<(String, Vec<ImagePart>)> {
    let entries = crate::uploads::resolve_ids(paths, ids);
    if entries.is_empty() {
        return Ok((String::new(), Vec::new()));
    }
    let mut prompt = String::from("\n\n【远程上传附件】");
    let mut images = Vec::new();
    for entry in &entries {
        let size = neosonia_core::util::human_bytes(entry.info.size);
        match entry.info.kind.as_str() {
            "image" => {
                prompt.push_str(&format!(
                    "\n- 图片: {}（{}） {}",
                    entry.info.name, size, entry.info.url
                ));
                if let Some(part) = load_image_part(entry) {
                    images.push(part);
                } else {
                    prompt.push_str("（图片解码失败，仅保留文件）");
                }
            }
            "text" => {
                prompt.push_str(&format!(
                    "\n- 文本: {}（{}） {}",
                    entry.info.name, size, entry.info.url
                ));
                match std::fs::read(&entry.path) {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        let total = text.chars().count();
                        let mut preview: String = text
                            .chars()
                            .take(crate::uploads::MAX_TEXT_INLINE_CHARS)
                            .collect();
                        if total > crate::uploads::MAX_TEXT_INLINE_CHARS {
                            preview.push_str("\n…（内容已截断，完整文件可用 read_file 读取）");
                        }
                        prompt.push_str(&format!("\n内容预览：\n```\n{preview}\n```"));
                    }
                    Err(_) => prompt.push_str("\n（读取失败）"),
                }
            }
            _ => {
                prompt.push_str(&format!(
                    "\n- 文件: {}（{}） 本地路径：{}",
                    entry.info.name,
                    size,
                    entry.path.display()
                ));
            }
        }
    }
    Ok((prompt, images))
}

/// `POST /api/chat {text, conv_id?, attachments?}` -> 202 `{turn_id}`。
pub async fn chat(State(inner): State<Arc<Inner>>, Json(req): Json<ChatReq>) -> Response {
    let mut text = req.text.trim().to_string();
    let attachments = req.attachments.clone();
    if attachments.len() > crate::uploads::MAX_ATTACHMENTS {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!(
                "单条消息最多 {} 个附件。",
                crate::uploads::MAX_ATTACHMENTS
            ),
        );
    }
    if text.is_empty() && attachments.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "消息内容为空。");
    }
    let engine = inner.engine.clone();
    if let Some(conv) = req
        .conv_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    {
        if engine.conversation_id().as_deref() != Some(conv.as_str()) {
            let engine_for_load = engine.clone();
            if let Err(e) = run_blocking(move || {
                engine_for_load.load_conversation(conv);
                Ok(())
            })
            .await
            {
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, e);
            }
        }
    }
    let (attachments_prompt, images) = if attachments.is_empty() {
        (String::new(), Vec::new())
    } else {
        let paths = inner.paths.clone();
        let ids = attachments.clone();
        match run_blocking(move || build_attachments(&paths, &ids)).await {
            Ok(v) => v,
            Err(e) => {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("读取附件失败：{e}"),
                );
            }
        }
    };
    if text.is_empty() {
        text = "（用户上传了附件）".to_string();
    }
    let full_prompt = format!("{text}{attachments_prompt}");
    let turn_id = if images.is_empty() {
        engine.submit(full_prompt)
    } else {
        engine.submit_with_images(full_prompt, images)
    };
    (
        StatusCode::ACCEPTED,
        Json(json!({ "turn_id": turn_id })),
    )
        .into_response()
}

// ============================================================
// 远程附件上传（v0.7）
// ============================================================

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    #[serde(default)]
    pub name: Option<String>,
    /// image / text / file（提示用；实际按 mime + 文件名判定）
    #[serde(default)]
    pub kind: Option<String>,
}

/// `POST /api/upload?name=<urlencoded>`：body 是文件原始字节。
pub async fn upload(
    State(inner): State<Arc<Inner>>,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "上传内容为空。");
    }
    if body.len() > crate::uploads::MAX_UPLOAD_BYTES {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "文件过大，上限 {} MB。",
                crate::uploads::MAX_UPLOAD_BYTES / 1024 / 1024
            ),
        );
    }
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let name = query
        .name
        .as_deref()
        .or(query.kind.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("附件")
        .to_string();
    let paths = inner.paths.clone();
    match run_blocking(move || crate::uploads::save(&paths, &name, &mime, &body)).await {
        Ok(info) => json_ok(info),
        Err(e) => json_error(StatusCode::BAD_REQUEST, e),
    }
}

/// `GET /api/uploads/{id}`：鉴权读取上传的原始文件。
pub async fn upload_get(
    State(inner): State<Arc<Inner>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let Some(entry) = crate::uploads::get(&inner.paths, &id) else {
        return json_error(StatusCode::NOT_FOUND, "附件不存在。");
    };
    match tokio::fs::read(&entry.path).await {
        Ok(bytes) => {
            let mut res = bytes.into_response();
            let content_type = HeaderValue::from_str(&entry.info.mime)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
            let h = res.headers_mut();
            h.insert(header::CONTENT_TYPE, content_type);
            h.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=3600"),
            );
            h.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("inline"),
            );
            res
        }
        Err(_) => json_error(StatusCode::NOT_FOUND, "附件不存在。"),
    }
}

/// `POST /api/devices/{id}/name {name}`：远程重命名已信任设备（v0.7）。
#[derive(Debug, Deserialize)]
pub struct RenameDeviceReq {
    #[serde(default)]
    pub name: String,
}

pub async fn rename_device(
    State(inner): State<Arc<Inner>>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<RenameDeviceReq>,
) -> Response {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "设备名不能为空。");
    }
    match inner.devices.rename(&id, &name) {
        Some(device) => json_ok(device),
        None => json_error(StatusCode::NOT_FOUND, "设备不存在。"),
    }
}

/// `POST /api/new`：清空当前对话（下一次消息会建新会话）。
pub async fn new_conversation(State(inner): State<Arc<Inner>>) -> Response {
    if inner.engine.is_busy() {
        return json_error(
            StatusCode::CONFLICT,
            "当前还有任务在执行，请先停止或等待完成，再新建对话。",
        );
    }
    inner.engine.new_conversation();
    ok_value(json!({ "ok": true }))
}

// ============================================================
// 截图
// ============================================================

/// `GET /api/screenshot/{name}`：安全读取 `paths.screenshots` 下的 PNG。
pub async fn screenshot(
    State(inner): State<Arc<Inner>>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    let path = match crate::screens::resolve(&inner.paths.screenshots, &name) {
        Ok(p) => p,
        Err(msg) => return json_error(StatusCode::BAD_REQUEST, msg),
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut res = bytes.into_response();
            res.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("image/png"),
            );
            res.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            );
            res.headers_mut().insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("inline"),
            );
            res
        }
        Err(_) => json_error(StatusCode::NOT_FOUND, "截图不存在。"),
    }
}

// ============================================================
// 单测
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use neosonia_core::Role;
    use neosonia_core::model::ImagePart;

    #[test]
    fn dto_hides_image_base64() {
        let img = ImagePart::png("QUJDREVGRw==".to_string(), 800, 600, Some("s1.png".into()));
        let msg = ChatMessage::user_with_images("看这个", vec![img]);
        let dto = to_session_message(1, &msg);
        assert_eq!(dto.seq, 1);
        assert_eq!(dto.role, "user");
        assert!(dto.has_image);
        assert_eq!(dto.image_count, 1);
        let json = serde_json::to_string(&dto).expect("序列化 DTO");
        assert!(!json.contains("QUJDREVGRw=="), "DTO 不应包含图片 base64");
        assert!(!json.contains("data_base64"), "DTO 不应包含 data_base64 字段");
        assert!(!json.contains("images"), "DTO 不应包含图片数组");
    }

    #[test]
    fn dto_keeps_tool_metadata() {
        let msg = ChatMessage::tool_result("call-1", "read_file", "内容", false);
        let dto = to_session_message(7, &msg);
        assert_eq!(dto.seq, 7);
        assert_eq!(dto.role, "tool");
        assert_eq!(dto.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(dto.name.as_deref(), Some("read_file"));
        assert_eq!(dto.tool_ok, Some(false));
        assert!(!dto.has_image);
    }

    #[test]
    fn dto_reports_reasoning() {
        let mut msg = ChatMessage::assistant("答案");
        msg.reasoning_content = Some("思考".into());
        let dto = to_session_message(3, &msg);
        assert_eq!(dto.role, Role::Assistant.as_str());
        assert_eq!(dto.reasoning_content.as_deref(), Some("思考"));
    }

    #[test]
    fn mode_fields_default_and_labels() {
        let settings = neosonia_core::Settings::default();
        let (id, label) = mode_fields(&settings);
        assert_eq!(id, "daily");
        assert_eq!(label, "日常");

        let mut settings = neosonia_core::Settings::default();
        settings.agent.mode = neosonia_core::config::AgentMode::Coding;
        let (id, label) = mode_fields(&settings);
        assert_eq!(id, "coding");
        assert_eq!(label, "编程");

        settings.agent.mode = neosonia_core::config::AgentMode::ComputerUse;
        let (id, label) = mode_fields(&settings);
        assert_eq!(id, "computer_use");
        assert_eq!(label, "计算机使用");
    }

    #[test]
    fn healthz_payload_exposes_bind_and_advertise() {
        let value = healthz_value(
            "0.0.0.0:8787",
            "http://192.168.1.10:8787",
            "192.168.1.10",
            true,
        );
        assert_eq!(value["ok"], true);
        assert_eq!(value["bind"], "0.0.0.0:8787");
        assert_eq!(value["advertise_url"], "http://192.168.1.10:8787");
        assert_eq!(value["lan_ip"], "192.168.1.10");
        assert_eq!(value["lan_only"], true);
    }

    #[test]
    fn pair_failure_messages_are_actionable() {
        use crate::pairing::PairCheck;
        let used = pair_check_message(PairCheck::Used);
        assert!(used.contains("已被使用") && used.contains("刷新配对码"), "{used}");
        let expired = pair_check_message(PairCheck::Expired);
        assert!(expired.contains("已过期") && expired.contains("刷新配对码"), "{expired}");
        let mismatch = pair_check_message(PairCheck::Mismatch);
        assert!(mismatch.contains("不正确"), "{mismatch}");
        assert_eq!(pair_check_message(PairCheck::Ok), "配对成功。");
        // 三条错误文案互不相同，客户端才能分类展示。
        assert_ne!(used, expired);
        assert_ne!(used, mismatch);
        assert_ne!(expired, mismatch);
    }

    #[test]
    fn provider_status_dto_serializes_and_hides_unknown() {
        use neosonia_core::provider_status::ProviderStatus;
        let green = ProviderStatus {
            provider: "DeepSeek".to_string(),
            indicator: "green".to_string(),
            label: "正常".to_string(),
            url: "https://status.deepseek.com".to_string(),
            checked_at_ms: 1_700_000_000_000,
            detail: String::new(),
        };
        let dto = provider_status_dto(green).expect("green 应返回 DTO");
        let json = serde_json::to_value(&dto).expect("序列化");
        assert_eq!(json["provider"], "DeepSeek");
        assert_eq!(json["indicator"], "green");
        assert_eq!(json["label"], "正常");
        assert_eq!(json["url"], "https://status.deepseek.com");
        assert_eq!(json["detail"], "");

        // 未知/未知等级 → null（SPA 隐藏小球，不误报）。
        for indicator in ["unknown", "", "UNKNOWN"] {
            let status = ProviderStatus {
                provider: "DeepSeek".to_string(),
                indicator: indicator.to_string(),
                label: "未知".to_string(),
                url: String::new(),
                checked_at_ms: 0,
                detail: "检测失败".to_string(),
            };
            assert!(
                provider_status_dto(status).is_none(),
                "indicator={indicator:?} 应返回 None"
            );
        }
        // 黄色/橙色/红色保留（图例：黄=轻微故障、橙=部分中断、红=严重中断）。
        for (indicator, label) in [
            ("yellow", "轻微故障"),
            ("orange", "部分中断"),
            ("red", "严重中断"),
        ] {
            let status = ProviderStatus {
                provider: "DeepSeek".to_string(),
                indicator: indicator.to_string(),
                label: label.to_string(),
                url: "https://status.deepseek.com".to_string(),
                checked_at_ms: 0,
                detail: "故障事件".to_string(),
            };
            let dto = provider_status_dto(status).expect("应保留故障等级");
            assert_eq!(dto.indicator, indicator);
            assert_eq!(dto.label, label);
        }
    }

    #[test]
    fn pagination_query_and_limits() {
        let q: MessagesQuery = serde_json::from_value(json!({"limit": 50, "before": 120}))
            .expect("解析分页参数");
        assert_eq!(q.before, Some(120));
        assert_eq!(q.limit, Some(50));

        let q: MessagesQuery = serde_json::from_value(json!({})).expect("解析默认参数");
        assert!(q.before.is_none());
        assert!(q.limit.is_none());

        // 首屏默认 200，加载更早默认 100；上限 500，下限 1。
        assert_eq!(messages_limit(None, false), 200);
        assert_eq!(messages_limit(None, true), 100);
        assert_eq!(messages_limit(Some(25), true), 25);
        assert_eq!(messages_limit(Some(0), false), 1);
        assert_eq!(messages_limit(Some(9999), false), 500);
    }

    #[test]
    fn pair_request_parses_tolerant() {
        let req: PairReq = serde_json::from_value(json!({"code": "A2C4-E6F8"})).expect("解析");
        assert_eq!(req.code, "A2C4-E6F8");
        assert!(req.name.is_none());
        let req: PairReq = serde_json::from_value(json!({"code": "X", "name_ua": "iPad"}))
            .expect("解析");
        assert_eq!(req.name_ua.as_deref(), Some("iPad"));
    }

    #[test]
    fn chat_request_parses_tolerant() {
        let req: ChatReq = serde_json::from_value(json!({"text": "hi"})).expect("解析");
        assert_eq!(req.text, "hi");
        assert!(req.conv_id.is_none());
        assert!(req.attachments.is_empty());
        let req: ChatReq =
            serde_json::from_value(json!({"text": "hi", "conv_id": "abc"})).expect("解析");
        assert_eq!(req.conv_id.as_deref(), Some("abc"));
        // v0.7：附件 id 列表。
        let req: ChatReq = serde_json::from_value(
            json!({"text": "", "attachments": ["0123456789abcdef0123456789abcdef"]}),
        )
        .expect("解析附件");
        assert_eq!(req.attachments.len(), 1);
    }

    #[test]
    fn build_attachments_prompt_and_images() {
        use std::io::Cursor;
        let dir = tempfile::tempdir().expect("临时目录");
        let paths = neosonia_core::Paths::from_root(dir.path());
        paths.ensure().expect("建目录");

        // 真 PNG（2x2），确保 image 解码成功并转成 ImagePart。
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([1u8, 2, 3, 255]));
        let mut png: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("编码 PNG");
        let image_info =
            crate::uploads::save(&paths, "shot.png", "image/png", &png).expect("保存图片");
        let text_info = crate::uploads::save(&paths, "note.md", "text/markdown", "# 标题\n内容".as_bytes())
            .expect("保存文本");

        let (prompt, images) = build_attachments(
            &paths,
            &[image_info.id.clone(), text_info.id.clone()],
        )
        .expect("构建附件");
        assert!(prompt.contains("【远程上传附件】"), "{prompt}");
        assert!(prompt.contains("图片: shot.png"), "{prompt}");
        assert!(prompt.contains("文本: note.md"), "{prompt}");
        assert!(prompt.contains("内容"), "文本内容应内联进 prompt");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime, "image/png");
        assert_eq!(images[0].width, 2);
        assert!(!images[0].data_base64.is_empty());

        // 不存在的 id 被忽略，不报错。
        let (empty_prompt, empty_images) =
            build_attachments(&paths, &["bad-id".to_string()]).expect("忽略非法附件");
        assert!(empty_prompt.is_empty());
        assert!(empty_images.is_empty());
    }

    #[test]
    fn guess_device_names() {
        assert_eq!(guess_device_name("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0)"), "iPhone");
        assert_eq!(guess_device_name("Mozilla/5.0 (iPad; CPU OS 17_0)"), "iPad");
        assert_eq!(guess_device_name("Mozilla/5.0 (Linux; Android 14)"), "Android 设备");
        assert_eq!(guess_device_name("Mozilla/5.0 (Windows NT 10.0; Win64; x64)"), "Windows 电脑");
        assert_eq!(guess_device_name("curl/8.0"), "远程设备");
    }

    /// core 的 ConversationStore 集成测试：会话列表 / 消息 / 搜索 / 删除 + DTO 映射。
    #[test]
    fn conversation_store_roundtrip() {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = ConversationStore::open(&dir.path().join("conversations.sqlite3"))
            .expect("打开会话库");
        let id = store
            .ensure(None, "帮我检查工作目录里的文件", "default")
            .expect("建会话");
        store
            .append(&id, &ChatMessage::user("帮我检查工作目录里的文件"))
            .expect("追加用户消息");
        store
            .append(&id, &ChatMessage::assistant("好的，我先列一下目录。"))
            .expect("追加助手消息");

        let list = store.list(None, 20).expect("会话列表");
        assert_eq!(list.len(), 1, "应有一个会话");
        assert_eq!(list[0].id, id);
        assert!(list[0].title.contains("工作目录"), "标题应取首条用户消息");

        let msgs = store.messages(&id, 0).expect("会话消息");
        assert!(msgs.len() >= 2, "应能读回至少两条消息");
        let dto: Vec<SessionMessage> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| to_session_message(i as i64 + 1, m))
            .collect();
        let json = serde_json::to_value(&dto).expect("序列化消息列表");
        assert!(json.is_array());
        assert!(json[0]["seq"].is_number(), "DTO 应带 seq 字段");

        let hits = store.search_messages("工作目录", 10).expect("搜索消息");
        assert!(!hits.is_empty(), "应能搜到包含关键词的消息");
        assert_eq!(hits[0].conversation_id, id);

        assert!(store.delete(&id).expect("删除会话"));
        assert!(store.list(None, 20).expect("删除后列表").is_empty());
    }

    /// seq 窗口分页：跨页连续、before 边界、空会话、last_seq。
    #[test]
    fn seq_window_pagination_roundtrip() {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = ConversationStore::open(&dir.path().join("page.sqlite3")).expect("打开会话库");
        let id = store.ensure(None, "分页测试会话", "default").expect("建会话");
        for i in 0..12 {
            store
                .append(&id, &ChatMessage::user(format!("分页消息 {i}")))
                .expect("追加消息");
        }

        // 最后一页：默认取最后 5 条，seq 升序。
        let last = store.messages_before(&id, None, 5).expect("最后一页");
        assert_eq!(last.len(), 5);
        let seqs: Vec<i64> = last.iter().map(|(seq, _)| *seq).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seq 必须升序");
        let first_seq = seqs[0];

        // 更早一页：seq 全部小于上一页第一条，且两页连续不重叠。
        let older = store.messages_before(&id, Some(first_seq), 5).expect("更早一页");
        assert_eq!(older.len(), 5);
        assert!(older.iter().all(|(seq, _)| *seq < first_seq));
        assert_eq!(older.last().map(|(seq, _)| *seq), Some(first_seq - 1));

        // before 边界：seq=1 没有更早的消息。
        assert!(store.messages_before(&id, Some(1), 10).expect("边界").is_empty());
        // limit=0 = 全部（旧语义），last_seq 从 1 开始。
        assert_eq!(store.messages_before(&id, None, 0).expect("全部").len(), 12);
        assert_eq!(store.last_seq(&id).expect("last_seq"), 12);

        // 空会话：消息为空、last_seq = -1（core 契约）。
        let empty = store.ensure(Some("empty-conv"), "空会话", "default").expect("空会话");
        assert!(store.messages_before(&empty, None, 10).expect("空会话消息").is_empty());
        assert_eq!(store.last_seq(&empty).expect("空会话 last_seq"), -1);
    }
}
