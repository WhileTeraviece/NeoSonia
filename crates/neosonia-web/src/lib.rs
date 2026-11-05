//! NeoSonia 局域网远程访问（axum 0.8）。
//!
//! 桌面端（`neosonia-app`）通过 [`WebServer::start`] 在 tokio runtime 里拉起一个
//! HTTP + WebSocket 服务，手机/平板浏览器扫码配对后即可查看历史对话、收发消息、
//! 实时观看 agent 的流式输出。
//!
//! # 组成
//!
//! - [`WebServer`] / [`WebConfig`] / [`WebInfo`]：对 app 暴露的服务接口（契约
//!   `docs/contracts-phase2.md` 第 5.1 节）。
//! - [`ReviewHub`] / [`review_hub`]：远程审查队列（只读展示待审批操作，
//!   **不能远程批准**）。app 在审批事件/结束时 `push` / `resolve` 即可，无需改
//!   [`WebConfig`]。
//! - HTTP 路由：`GET /`（内嵌 SPA）、`GET /pair`、`POST /api/pair`、
//!   `GET /api/me`、`GET /api/sessions`、`GET /api/sessions/{id}`（支持
//!   `before=<seq>&limit=` 分页）、`POST /api/sessions/{id}/delete`、
//!   `GET /api/search`、`GET /api/review/pending`、`POST /api/chat`、
//!   `GET /api/devices`、`POST /api/new`、`GET /api/screenshot/{name}`、`GET /ws`。
//! - 安全：配对码一次性 + TTL；token 只以 sha256 落盘；`lan_only` 时校验 peer IP
//!   属于私有网段；截图路径 canonicalize 后校验前缀，防目录穿越。
//! - 前端：`assets/web/{index.html,app.js,style.css}` 用 `include_str!` 内嵌，
//!   不引用任何 CDN，离线可用（骨架屏、消息分页、流式合批渲染）。
//!
//! 所有用户可见文案为中文，零 emoji。

mod api;
mod assets;
mod auth;
mod devices;
mod events;
mod net;
mod pairing;
mod review;
mod screens;
mod server;
mod uploads;
mod ws;

pub use review::{ReviewHub, ReviewItem, review_hub};
pub use server::WebServer;

use std::path::PathBuf;
use std::sync::Arc;

use neosonia_core::{AgentEngine, AgentEvent, ConversationStore, Paths, Settings, TrustedDevice};
use parking_lot::RwLock;

/// 版本号（和 app / core 保持同一个 workspace 版本）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 启动 Web 服务需要的依赖。由 `neosonia-app` 在 tokio runtime 内构造。
///
/// v0.6 新增 `bind` / `advertise_url` 两个 **Option** 字段（None = 回退到
/// `settings.remote.bind` / 自动探测本机 LAN IP），原有字段不变。
pub struct WebConfig {
    /// 目录布局（`data/devices.json`、`data/qrcodes/`、`data/screenshots/` 等）。
    pub paths: Paths,
    /// 全量设置（`remote` 块决定监听地址/端口/配对策略；运行期可热改）。
    pub settings: Arc<RwLock<Settings>>,
    /// agent 引擎句柄：提交消息、取消、切会话、读忙碌状态。
    pub engine: Arc<AgentEngine>,
    /// 对话历史库：会话列表 / 消息 / 搜索 / 删除。
    pub conversations: Arc<ConversationStore>,
    /// 引擎事件广播：WebSocket 订阅后原样转发给浏览器。
    pub events: tokio::sync::broadcast::Sender<AgentEvent>,
    /// 监听地址覆盖（例如 `Some("0.0.0.0")`）。`None` = 用 `settings.remote.bind`。
    ///
    /// 手机连不上最常见的原因就是这里/设置里绑了 `127.0.0.1`：app 应传 `0.0.0.0`。
    pub bind: Option<String>,
    /// 对外广播地址（例如 `Some("http://192.168.1.10:8787")`）。
    ///
    /// QR 码、配对页链接、`/api/me` 的 `advertise_url` 都用它；`None` = 根据监听地址
    /// 自动探测本机 LAN IP（绑 0.0.0.0 时取 `local_ip_address::local_ip()`）。
    /// 会做规范化（去尾斜杠、拒绝 `0.0.0.0`），非法值回退自动探测。
    pub advertise_url: Option<String>,
}

/// 远程访问状态快照（app 的设备页用）。v0.6 新增 3 个字段，旧字段不变。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WebInfo {
    /// 配对链接：`<advertise_url>/pair#<code>`（code 放 fragment，不落服务端日志）。
    pub url: String,
    /// 当前配对码（一次性、带 TTL）。
    pub code: String,
    /// 配对码过期时间（Unix 毫秒）。
    pub expires_at_ms: i64,
    /// 配对二维码 PNG 的绝对路径（生成失败时为 None）。
    pub qr_png: Option<PathBuf>,
    /// 已信任设备列表。
    pub devices: Vec<TrustedDevice>,
    /// 实际监听地址，例如 `0.0.0.0:8787`（v0.6）。
    pub bind: String,
    /// 对外访问地址，例如 `http://192.168.1.10:8787`（v0.6）。
    pub advertise_url: String,
    /// 本机 LAN IP，例如 `192.168.1.10`（v0.6；探测失败时为空串）。
    pub lan_ip: String,
}

impl WebInfo {
    /// 配对码剩余有效时间（毫秒，可能为 0）。
    pub fn remaining_ms(&self) -> i64 {
        (self.expires_at_ms - neosonia_core::util::now_ms()).max(0)
    }

    /// 配对码是否还有效。
    pub fn is_pairing_active(&self) -> bool {
        self.remaining_ms() > 0
    }
}
