//! `WebServer`：启动/停止 axum 服务，路由装配，安全响应头。
//!
//! 由 `neosonia-app` 在 tokio runtime 内调用 [`WebServer::start`]；监听地址来自
//! `settings.remote.{bind,port}`（运行期热改后需要重启服务才生效）。

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use neosonia_core::{AgentEngine, AgentEvent, ConversationStore, Paths, Settings};
use parking_lot::{Mutex, RwLock};
use tokio::sync::{broadcast, watch};

use crate::devices::DeviceStore;
use crate::pairing::{PairingState, write_qr_png};
use crate::{WebConfig, WebInfo};

/// 对所有响应生效的 CSP / 安全头。
///
/// `style-src` 允许 `'unsafe-inline'`：`index.html` 里有一小段首屏关键 CSS
/// （底色 + 骨架屏动画），内联可以避免白屏闪烁；页面没有任何内联脚本/事件属性，
/// 用户可控内容一律经 `textContent` 或转义后插入，脚本只来自 `'self'`。
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
     script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; \
     connect-src 'self' ws: wss:; font-src 'self'; object-src 'none'; \
     base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// 服务内部共享状态（handler / 中间件都通过 `State<Arc<Inner>>` 访问）。
pub(crate) struct Inner {
    pub paths: Paths,
    pub settings: Arc<RwLock<Settings>>,
    pub engine: Arc<AgentEngine>,
    pub conversations: Arc<ConversationStore>,
    pub events: broadcast::Sender<AgentEvent>,
    pub devices: DeviceStore,
    pub pairing: PairingState,
    /// 实际监听地址（端口 0 时这里是系统分配的端口）。
    pub bound: SocketAddr,
    /// 对外访问根地址（LAN IP；QR/配对链接/API 都用它，不是 127.0.0.1）。
    pub advertise_base: String,
}

impl Inner {
    /// 对外展示的根 URL（`http://<lan_ip>:<port>`）。
    pub fn lan_base_url(&self) -> String {
        self.advertise_base.clone()
    }

    /// 实际监听地址文本，例如 `0.0.0.0:8787`。
    pub fn bind_text(&self) -> String {
        format!("{}:{}", self.bound.ip(), self.bound.port())
    }

    /// 本机 LAN IP 文本（advertise_url 的 host）；解析失败返回空串。
    pub fn advertise_host(&self) -> String {
        let rest = self
            .advertise_base
            .split("://")
            .nth(1)
            .unwrap_or("")
            .split('/')
            .next()
            .unwrap_or("");
        // 去掉 IPv6 的方括号与端口。
        if rest.starts_with('[') {
            let end = rest.find(']').unwrap_or(rest.len());
            rest[1..end].to_string()
        } else {
            rest.split(':').next().unwrap_or("").to_string()
        }
    }

    /// 配对链接（code 放 fragment，浏览器不会发给服务器）。
    pub fn pair_url(&self) -> String {
        format!("{}/pair#{}", self.lan_base_url(), self.pairing.view().code)
    }

    /// 二维码 PNG 的落盘路径。
    pub fn qr_path(&self) -> PathBuf {
        self.paths.qrcodes.join("pair.png")
    }
}

/// 远程访问服务句柄。`Drop` 时自动停止。
pub struct WebServer {
    inner: Arc<Inner>,
    shutdown: Mutex<Option<watch::Sender<bool>>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl WebServer {
    /// 启动服务。**必须在 tokio runtime 内调用**（绑定 + 注册 listener 需要 runtime）。
    pub fn start(cfg: WebConfig) -> anyhow::Result<WebServer> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            anyhow!("WebServer::start 必须在 tokio runtime 内调用（app 请用 tokio::runtime::Handle::block_on 或 spawn 后再启动）")
        })?;

        // 远程访问需要 qrcodes/ 等目录存在。
        cfg.paths.ensure().context("创建远程访问所需目录失败")?;

        let remote = cfg.settings.read().remote.clone();
        // v0.6：app 可以显式覆盖监听地址（手机连不上的根因是绑到了 127.0.0.1）。
        let bind_spec = crate::net::choose_bind(cfg.bind.as_deref(), &remote.bind);
        let requested = resolve_bind(&bind_spec, remote.port)?;
        let listener = std::net::TcpListener::bind(requested).with_context(|| {
            format!(
                "无法监听 {requested}：端口可能被占用或被系统禁止。请在设置页换一个端口，或检查防火墙。"
            )
        })?;
        listener
            .set_nonblocking(true)
            .context("设置监听 socket 为非阻塞失败")?;
        let bound = listener.local_addr().context("读取实际监听地址失败")?;
        let tokio_listener =
            tokio::net::TcpListener::from_std(listener).context("注册 tokio 监听器失败")?;
        // QR/配对链接/API 一律用这个地址：优先 app 传入的 advertise_url（LAN IP），
        // 否则按监听地址自动探测（绑 0.0.0.0 时取本机 LAN IP）。
        let advertise_base =
            crate::net::advertise_base(cfg.advertise_url.as_deref(), bound.ip(), bound.port());

        let devices = DeviceStore::load(&cfg.paths.devices_file);
        let pairing = PairingState::new(remote.pair_ttl_secs);
        let inner = Arc::new(Inner {
            paths: cfg.paths,
            settings: cfg.settings,
            engine: cfg.engine,
            conversations: cfg.conversations,
            events: cfg.events,
            devices,
            pairing,
            bound,
            advertise_base,
        });

        if let Err(e) = write_pair_qr(&inner) {
            tracing::warn!("生成配对二维码失败（不影响配对，只是没有二维码图片）：{e}");
        }

        // v0.7 收尾：后台预热厂商 API 状态（core 内部 60s 缓存 + 单飞）。
        // DeepSeek 状态页/TLS 首次可能 >3s，/api/me 用 3s 超时避免拖慢页面；
        // 预热任务保证缓存尽快就绪，小球随后出现。
        {
            let settings_for_status = inner.settings.clone();
            handle.spawn(async move {
                loop {
                    let snapshot = settings_for_status.read().clone();
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        neosonia_core::provider_status::current_status(&snapshot),
                    )
                    .await;
                    tokio::time::sleep(std::time::Duration::from_secs(50)).await;
                }
            });
        }

        let app = router(inner.clone());
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let task = handle.spawn(async move {
            let server = axum::serve(
                tokio_listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                loop {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    if shutdown_rx.changed().await.is_err() {
                        break;
                    }
                }
            });
            if let Err(e) = server.await {
                tracing::error!("远程访问服务异常退出：{e}");
            }
        });

        tracing::info!(
            addr = %bound,
            advertise = %inner.lan_base_url(),
            lan_only = remote.lan_only,
            "远程访问服务已启动"
        );

        Ok(WebServer {
            inner,
            shutdown: Mutex::new(Some(shutdown_tx)),
            task: Mutex::new(Some(task)),
        })
    }

    /// 当前状态快照（app 设备页显示二维码/配对码/设备列表）。
    pub fn info(&self) -> WebInfo {
        let view = self.inner.pairing.view();
        let qr = self.inner.qr_path();
        WebInfo {
            url: format!("{}/pair#{}", self.inner.lan_base_url(), view.code),
            code: view.code,
            expires_at_ms: view.expires_at_ms,
            qr_png: if qr.is_file() { Some(qr) } else { None },
            devices: self.devices(),
            bind: self.inner.bind_text(),
            advertise_url: self.inner.lan_base_url(),
            lan_ip: self.inner.advertise_host(),
        }
    }

    /// 换一个新配对码并重写二维码。
    pub fn refresh_pairing(&self) -> WebInfo {
        let ttl = self.inner.settings.read().remote.pair_ttl_secs;
        self.inner.pairing.rotate(ttl);
        if let Err(e) = write_pair_qr(&self.inner) {
            // 写失败时删掉旧二维码，避免 app 显示一个已经失效的码。
            let _ = std::fs::remove_file(self.inner.qr_path());
            tracing::warn!("刷新配对二维码失败：{e}");
        }
        self.info()
    }

    /// 已信任设备列表。
    pub fn devices(&self) -> Vec<neosonia_core::TrustedDevice> {
        self.inner.devices.list()
    }

    /// 重命名一台已信任设备（与远程 `POST /api/devices/{id}/name` 走同一个 store）。
    pub fn rename_device(&self, device_id: &str, name: &str) -> Option<neosonia_core::TrustedDevice> {
        self.inner.devices.rename(device_id, name)
    }

    /// 撤销一台设备；返回是否真的删除了。
    pub fn revoke(&self, device_id: &str) -> bool {
        self.inner.devices.revoke(device_id)
    }

    /// 停止服务（幂等）。
    pub fn stop(&self) {
        if let Some(tx) = self.shutdown.lock().take() {
            let _ = tx.send(true);
        }
        if let Some(task) = self.task.lock().take() {
            task.abort();
        }
        tracing::info!("远程访问服务已停止");
    }

    /// 局域网访问根地址（`http://<lan_ip>:<port>`，与 QR 码一致）。
    pub fn lan_url(&self) -> String {
        self.inner.lan_base_url()
    }

    /// 对外访问地址（LAN IP 版本，不含尾斜杠）。
    pub fn advertise_url(&self) -> String {
        self.inner.lan_base_url()
    }

    /// 实际监听地址文本，例如 `0.0.0.0:8787`。
    pub fn bind_text(&self) -> String {
        self.inner.bind_text()
    }

    /// 实际监听端口。
    pub fn port(&self) -> u16 {
        self.inner.bound.port()
    }

    /// 实际监听地址。
    pub fn bound_addr(&self) -> SocketAddr {
        self.inner.bound
    }

    /// 服务是否还在运行。
    pub fn is_running(&self) -> bool {
        self.task
            .lock()
            .as_ref()
            .map(|t| !t.is_finished())
            .unwrap_or(false)
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn write_pair_qr(inner: &Inner) -> anyhow::Result<PathBuf> {
    let path = inner.qr_path();
    write_qr_png(&path, &inner.pair_url())?;
    Ok(path)
}

/// 把 `remote.bind` 解析成可监听的 `SocketAddr`（支持空串/`0.0.0.0`/IPv6/主机名）。
fn resolve_bind(bind: &str, port: u16) -> anyhow::Result<SocketAddr> {
    let raw = bind.trim();
    let raw = if raw.is_empty() { "0.0.0.0" } else { raw };
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let host = if raw.contains(':') { raw.to_string() } else { format!("{raw}:{port}") };
    let mut addrs = host.to_socket_addrs().map_err(|e| {
        anyhow!("监听地址「{raw}」不合法：{e}。请在设置页填写 IP（例如 0.0.0.0）或主机名。")
    })?;
    addrs
        .next()
        .ok_or_else(|| anyhow!("监听地址「{raw}」解析不到任何地址。"))
}

/// 装配路由。`pub(crate)` 以便探针/测试复用。
pub(crate) fn router(inner: Arc<Inner>) -> Router {
    let public = Router::new()
        .route("/", get(crate::assets::index))
        .route("/pair", get(crate::assets::index))
        .route("/app.js", get(crate::assets::app_js))
        .route("/spa-core.js", get(crate::assets::spa_core_js))
        .route("/md-core.js", get(crate::assets::md_core_js))
        .route("/logo-64.png", get(crate::assets::logo_png))
        .route("/style.css", get(crate::assets::style_css))
        .route("/healthz", get(crate::api::healthz))
        .route("/favicon.ico", get(favicon))
        .route("/api/pair", post(crate::api::pair))
        .with_state(inner.clone());

    let protected = Router::new()
        .route("/api/me", get(crate::api::me))
        .route("/api/devices", get(crate::api::devices))
        .route("/api/devices/{id}/name", post(crate::api::rename_device))
        // v0.7：附件上传走原始字节 body（不引入 multipart），单文件放宽到 25MB。
        .route(
            "/api/upload",
            post(crate::api::upload).layer(axum::extract::DefaultBodyLimit::max(
                crate::uploads::MAX_UPLOAD_BYTES + 1024 * 1024,
            )),
        )
        .route("/api/uploads/{id}", get(crate::api::upload_get))
        .route("/api/sessions", get(crate::api::sessions))
        .route("/api/sessions/{id}", get(crate::api::session_messages))
        .route("/api/sessions/{id}/delete", post(crate::api::session_delete))
        .route("/api/search", get(crate::api::search))
        .route("/api/review/pending", get(crate::api::review_pending))
        .route("/api/chat", post(crate::api::chat))
        .route("/api/new", post(crate::api::new_conversation))
        .route("/api/screenshot/{name}", get(crate::api::screenshot))
        .layer(middleware::from_fn_with_state(
            inner.clone(),
            crate::auth::require_device,
        ))
        .with_state(inner.clone());

    let websocket = Router::new()
        .route("/ws", get(crate::ws::ws_handler))
        .with_state(inner.clone());

    Router::new()
        .merge(public)
        .merge(protected)
        .merge(websocket)
        .fallback(not_found)
        // 内层：安全响应头。
        .layer(middleware::from_fn(security_headers))
        // 外层：局域网来源校验（最后加的层在最外面）。
        .layer(middleware::from_fn_with_state(
            inner,
            crate::auth::lan_guard,
        ))
}

async fn not_found() -> Response {
    crate::auth::reject(StatusCode::NOT_FOUND, "接口不存在。")
}

async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("geolocation=(), microphone=(), camera=()"),
    );
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn resolve_bind_variants() {
        assert_eq!(
            resolve_bind("", 8787).unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8787)
        );
        assert_eq!(
            resolve_bind("127.0.0.1", 9000).unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9000)
        );
        assert_eq!(
            resolve_bind("::1", 9000).unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 9000)
        );
        assert_eq!(
            resolve_bind("0.0.0.0:9100", 1).unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9100)
        );
        assert!(resolve_bind("这不是地址", 8787).is_err());
    }

    #[test]
    fn bind_port_zero_gets_real_port() {
        // 只验证解析逻辑：端口 0 交给系统分配，start 时再 local_addr。
        let addr = resolve_bind("127.0.0.1", 0).unwrap();
        assert_eq!(addr.port(), 0);
    }

    #[test]
    fn security_headers_value_is_valid() {
        assert!(CONTENT_SECURITY_POLICY.contains("connect-src 'self' ws: wss:"));
        assert!(CONTENT_SECURITY_POLICY.contains("frame-ancestors 'none'"));
        // favicon 是 data URI，所以 img-src 必须允许 data:。
        assert!(CONTENT_SECURITY_POLICY.contains("img-src 'self' data: blob:"));
        // 首屏关键 CSS 内联在 index.html，需要 style-src 允许 inline；脚本仍只允许 'self'。
        assert!(CONTENT_SECURITY_POLICY.contains("style-src 'self' 'unsafe-inline'"));
        assert!(CONTENT_SECURITY_POLICY.contains("script-src 'self'"));
        assert!(!CONTENT_SECURITY_POLICY.contains("script-src 'self' 'unsafe-inline'"));
    }
}
