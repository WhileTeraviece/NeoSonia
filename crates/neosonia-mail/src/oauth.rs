//! Thunderbird 式邮箱 OAuth2 登录（Outlook device code / Gmail loopback PKCE）+ XOAUTH2。
//!
//! 为什么这样做：
//! - **Microsoft（Outlook / Microsoft 365）**：用 OAuth2 **device code flow**，
//!   不需要本地回调服务器，也不需要把 client_secret 放进桌面程序：
//!   `POST .../devicecode` 拿到 `user_code` + `verification_uri` 给用户看，
//!   同时后台按 `interval` 轮询 `.../token`，用户授权完成后拿到 refresh_token。
//! - **Google（Gmail）**：用 **loopback + PKCE**（像 Thunderbird 的桌面客户端那样）：
//!   在 `127.0.0.1` 随机端口起一个一次性 HTTP 回调服务器，浏览器授权后 Google 会
//!   redirect 回这个端口，再用 code + code_verifier 换 token。Google 桌面客户端
//!   允许任意 loopback 端口，所以随机端口是官方支持的做法。
//! - **Custom**：任意标准 OAuth2 授权码 + loopback PKCE（自填 auth_url/token_url）。
//!
//! 内置的 client_id 来自 Thunderbird 开源项目（它们的桌面客户端注册是公开的），
//! 详见 `crates/neosonia-mail/README.md`；配置里的 `mail.oauth.client_id` 可以覆盖。
//!
//! 本模块**不读写 Settings**：`start_flow` 只做网络流程 + 返回 [`OAuthResult`]，
//! 由 app 层把 refresh_token / access_token / expires_at_ms / email 写回 settings。
//! 之后 `OAuth::from_settings(&settings.mail.oauth)` + `access_token()` 负责带缓存刷新。
//!
//! 所有网络操作都是阻塞实现，调用方放进 `tokio::task::spawn_blocking`；本模块的
//! 异步入口（`start_flow` / `OAuthFlow::wait`）内部自己处理线程池，不会阻塞 UI。
//! 错误全中文、零 emoji、不 panic。

use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use base64::Engine as _;
use mailledger_oauth::{
    AuthorizationCodeFlow, Error as MlError, OAuthClient as MlOAuthClient, Provider as MlProvider,
    Token as MlToken,
};
use neosonia_core::config::{MailOAuthSettings, OAuthProvider};
use neosonia_core::util::now_ms;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

// ============================================================
// 常量 / 默认值
// ============================================================

/// Microsoft Office 的公共客户端 ID（广为人知、公开的 Microsoft 第一方 public client）。
///
/// 默认使用 Thunderbird 的公开桌面客户端 id（Microsoft 官方允许的 public client；授权页显示 Thunderbird）。
/// 想显示自己的应用名，请在设置里填写自己 Azure 应用的 Application (client) ID。
/// 该 ID 由 Microsoft 维护，可能被调整/限流；用于公共客户端 device code 流程，无需 client_secret。
pub const MICROSOFT_CLIENT_ID: &str = "9e5f94bc-e8a4-4e73-b8be-63364c29d753";

/// 历史版本内置过的 Thunderbird 公开 client_id（已弃用；用于迁移旧配置，不能再出现在授权页）。
pub const LEGACY_THUNDERBIRD_MICROSOFT_CLIENT_ID: &str =
    "9e5f94bc-e8a4-4e73-b8be-63364c29d753";
/// 历史版本内置过的 Thunderbird Google 公开 client_id（已弃用；用于迁移旧配置）。
pub const LEGACY_THUNDERBIRD_GOOGLE_CLIENT_ID: &str =
    "406964657835-aq8lmia8j95dhl1a2bvharmfk3t1hgqj.apps.googleusercontent.com";

/// 是否是历史遗留的 Thunderbird client_id（迁移时清掉，避免继续显示 Thunderbird）。
pub fn is_legacy_thunderbird_client_id(client_id: &str) -> bool {
    let value = client_id.trim();
    value == LEGACY_THUNDERBIRD_MICROSOFT_CLIENT_ID || value == LEGACY_THUNDERBIRD_GOOGLE_CLIENT_ID
}

/// client_id 为空且对应 provider 没有默认值时的提示（Microsoft 有默认公共客户端，不报这个错）。
pub const CLIENT_ID_MISSING_HINT: &str =
    "未配置 OAuth2 client_id：Google 请先在设置页「高级」里填写自建「桌面应用」OAuth 客户端的 \
     client_id（和 client_secret）；Custom 请填写自己的 OAuth 客户端 ID";

/// Microsoft 邮箱默认 scope（IMAP + SMTP + offline_access；openid 用于 id_token，库会忽略但不影响）。
pub const MICROSOFT_SCOPE: &str = "openid profile email offline_access \
     https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send";
/// Gmail 默认 scope（mail + openid）。
pub const GOOGLE_SCOPE: &str = "https://mail.google.com/ openid email profile";

/// Microsoft OAuth2 v2.0 授权/令牌端点前缀（tenant 自行替换）。
const MICROSOFT_ENDPOINT_BASE: &str = "https://login.microsoftonline.com";
/// Google 授权端点。
const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google 令牌端点。
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// access_token 剩余有效期少于这个值时提前刷新（毫秒）。
pub const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;
/// 默认 HTTP 超时（秒）。
const HTTP_TIMEOUT_SECS: u64 = 30;
/// loopback 授权流程默认等待时长（秒）。
const LOOPBACK_EXPIRES_SECS: u64 = 300;
/// device code 流程的保底最长等待（秒）。
const DEVICE_CODE_MAX_WAIT_SECS: u64 = 900;

// ============================================================
// 公共数据结构
// ============================================================

/// 一次成功授权/刷新得到的结果。app 负责把它写回 `settings.mail.oauth`。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OAuthResult {
    /// 邮箱地址（从 id_token / userinfo 里解出来；拿不到时可能为空）。
    pub email: String,
    /// 长期刷新令牌（写回 settings，不要打印到日志）。
    pub refresh_token: String,
    /// 访问令牌。
    pub access_token: String,
    /// access_token 过期时间（Unix 毫秒）。
    pub expires_at_ms: i64,
    /// 服务端返回的 scope（可能为空）。
    pub scope: String,
    /// 通常是 Bearer。
    pub token_type: String,
}

/// `OAuth` 当前持有的令牌快照（给 app / UI 用）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: i64,
    pub email: String,
    pub scope: String,
}

/// 一次交互式授权的句柄。
///
/// - Microsoft：`authorize_url` = 浏览器要打开的 `verification_uri`，`user_code` 有值；
/// - Google/Custom：`authorize_url` = 完整授权 URL（含 PKCE），`user_code` 为 None，
///   授权完成后浏览器会被 redirect 到本机 loopback 端口。
pub struct OAuthFlow {
    /// 浏览器要打开的地址。
    pub authorize_url: String,
    /// device code 流程需要展示给用户的码（Google 为 None）。
    pub user_code: Option<String>,
    /// 这次授权最长多少秒内有效。
    pub expires_in: u64,
    /// 等待用户完成授权；返回 (email, refresh_token, access_token, expires_at_ms)。
    pub wait: JoinHandle<anyhow::Result<OAuthResult>>,
    /// loopback 流程的回调地址（Google/Custom 才有）。
    pub redirect_uri: Option<String>,
    /// 本次使用的提供方。
    pub provider: OAuthProvider,
    /// 本次使用的 client_id（app 可写回 settings）。
    pub client_id: String,
    /// 本次请求的 scope（app 可写回 settings）。
    pub scope: String,
}

impl OAuthFlow {
    /// 放弃等待（关闭后台轮询/回调服务器）。
    pub fn abort(&self) {
        self.wait.abort();
    }
}

impl std::fmt::Debug for OAuthFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthFlow")
            .field("authorize_url", &self.authorize_url)
            .field("user_code", &self.user_code)
            .field("expires_in", &self.expires_in)
            .field("redirect_uri", &self.redirect_uri)
            .field("provider", &self.provider)
            .field("client_id", &self.client_id)
            .finish()
    }
}

/// Microsoft device code 接口返回的信息（测试/诊断用）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeviceCodeInfo {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    pub message: String,
}

// ============================================================
// XOAUTH2 认证字符串
// ============================================================

/// 构造 IMAP/SMTP 的 XOAUTH2 初始响应：
/// `user=<user>\x01auth=Bearer <token>\x01\x01`。
pub fn build_xoauth2(user: &str, token: &str) -> String {
    // mailledger-oauth 的 SASL 助手返回 base64（RFC 7628/XOAUTH2 初始响应），
    // 而 imap/lettre 的 AUTHENTICATE 由它们自己 base64，所以这里解码回原始字符串。
    let encoded = mailledger_oauth::sasl::xoauth2_response(user, token);
    match base64::engine::general_purpose::STANDARD.decode(encoded.as_bytes()) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => format!("user={user}\x01auth=Bearer {token}\x01\x01"),
    }
}

/// `imap` crate 的 XOAUTH2 认证器。
///
/// 第一次 challenge 返回 `user=...\x01auth=Bearer ...\x01\x01`；
/// 如果服务器再发 challenge（通常表示令牌无效），第二次返回空串让服务器直接
/// 返回最终错误，而不是把令牌重复发一遍。
pub struct XOAuth2Authenticator {
    user: String,
    token: String,
    responded: std::sync::atomic::AtomicBool,
}

impl XOAuth2Authenticator {
    pub fn new(user: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            token: token.into(),
            responded: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl imap::Authenticator for XOAuth2Authenticator {
    type Response = String;

    fn process(&self, _challenge: &[u8]) -> Self::Response {
        use std::sync::atomic::Ordering;
        if self.responded.swap(true, Ordering::SeqCst) {
            String::new()
        } else {
            build_xoauth2(&self.user, &self.token)
        }
    }
}

// ============================================================
// PKCE / 编码 / JWT 小工具（纯函数，可单测）
// ============================================================

/// 生成 PKCE code_verifier（43 个 base64url 字符，来自 32 字节随机数）。
pub fn pkce_verifier() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64_url_encode(&bytes)
}

/// PKCE S256：`BASE64URL(SHA256(verifier))`。
pub fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64_url_encode(&digest)
}

fn base64_url_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn base64_url_decode(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(text))
        .ok()
}

/// `application/x-www-form-urlencoded` 编码（空格用 %20，符合 OAuth 的 RFC3986 要求）。
pub fn form_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 从 URL query 里取一个参数（不做百分号解码之外的花活，够授权 URL 用）。
pub fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 从 JWT 的 payload 里解出某个 claim（不校验签名，只用于读 id_token 里的邮箱）。
pub fn jwt_claim(id_token: &str, claims: &[&str]) -> Option<String> {
    let json = jwt_payload(id_token)?;
    for claim in claims {
        if let Some(value) = json.get(*claim).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// 从 id_token 里取邮箱地址（Microsoft: preferred_username/upn；Google: email）。
pub fn jwt_email(id_token: &str) -> Option<String> {
    jwt_claim(
        id_token,
        &["email", "preferred_username", "upn", "unique_name"],
    )
}

// ============================================================
// 默认值解析
// ============================================================

/// 返回实际使用的 client_id：配置优先；Microsoft 为空时回退到 Microsoft Office 公共客户端 ID；
/// Google/Custom 没有默认值（必须用户自建）。
pub fn resolved_client_id(cfg: &MailOAuthSettings) -> String {
    let configured = cfg.client_id.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    if !configured.is_empty() {
        tracing::warn!(
            "client_id 为空：Microsoft 使用内置 Thunderbird 公开客户端 id（授权页显示 Thunderbird）"
        );
    }
    match cfg.provider {
        OAuthProvider::Microsoft => MICROSOFT_CLIENT_ID.to_string(),
        OAuthProvider::Google | OAuthProvider::Custom => String::new(),
    }
}

/// 没配置 scope 时按 provider 用默认 scope。
///
/// Microsoft 额外保证 scope 里一定有 `offline_access`：否则 token 响应不会带 refresh_token
/// （历史配置里存的 scope 可能来自 token 响应的 granted scope，常常不含 offline_access）。
pub fn resolved_scope(cfg: &MailOAuthSettings) -> String {
    let configured = cfg.scope.trim();
    let mut scope = if !configured.is_empty() {
        configured.to_string()
    } else {
        match cfg.provider {
            OAuthProvider::Microsoft => MICROSOFT_SCOPE.to_string(),
            OAuthProvider::Google => GOOGLE_SCOPE.to_string(),
            OAuthProvider::Custom => String::new(),
        }
    };
    if cfg.provider == OAuthProvider::Microsoft
        && !scope.split_whitespace().any(|s| s == "offline_access")
    {
        static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if WARNED.set(()).is_ok() {
            tracing::warn!(
                original_scope = %scope,
                "Microsoft OAuth scope 缺少 offline_access，已自动追加（否则拿不到 refresh_token）"
            );
        }
        if !scope.is_empty() {
            scope.push(' ');
        }
        scope.push_str("offline_access");
    }
    scope
}

fn microsoft_tenant(cfg: &MailOAuthSettings) -> String {
    let tenant = cfg.tenant.trim();
    if tenant.is_empty() {
        "common".to_string()
    } else {
        tenant.to_string()
    }
}

fn microsoft_endpoint_base(cfg: &MailOAuthSettings) -> String {
    format!(
        "{MICROSOFT_ENDPOINT_BASE}/{}/oauth2/v2.0",
        microsoft_tenant(cfg)
    )
}

/// 解析授权端点：
/// - Microsoft：device code 请求地址（可用 `auth_url` 覆盖）；
/// - Google：授权码端点；
/// - Custom：必须自己填 `auth_url`。
pub fn resolved_auth_url(cfg: &MailOAuthSettings) -> String {
    let configured = cfg.auth_url.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    match cfg.provider {
        OAuthProvider::Microsoft => format!("{}/devicecode", microsoft_endpoint_base(cfg)),
        OAuthProvider::Google => GOOGLE_AUTH_URL.to_string(),
        OAuthProvider::Custom => String::new(),
    }
}

/// 解析令牌端点（Microsoft/Google 有默认值；Custom 必填）。
pub fn resolved_token_url(cfg: &MailOAuthSettings) -> String {
    let configured = cfg.token_url.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    match cfg.provider {
        OAuthProvider::Microsoft => format!("{}/token", microsoft_endpoint_base(cfg)),
        OAuthProvider::Google => GOOGLE_TOKEN_URL.to_string(),
        OAuthProvider::Custom => String::new(),
    }
}

/// Microsoft 的授权码端点（device flow 用 devicecode，这里保留 authorize 给适配层）。
fn microsoft_authorize_url(cfg: &MailOAuthSettings) -> String {
    format!("{}/authorize", microsoft_endpoint_base(cfg))
}

/// 拆 scope 字符串成库需要的 `Vec<String>`。
fn split_scopes(scope: &str) -> Vec<String> {
    scope
        .split_whitespace()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 邮箱兜底：只从 OAuth 配置里的 email 取；app 在发起授权前可用 imap.user/smtp.user
/// 补到 `settings.mail.oauth.email`（OAuth 层拿不到 MailSettings 的其它字段）。
fn fallback_email(cfg: &MailOAuthSettings) -> String {
    cfg.email.trim().to_string()
}

/// 从 access_token JWT 里取邮箱（Microsoft IMAP token 常带 upn/unique_name）。
fn email_from_access_token(access_token: &str) -> Option<String> {
    jwt_claim(
        access_token,
        &[
            "upn",
            "unique_name",
            "email",
            "preferred_username",
            "smtp",
        ],
    )
}

// ============================================================
// OAuth（保存 token、按需刷新）
// ============================================================

#[derive(Clone, Debug)]
struct OAuthState {
    provider: OAuthProvider,
    client_id: String,
    client_secret: String,
    scope: String,
    token_url: String,
    access_token: String,
    refresh_token: String,
    expires_at_ms: i64,
    email: String,
    /// 仅在证书校验失败时允许降级为不校验证书重试一次（默认 false）。
    allow_insecure_tls: bool,
}

/// 保存一份 OAuth 配置 + token 的轻量对象；`access_token()` 会按需刷新。
///
/// app 的典型用法：
/// ```ignore
/// // 1) 交互式授权（app 层）
/// let flow = OAuth::start_flow(OAuthProvider::Microsoft, "", "common").await?;
/// open::that(&flow.authorize_url);
/// let result = flow.wait.await???;
/// // 2) 把 result 写进 settings.mail.oauth（refresh_token/access_token/expires_at_ms/email）
/// // 3) 之后每次发信/收信前（或者由 MailService 自动）：
/// let mut oauth = OAuth::from_settings(&settings.mail.oauth);
/// let token = oauth.access_token()?;
/// ```
pub struct OAuth {
    state: Mutex<OAuthState>,
}

impl Default for OAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuth {
    /// 空配置（默认端点、无 client_id），没有 refresh_token。
    pub fn new() -> Self {
        let cfg = MailOAuthSettings::default();
        Self::from_settings(&cfg)
    }

    /// 从 `settings.mail.oauth` 构造。
    pub fn from_settings(cfg: &MailOAuthSettings) -> Self {
        let client_id = resolved_client_id(cfg);
        let scope = resolved_scope(cfg);
        let token_url = resolved_token_url(cfg);
        Self {
            state: Mutex::new(OAuthState {
                provider: cfg.provider,
                client_id,
                client_secret: cfg.client_secret.trim().to_string(),
                scope,
                token_url,
                access_token: cfg.access_token.trim().to_string(),
                refresh_token: cfg.refresh_token.trim().to_string(),
                expires_at_ms: cfg.expires_at_ms,
                email: cfg.email.trim().to_string(),
                allow_insecure_tls: cfg.allow_insecure_tls,
            }),
        }
    }

    /// 令牌快照（app 用来判断是否已登录/什么时候过期）。
    pub fn snapshot(&self) -> OAuthTokens {
        let state = self.state.lock();
        OAuthTokens {
            access_token: state.access_token.clone(),
            refresh_token: state.refresh_token.clone(),
            expires_at_ms: state.expires_at_ms,
            email: state.email.clone(),
            scope: state.scope.clone(),
        }
    }

    /// 把一次授权/刷新的结果合并进当前对象（主要给测试和高级调用方用）。
    pub fn apply_result(&self, result: &OAuthResult) {
        let mut state = self.state.lock();
        apply_result_to_state(&mut state, result);
        clear_needs_reauth();
    }

    /// 是否已经拿到过 refresh_token（能自动续期）。
    pub fn has_refresh_token(&self) -> bool {
        !self.state.lock().refresh_token.trim().is_empty()
    }

    /// access_token 是否还在有效期内（留 5 分钟余量）。
    pub fn access_token_valid(&self) -> bool {
        let state = self.state.lock();
        !state.access_token.trim().is_empty()
            && state.expires_at_ms > now_ms().saturating_add(REFRESH_MARGIN_MS)
    }

    /// 拿 access_token：缓存还有效就直接返回；否则用 refresh_token 换新的。
    ///
    /// 这个方法是**阻塞**的（内部走阻塞 HTTP），请放在 `spawn_blocking` 里调用。
    pub fn access_token(&mut self) -> anyhow::Result<String> {
        self.access_token_impl(false)
    }

    /// 强制刷新（忽略缓存）：IMAP/SMTP 用旧 token 认证失败后重试用。
    pub fn access_token_force(&self) -> anyhow::Result<String> {
        self.access_token_impl(true)
    }

    /// 当前使用的 OAuth 提供方。
    pub fn provider(&self) -> OAuthProvider {
        self.state.lock().provider
    }

    /// 当前客户端 ID（只读，用于诊断时做掩码显示）。
    pub fn client_id(&self) -> String {
        self.state.lock().client_id.clone()
    }

    fn access_token_impl(&self, force: bool) -> anyhow::Result<String> {
        let cached = {
            let state = self.state.lock();
            if !force
                && !state.access_token.trim().is_empty()
                && state.expires_at_ms > now_ms().saturating_add(REFRESH_MARGIN_MS)
            {
                return Ok(state.access_token.clone());
            }
            state.clone()
        };
        if cached.refresh_token.trim().is_empty() {
            bail!(
                "OAuth2 登录信息已缺失或过期：settings.mail.oauth.refresh_token 为空。\
                 请在设置页点「用 Microsoft 登录 / 用 Google 登录」重新完成一次授权。"
            );
        }
        if cached.token_url.trim().is_empty() {
            bail!("OAuth2 令牌端点为空：请检查 settings.mail.oauth.token_url（Custom 提供方必须填写）");
        }

        // 主线：mailledger-oauth 的 OAuthClient::refresh_token（async，内部 reqwest）。
        let result = match refresh_with_mailledger(&cached) {
            Ok(result) => result,
            Err(library_error) => {
                if cached.allow_insecure_tls && looks_like_tls_error(&library_error.to_string()) {
                    tracing::warn!(
                        error = %library_error,
                        "mailledger-oauth 刷新失败（TLS/连接类），按 allow_insecure_tls=true 降级到本机 reqwest 重试一次"
                    );
                    refresh_with_http_fallback(&cached).map_err(|fallback_error| {
                        anyhow!(
                            "{library_error}\n已按 mail.oauth.allow_insecure_tls=true 关闭证书校验重试，仍失败：{fallback_error}"
                        )
                    })?
                } else {
                    return Err(library_error);
                }
            }
        };
        clear_needs_reauth();

        let mut state = self.state.lock();
        apply_result_to_state(&mut state, &result);
        if result.refresh_token.trim().is_empty() {
            // 刷新响应通常不回 refresh_token，保留旧的。
            state.refresh_token = cached.refresh_token.clone();
        }
        if state.email.trim().is_empty() {
            state.email = cached.email.clone();
        }
        Ok(state.access_token.clone())
    }
}

/// 用 mailledger-oauth 的 `OAuthClient::refresh_token` 刷新（async → 独立线程 runtime）。
fn refresh_with_mailledger(cached: &OAuthState) -> anyhow::Result<OAuthResult> {
    let client = ml_client_for_values(
        &cached.client_id,
        &cached.client_secret,
        cached.provider,
        &cached.token_url,
        &cached.token_url, // refresh 只需要 token_url；auth_url 用一个合法地址占位
        None,
        &cached.scope,
    )?;
    let refresh_token = cached.refresh_token.clone();
    let token = MlToken {
        access_token: cached.access_token.clone(),
        token_type: "Bearer".to_string(),
        expires_at: None,
        refresh_token: Some(refresh_token),
        scope: if cached.scope.trim().is_empty() {
            None
        } else {
            Some(cached.scope.clone())
        },
    };
    let timeout = http_timeout();
    let output = run_async_blocking(async move {
        tokio::time::timeout(timeout, client.refresh_token(&token)).await
    })?;
    let token = match output {
        Ok(Ok(token)) => token,
        Ok(Err(e)) => return Err(ml_error_to_anyhow("刷新 OAuth2 access_token", &e)),
        Err(_) => bail!("刷新 OAuth2 access_token 超时（{} 秒）", HTTP_TIMEOUT_SECS),
    };
    Ok(token_to_result(
        &token,
        cached.provider,
        &cached.email,
        &cached.scope,
    ))
}

/// 降级路径：本机 reqwest blocking（仅在 allow_insecure_tls=true 且 TLS/连接类失败时使用）。
fn refresh_with_http_fallback(cached: &OAuthState) -> anyhow::Result<OAuthResult> {
    let form = build_refresh_form(
        cached.provider,
        &cached.client_id,
        &cached.client_secret,
        &cached.refresh_token,
        &cached.scope,
    );
    let response = http_post_form(&cached.token_url, &form, http_timeout(), true)
        .map_err(|e| anyhow!("刷新 OAuth2 access_token 失败：{e}"))?;
    if !(200..300).contains(&response.status) {
        bail!(
            "刷新 OAuth2 access_token 被拒绝（HTTP {}）：{}",
            response.status,
            truncate_body(&response.body)
        );
    }
    token_result_from_body_opts(&response.body, cached.provider, true)
        .map_err(|e| anyhow!("刷新 OAuth2 access_token 失败：{e}"))
}

fn looks_like_tls_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("tls")
        || lower.contains("certificate")
        || lower.contains("handshake")
        || lower.contains("connect")
        || lower.contains("connection")
        || lower.contains("sending request")
}

/// 在独立线程的 current_thread runtime 上跑 async 库调用：
/// service 的调用点在 `spawn_blocking`/普通线程里，直接 `block_on` 会 panic。
fn run_async_blocking<F, T>(future: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<T>>();
    std::thread::Builder::new()
        .name("neosonia-oauth-lib".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    let _ = tx.send(Err(anyhow!("创建 OAuth 库使用的 tokio runtime 失败：{e}")));
                    return;
                }
            };
            let _ = tx.send(Ok(runtime.block_on(future)));
        })
        .map_err(|e| anyhow!("启动 OAuth 后台线程失败：{e}"))?;
    match rx.recv() {
        Ok(result) => result,
        Err(_) => bail!("OAuth 后台线程异常结束"),
    }
}

fn ml_error_to_anyhow(action: &str, error: &MlError) -> anyhow::Error {
    match error {
        MlError::OAuth { error, description } => {
            if error == "invalid_grant" {
                mark_needs_reauth(description);
                anyhow!(
                    "{action}失败：授权已失效（invalid_grant）：{description}。\
                     请在设置页重新点「用 Microsoft 登录 / 用 Google 登录」完成授权。"
                )
            } else if error == "access_denied" {
                anyhow!("{action}失败：用户拒绝了授权，或授权被管理员阻止")
            } else {
                anyhow!("{action}失败：OAuth 服务端返回 {error}：{description}")
            }
        }
        MlError::AccessDenied => anyhow!("{action}失败：用户拒绝了授权"),
        MlError::TokenExpired => anyhow!("{action}失败：授权码/设备码已过期，请重新登录"),
        MlError::Timeout(secs) => anyhow!("{action}超时（{secs} 秒）"),
        MlError::NoRefreshToken => anyhow!("{action}失败：没有 refresh_token，请重新授权"),
        MlError::InvalidConfig(message) => anyhow!("{action}失败：OAuth 配置不正确：{message}"),
        MlError::Http(http_error) => anyhow!(
            "{action}失败（HTTP/TLS 请求）：{}",
            reqwest_error_detail(http_error)
        ),
        other => anyhow!("{action}失败：{other}"),
    }
}

fn token_to_result(
    token: &MlToken,
    provider: OAuthProvider,
    fallback_email: &str,
    requested_scope: &str,
) -> OAuthResult {
    let email = {
        let fallback = fallback_email.trim();
        if !fallback.is_empty() {
            fallback.to_string()
        } else {
            email_from_access_token(&token.access_token).unwrap_or_default()
        }
    };
    let expires_at_ms = token
        .expires_at
        .map(|at| at.timestamp_millis())
        .unwrap_or_else(|| now_ms() + 3_600_000);
    let scope = token
        .scope
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| requested_scope.trim().to_string());
    let mut result = OAuthResult {
        email,
        refresh_token: token.refresh_token.clone().unwrap_or_default(),
        access_token: token.access_token.clone(),
        expires_at_ms,
        scope,
        token_type: token.token_type.clone(),
    };
    if result.email.is_empty() {
        result.email = String::new(); // app 侧会提示用户先填邮箱
    }
    let _ = provider;
    result
}

/// 按配置构造 mailledger 的 Provider（端点/scope 全部来自 settings）。
fn ml_provider_for_values(
    provider: OAuthProvider,
    auth_url: &str,
    token_url: &str,
    device_url: Option<&str>,
    scope: &str,
) -> anyhow::Result<MlProvider> {
    let name = match provider {
        OAuthProvider::Microsoft => "Microsoft",
        OAuthProvider::Google => "Google",
        OAuthProvider::Custom => "Custom",
    };
    let mut provider_cfg = MlProvider::new(name, auth_url, token_url)
        .map_err(|e| anyhow!("构造 OAuth Provider 失败：{e}"))?;
    if let Some(device_url) = device_url {
        provider_cfg = provider_cfg
            .with_device_auth_url(device_url)
            .map_err(|e| anyhow!("设置 OAuth device 端点失败：{e}"))?;
    }
    Ok(provider_cfg.with_default_scopes(split_scopes(scope)))
}

fn ml_client_for_values(
    client_id: &str,
    client_secret: &str,
    provider: OAuthProvider,
    auth_url: &str,
    token_url: &str,
    device_url: Option<&str>,
    scope: &str,
) -> anyhow::Result<MlOAuthClient> {
    if client_id.trim().is_empty() {
        bail!("{CLIENT_ID_MISSING_HINT}");
    }
    let provider_cfg = ml_provider_for_values(provider, auth_url, token_url, device_url, scope)?;
    let client = MlOAuthClient::new(client_id.trim(), provider_cfg);
    let client = if client_secret.trim().is_empty() {
        client
    } else {
        client.with_client_secret(client_secret.trim())
    };
    Ok(client)
}

/// 从 settings 构造库 provider（交互式流程用；Microsoft 带 device 端点）。
fn ml_provider_for_settings(cfg: &MailOAuthSettings) -> anyhow::Result<MlProvider> {
    let scope = resolved_scope(cfg);
    let token_url = resolved_token_url(cfg);
    match cfg.provider {
        OAuthProvider::Microsoft => ml_provider_for_values(
            cfg.provider,
            &microsoft_authorize_url(cfg),
            &token_url,
            Some(&resolved_auth_url(cfg)),
            &scope,
        ),
        OAuthProvider::Google => ml_provider_for_values(
            cfg.provider,
            &resolved_auth_url(cfg),
            &token_url,
            None,
            &scope,
        ),
        OAuthProvider::Custom => ml_provider_for_values(
            cfg.provider,
            &resolved_auth_url(cfg),
            &token_url,
            None,
            &scope,
        ),
    }
}

fn ml_client_for_settings(cfg: &MailOAuthSettings) -> anyhow::Result<MlOAuthClient> {
    let client_id = resolved_client_id(cfg);
    if client_id.trim().is_empty() {
        bail!("{CLIENT_ID_MISSING_HINT}");
    }
    let provider_cfg = ml_provider_for_settings(cfg)?;
    let client = MlOAuthClient::new(client_id, provider_cfg);
    let secret = cfg.client_secret.trim();
    Ok(if secret.is_empty() {
        client
    } else {
        client.with_client_secret(secret)
    })
}

fn apply_result_to_state(state: &mut OAuthState, result: &OAuthResult) {
    if !result.access_token.trim().is_empty() {
        state.access_token = result.access_token.trim().to_string();
    }
    if !result.refresh_token.trim().is_empty() {
        state.refresh_token = result.refresh_token.trim().to_string();
    }
    if result.expires_at_ms > 0 {
        state.expires_at_ms = result.expires_at_ms;
    }
    if !result.email.trim().is_empty() {
        state.email = result.email.trim().to_string();
    }
    if !result.scope.trim().is_empty() {
        state.scope = result.scope.trim().to_string();
    }
}

fn http_timeout() -> Duration {
    Duration::from_secs(HTTP_TIMEOUT_SECS)
}

fn build_refresh_form(
    _provider: OAuthProvider,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
    scope: &str,
) -> Vec<(&'static str, String)> {
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("client_id", client_id.to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    if !scope.trim().is_empty() {
        form.push(("scope", scope.trim().to_string()));
    }
    if !client_secret.trim().is_empty() {
        form.push(("client_secret", client_secret.trim().to_string()));
    }
    form
}

// ============================================================
// 交互式授权：入口
// ============================================================

impl OAuth {
    /// 契约要求的三参数入口：内置 client_id / scope / 端点，只按 provider 走流程。
    ///
    /// - Microsoft：device code，`user_code` 需要显示给用户；
    /// - Google：loopback PKCE；
    /// - Custom：参数里没有端点，会返回错误，请用 [`OAuth::start_flow_with_settings`]。
    pub async fn start_flow(
        provider: OAuthProvider,
        client_id: &str,
        tenant: &str,
    ) -> anyhow::Result<OAuthFlow> {
        let cfg = MailOAuthSettings {
            enabled: true,
            provider,
            client_id: client_id.to_string(),
            tenant: tenant.to_string(),
            ..MailOAuthSettings::default()
        };
        Self::start_flow_with_settings(&cfg).await
    }

    /// 完整入口：provider / client_id / tenant / scope / 自定义端点全部从配置来。
    pub async fn start_flow_with_settings(cfg: &MailOAuthSettings) -> anyhow::Result<OAuthFlow> {
        start_flow_impl(cfg).await
    }
}

async fn start_flow_impl(cfg: &MailOAuthSettings) -> anyhow::Result<OAuthFlow> {
    let provider = cfg.provider;
    let client_id = resolved_client_id(cfg);
    if client_id.trim().is_empty() {
        bail!("{CLIENT_ID_MISSING_HINT}");
    }
    let token_url = resolved_token_url(cfg);
    if token_url.trim().is_empty() {
        bail!("OAuth2 令牌端点为空：请在设置页填写 token_url（Custom 提供方必须填）");
    }
    let auth_url = resolved_auth_url(cfg);
    if provider != OAuthProvider::Microsoft && auth_url.trim().is_empty() {
        bail!("OAuth2 授权端点为空：请在设置页填写 auth_url（Custom 提供方必须填）");
    }
    let scope = resolved_scope(cfg);
    if scope.trim().is_empty() {
        bail!("OAuth2 scope 为空：请在设置页填写 scope（Custom 提供方必须填）");
    }
    // 邮箱：oauth.email / access_token JWT（库的 Token 不含 id_token，只能兜底）。
    let email = fallback_email(cfg);
    let allow_insecure_tls = cfg.allow_insecure_tls;

    match provider {
        OAuthProvider::Microsoft => {
            // Microsoft 用本 crate 自己实现的 device code 流程（流程保旧，HTTP 用 reqwest）：
            // 这样 devicecode 请求/轮询请求体完全可控（确保 scope 带 offline_access），
            // token 响应里的 refresh_token 也能原样映射，不依赖库的内部固定 scope。
            let device_url = resolved_auth_url(cfg);
            let info =
                fetch_device_code_native(&device_url, &client_id, &scope, allow_insecure_tls)
                    .await?;
            let expires_in = info.expires_in.clamp(30, DEVICE_CODE_MAX_WAIT_SECS);
            let interval = info.interval.max(1).min(30);
            let device_code = info.device_code.clone();
            let wait_token_url = token_url.clone();
            let wait_client_id = client_id.clone();
            let wait_scope = scope.clone();
            let wait = tokio::spawn(async move {
                poll_device_code_native(
                    wait_token_url,
                    wait_client_id,
                    wait_scope,
                    device_code,
                    interval,
                    expires_in,
                    email,
                    provider,
                    allow_insecure_tls,
                )
                .await
            });
            Ok(OAuthFlow {
                authorize_url: info.verification_uri,
                user_code: Some(info.user_code),
                expires_in,
                wait,
                redirect_uri: None,
                provider,
                client_id,
                scope,
            })
        }
        OAuthProvider::Google | OAuthProvider::Custom => {
            let client = ml_client_for_settings(cfg)?;
            start_loopback_flow_library(provider, client_id, scope, email, client).await
        }
    }
}

/// 设备码请求体（单测断言 Microsoft scope 里包含 offline_access）。
fn build_device_code_form(client_id: &str, scope: &str) -> Vec<(&'static str, String)> {
    vec![
        ("client_id", client_id.to_string()),
        ("scope", scope.to_string()),
    ]
}

fn build_device_token_form(
    client_id: &str,
    device_code: &str,
    scope: &str,
) -> Vec<(&'static str, String)> {
    vec![
        (
            "grant_type",
            "urn:ietf:params:oauth:grant-type:device_code".to_string(),
        ),
        ("client_id", client_id.to_string()),
        ("device_code", device_code.to_string()),
        ("scope", scope.to_string()),
    ]
}

/// 请求 Microsoft device code（本 crate 实现，reqwest blocking）；错误中文。
async fn fetch_device_code_native(
    device_url: &str,
    client_id: &str,
    scope: &str,
    allow_insecure_tls: bool,
) -> anyhow::Result<DeviceCodeInfo> {
    if !scope.split_whitespace().any(|s| s == "offline_access") {
        tracing::warn!(
            scope = %scope,
            "Microsoft device code scope 缺少 offline_access，可能拿不到 refresh_token"
        );
    }
    tracing::info!(
        scope = %scope,
        has_offline_access = scope.split_whitespace().any(|s| s == "offline_access"),
        "请求 Microsoft device code"
    );
    let url = device_url.to_string();
    let form = build_device_code_form(client_id, scope);
    let body = tokio::task::spawn_blocking(move || {
        let response = http_post_form(&url, &form, http_timeout(), allow_insecure_tls)
            .map_err(|e| anyhow!("请求 Microsoft device code 失败：{e}"))?;
        if !(200..300).contains(&response.status) {
            bail!(
                "Microsoft device code 请求被拒绝（HTTP {}）：{}。常见原因：client_id 无效、tenant 不对、该账号类型不支持 device code。",
                response.status,
                truncate_body(&response.body)
            );
        }
        parse_device_code(&response.body)
    })
    .await
    .context("device code 请求后台线程异常结束")??;
    Ok(body)
}

/// 轮询 Microsoft token 端点（本 crate 实现）：保留 refresh_token / scope。
async fn poll_device_code_native(
    token_url: String,
    client_id: String,
    scope: String,
    device_code: String,
    mut interval_secs: u64,
    expires_in: u64,
    fallback_email: String,
    provider: OAuthProvider,
    allow_insecure_tls: bool,
) -> anyhow::Result<OAuthResult> {
    let deadline = std::time::Instant::now() + Duration::from_secs(expires_in.max(30));
    loop {
        if std::time::Instant::now() >= deadline {
            bail!("Microsoft 设备代码授权超时：请重新点击「用 Microsoft 登录」生成新的 user_code");
        }
        tokio::time::sleep(Duration::from_secs(interval_secs.max(1))).await;
        let url = token_url.clone();
        let form = build_device_token_form(&client_id, &device_code, &scope);
        let response = tokio::task::spawn_blocking(move || {
            http_post_form(&url, &form, http_timeout(), allow_insecure_tls)
        })
        .await
        .context("device code 轮询后台线程异常结束")?
        .map_err(|e| anyhow!("轮询 Microsoft 令牌端点失败：{e}"))?;

        // Microsoft 慢速轮询要求：按 RFC 8628 增加间隔。
        if response.body.contains("\"slow_down\"") {
            interval_secs = (interval_secs + 5).min(30);
        }
        match interpret_token_response(&response.body, OAuthProvider::Microsoft, allow_insecure_tls) {
            TokenPoll::Ok(mut result) => {
                if result.email.trim().is_empty() {
                    result.email = fallback_email;
                }
                tracing::info!(
                    refresh_token_present = !result.refresh_token.trim().is_empty(),
                    scope = %result.scope,
                    expires_at_ms = result.expires_at_ms,
                    "Microsoft device code 授权成功"
                );
                if result.refresh_token.trim().is_empty() {
                    tracing::warn!(
                        "Microsoft token 响应没有 refresh_token（scope 可能缺 offline_access，或该租户策略不返回 refresh token）"
                    );
                }
                let _ = provider;
                return Ok(result);
            }
            TokenPoll::Pending => continue,
            TokenPoll::Err(error) => return Err(error),
        }
    }
}

// ============================================================
// Microsoft device code
// ============================================================

/// 解析 `POST /devicecode` 的 JSON（保留为公共工具，供诊断/测试用）。
pub fn parse_device_code(body: &str) -> anyhow::Result<DeviceCodeInfo> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| anyhow!("解析 Microsoft device code 响应失败：{e}；响应：{}", truncate_body(body)))?;
    if let Some(err) = value.get("error").and_then(Value::as_str) {
        let desc = value
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or("");
        bail!("Microsoft device code 返回错误：{err} {desc}");
    }
    let device_code = value
        .get("device_code")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let user_code = value
        .get("user_code")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let verification_uri = value
        .get("verification_uri")
        .and_then(Value::as_str)
        .or_else(|| value.get("verification_url").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    if device_code.is_empty() || user_code.is_empty() || verification_uri.is_empty() {
        bail!(
            "Microsoft device code 响应缺少 device_code/user_code/verification_uri：{}",
            truncate_body(body)
        );
    }
    Ok(DeviceCodeInfo {
        device_code,
        user_code,
        verification_uri,
        expires_in: value
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(900),
        interval: value.get("interval").and_then(Value::as_u64).unwrap_or(5),
        message: value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

// ============================================================
// Google / Custom：loopback + PKCE
// ============================================================

async fn start_loopback_flow_library(
    provider: OAuthProvider,
    client_id: String,
    scope: String,
    fallback_email: String,
    client: MlOAuthClient,
) -> anyhow::Result<OAuthFlow> {
    // 先同步 bind 拿到端口，才能拼出 redirect_uri。
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("无法在本机随机端口启动 OAuth 回调服务器（127.0.0.1:0）")?;
    listener
        .set_nonblocking(true)
        .context("设置回调服务器非阻塞失败")?;
    let port = listener
        .local_addr()
        .context("读取回调服务器端口失败")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let state = random_state();
    let scopes = split_scopes(&scope);
    let client = client.with_redirect_uri(redirect_uri.clone());
    // 库的 AuthorizationCodeFlow 用 provider.name 判断 Google 专属参数（access_type/prompt）。
    let flow = AuthorizationCodeFlow::new(client).with_pkce();
    let authorize_url = flow
        .authorization_url(Some(&scopes), Some(&state))
        .map_err(|e| anyhow!("生成授权 URL 失败：{e}"))?
        .to_string();
    let expires_in = LOOPBACK_EXPIRES_SECS;
    let wait_scope = scope.clone();
    let wait = tokio::spawn(async move {
        wait_loopback_callback_library(
            listener,
            flow,
            state,
            expires_in,
            fallback_email,
            provider,
            wait_scope,
        )
        .await
    });
    Ok(OAuthFlow {
        authorize_url,
        user_code: None,
        expires_in,
        wait,
        redirect_uri: Some(redirect_uri),
        provider,
        client_id,
        scope,
    })
}

/// 拼授权码 URL（Google 额外带 `access_type=offline` 与 `prompt=consent` 以拿 refresh_token）。
pub fn build_authorize_url(
    provider: OAuthProvider,
    auth_url: &str,
    client_id: &str,
    scope: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> String {
    let mut url = format!(
        "{}?client_id={}&response_type=code&redirect_uri={}&scope={}\
         &code_challenge={}&code_challenge_method=S256&state={}",
        auth_url.trim_end_matches(['?', '&']),
        form_encode(client_id),
        form_encode(redirect_uri),
        form_encode(scope),
        form_encode(code_challenge),
        form_encode(state),
    );
    if provider == OAuthProvider::Google {
        url.push_str("&access_type=offline&prompt=consent&include_granted_scopes=true");
    }
    url
}

fn random_state() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[allow(clippy::too_many_arguments)]
async fn wait_loopback_callback_library(
    listener: std::net::TcpListener,
    flow: AuthorizationCodeFlow,
    expected_state: String,
    expires_in: u64,
    fallback_email: String,
    provider: OAuthProvider,
    scope: String,
) -> anyhow::Result<OAuthResult> {
    let listener =
        tokio::net::TcpListener::from_std(listener).context("接管 OAuth 回调服务器失败")?;
    let accepted = tokio::time::timeout(
        Duration::from_secs(expires_in),
        listener.accept(),
    )
    .await;
    let (mut stream, _peer) = match accepted {
        Err(_) => bail!(
            "OAuth 授权等待超时（{expires_in} 秒）：请重新点击登录按钮，并在浏览器里完成授权"
        ),
        Ok(Err(e)) => bail!("OAuth 本地回调服务器出错：{e}"),
        Ok(Ok(pair)) => pair,
    };
    let mut buf = vec![0u8; 8192];
    let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await;
    let n = match read {
        Err(_) => bail!("OAuth 本地回调读取超时，请重新点击登录按钮"),
        Ok(Err(e)) => bail!("OAuth 本地回调读取失败：{e}"),
        Ok(Ok(n)) => n,
    };
    let request = String::from_utf8_lossy(&buf[..n]).to_string();
    let first_line = request.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("/");

    let provider_error = query_param(path, "error");
    let provider_error_desc = query_param(path, "error_description");
    let code = query_param(path, "code");
    let got_state = query_param(path, "state").unwrap_or_default();

    let response_body = match (&provider_error, &code) {
        (Some(err), _) => format!(
            "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
             <title>NeoSonia 邮箱授权失败</title></head><body style=\"font-family:sans-serif;background:#121114;color:#eee;padding:32px\">\
             <h2>授权没有完成</h2><p>服务端返回：{} {}</p><p>可以关闭此页面，回到 NeoSonia 重试。</p>\
             </body></html>",
            html_escape(err),
            html_escape(provider_error_desc.as_deref().unwrap_or(""))
        ),
        (_, None) => "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
             <title>NeoSonia 邮箱授权失败</title></head><body style=\"font-family:sans-serif;background:#121114;color:#eee;padding:32px\">\
             <h2>没有拿到授权码</h2><p>可以关闭此页面，回到 NeoSonia 重试。</p></body></html>"
            .to_string(),
        (_, Some(_)) if got_state != expected_state => "<!doctype html><html lang=\"zh-CN\">\
             <head><meta charset=\"utf-8\"><title>NeoSonia 邮箱授权失败</title></head>\
             <body style=\"font-family:sans-serif;background:#121114;color:#eee;padding:32px\">\
             <h2>授权状态校验失败</h2><p>请关闭此页面，回到 NeoSonia 重新点击登录按钮。</p></body></html>"
            .to_string(),
        (_, Some(_)) => "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
             <title>NeoSonia 邮箱授权完成</title></head><body style=\"font-family:sans-serif;background:#121114;color:#eee;padding:32px\">\
             <h2>授权完成</h2><p>可以关闭此页面，回到 NeoSonia。</p></body></html>"
            .to_string(),
    };
    write_http_response(&mut stream, &response_body).await;

    if let Some(err) = provider_error {
        bail!(
            "邮箱授权没有完成：服务端返回 {}{}",
            err,
            provider_error_desc
                .map(|d| format!("（{d}）"))
                .unwrap_or_default()
        );
    }
    let Some(code) = code else {
        bail!("邮箱授权没有完成：回调里没有 code 参数");
    };
    if got_state != expected_state {
        bail!("邮箱授权回调的 state 校验失败：可能存在安全风险，已中止");
    }

    // 主线：mailledger-oauth 的 AuthorizationCodeFlow::exchange_code（PKCE verifier 在 flow 里）。
    let token = tokio::time::timeout(http_timeout(), flow.exchange_code(&code, None))
        .await
        .map_err(|_| anyhow!("用授权码换取 token 超时（{} 秒）", HTTP_TIMEOUT_SECS))?
        .map_err(|e| ml_error_to_anyhow("用授权码换取 token", &e))?;
    Ok(token_to_result(&token, provider, &fallback_email, &scope))
}

async fn write_http_response(stream: &mut tokio::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.as_bytes().len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

fn html_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ============================================================
// 令牌响应解析（保留给降级 HTTP 路径与单测）
// ============================================================

#[derive(Debug, Default, Deserialize)]
struct RawTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
    token_type: Option<String>,
    id_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

enum TokenPoll {
    Ok(OAuthResult),
    Pending,
    Err(anyhow::Error),
}

/// 把 token 端点返回体解析成 [`OAuthResult`]（错误直接返回中文 `anyhow::Error`）。
pub fn token_result_from_body(body: &str, provider: OAuthProvider) -> anyhow::Result<OAuthResult> {
    token_result_from_body_opts(body, provider, false)
}

/// 同 [`token_result_from_body`]，但允许按配置在证书失败时降级（Google userinfo 用）。
pub fn token_result_from_body_opts(
    body: &str,
    provider: OAuthProvider,
    allow_insecure_tls: bool,
) -> anyhow::Result<OAuthResult> {
    match interpret_token_response(body, provider, allow_insecure_tls) {
        TokenPoll::Ok(result) => Ok(result),
        TokenPoll::Pending => {
            bail!("OAuth 授权仍在等待用户确认（authorization_pending），请稍后再试")
        }
        TokenPoll::Err(e) => Err(e),
    }
}

fn interpret_token_response(
    body: &str,
    provider: OAuthProvider,
    _allow_insecure_tls: bool,
) -> TokenPoll {
    let raw: RawTokenResponse = match serde_json::from_str(body) {
        Ok(raw) => raw,
        Err(e) => {
            return TokenPoll::Err(anyhow!(
                "解析 OAuth 令牌响应失败：{e}；响应：{}",
                truncate_body(body)
            ))
        }
    };
    if let Some(error) = raw.error.as_deref() {
        return match error {
            "authorization_pending" | "slow_down" => TokenPoll::Pending,
            "expired_token" => TokenPoll::Err(anyhow!(
                "授权码/设备码已过期：请重新点击登录按钮生成新的授权"
            )),
            "authorization_declined" | "access_denied" => {
                TokenPoll::Err(anyhow!("用户拒绝了授权，或授权被管理员阻止"))
            }
            "invalid_grant" => TokenPoll::Err(anyhow!(
                "授权已失效（invalid_grant）：refresh_token 被吊销或已过期，请在设置页重新登录邮箱{}",
                raw.error_description
                    .as_deref()
                    .map(|d| format!("（{d}）"))
                    .unwrap_or_default()
            )),
            other => TokenPoll::Err(anyhow!(
                "OAuth 服务端返回错误 {other}{}",
                raw.error_description
                    .as_deref()
                    .map(|d| format!("：{d}"))
                    .unwrap_or_default()
            )),
        };
    }
    let Some(access_token) = raw.access_token.filter(|t| !t.trim().is_empty()) else {
        return TokenPoll::Err(anyhow!(
            "OAuth 令牌响应里没有 access_token：{}",
            truncate_body(body)
        ));
    };
    let email = raw
        .id_token
        .as_deref()
        .and_then(jwt_email)
        .unwrap_or_default();
    let _ = provider;
    let expires_in = raw.expires_in.unwrap_or(3600).max(1);
    TokenPoll::Ok(OAuthResult {
        email,
        refresh_token: raw.refresh_token.unwrap_or_default(),
        access_token,
        expires_at_ms: now_ms().saturating_add(expires_in * 1000),
        scope: raw.scope.unwrap_or_default(),
        token_type: raw.token_type.unwrap_or_else(|| "Bearer".to_string()),
    })
}

fn truncate_body(body: &str) -> String {
    let compact = body.trim().replace('\n', " ");
    if compact.chars().count() > 400 {
        compact.chars().take(400).collect::<String>() + "..."
    } else {
        compact
    }
}

// ============================================================
// OAuth 的 HTTP 客户端（reqwest blocking + native-tls）
// ============================================================
//
// 不用手写 TLS：Windows 上 reqwest 的 native-tls 后端走 SChannel，能使用系统
// 根证书（Kaspersky 等 MITM 代理安装的证书也能被信任）和系统代理设置；
// rustls + 内置 webpki 根证书在这种环境会握手失败（OAuth 授权后仍不可用的根因）。
//
// 真机 TLS 行为无法在 CI 里单测；这里单测覆盖 mock HTTP（127.0.0.1）的请求/响应
// 与错误映射，真实 endpoint 由 device code 真机测试覆盖。

#[derive(Debug, Clone)]
struct HttpResponse {
    status: u16,
    body: String,
}

fn build_http_client(insecure: bool, timeout: Duration) -> anyhow::Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .user_agent(format!("NeoSonia/{}", env!("CARGO_PKG_VERSION")));
    if insecure {
        builder = builder.danger_accept_invalid_certs(true);
        tracing::warn!(
            "OAuth：已按 mail.oauth.allow_insecure_tls=true 关闭 HTTPS 证书校验（不推荐，仅用于证书/MITM 排查）"
        );
    }
    builder
        .build()
        .map_err(|e| anyhow!("初始化 OAuth HTTP 客户端失败：{}", reqwest_error_detail(&e)))
}

/// 把 reqwest::Error 变成带分类 + 完整 source 链的中文信息（不含 token）。
fn reqwest_error_detail(e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "超时"
    } else if e.is_connect() {
        "连接失败（TLS 握手/DNS/代理/证书）"
    } else if e.is_builder() {
        "请求构造失败"
    } else if e.is_request() {
        "请求发送失败"
    } else if e.is_body() {
        "响应体读取失败"
    } else if e.is_decode() {
        "响应解码失败"
    } else if e.is_redirect() {
        "重定向失败"
    } else if e.is_status() {
        "HTTP 状态错误"
    } else {
        "网络错误"
    };
    let mut chain: Vec<String> = vec![e.to_string()];
    let mut source = std::error::Error::source(e);
    let mut depth = 0;
    while let Some(err) = source {
        depth += 1;
        if depth > 8 {
            break;
        }
        let text = err.to_string();
        if !text.is_empty() && chain.last().map(|s| s != &text).unwrap_or(true) {
            chain.push(text);
        }
        source = err.source();
    }
    format!("{kind}：{}", chain.join(" <- "))
}

fn http_post_form_once(
    url: &str,
    form: &[(String, String)],
    timeout: Duration,
    insecure: bool,
) -> anyhow::Result<HttpResponse> {
    let client = build_http_client(insecure, timeout)?;
    let response = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(form)
        .send()
        .map_err(|e| anyhow!("OAuth POST {} 失败：{}", short_url(url), reqwest_error_detail(&e)))?;
    let status = response.status().as_u16();
    let body = response
        .text()
        .map_err(|e| anyhow!("读取 OAuth 响应失败（POST {}）：{}", short_url(url), reqwest_error_detail(&e)))?;
    Ok(HttpResponse { status, body })
}

fn http_post_form(
    url: &str,
    pairs: &[(&str, String)],
    timeout: Duration,
    allow_insecure_retry: bool,
) -> anyhow::Result<HttpResponse> {
    let form: Vec<(String, String)> = pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect();
    match http_post_form_once(url, &form, timeout, false) {
        Ok(response) => Ok(response),
        Err(first) => {
            if allow_insecure_retry {
                tracing::warn!(url = short_url(url), error = %first, "OAuth HTTPS 请求失败，按配置关闭证书校验后重试一次");
                http_post_form_once(url, &form, timeout, true).map_err(|second| {
                    anyhow!(
                        "{first}\n已按 mail.oauth.allow_insecure_tls=true 关闭证书校验重试，仍失败：{second}"
                    )
                })
            } else {
                Err(first)
            }
        }
    }
}

/// 错误/日志里只显示 `协议://主机/路径`，不打印 query（避免带出 code/token）。
fn short_url(url: &str) -> String {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    no_query.to_string()
}

// ============================================================
// JWT claims（只读解析，不校验签名，不打印 token 明文）
// ============================================================

/// access_token 里与排障相关的 claims（Microsoft 的 access token 是 JWT）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AccessTokenClaims {
    pub audience: Vec<String>,
    pub scopes: Vec<String>,
    pub has_imap_scope: bool,
    pub has_smtp_scope: bool,
}

/// 解析 JWT payload（base64url JSON）；不是 JWT 时返回 None。
pub fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64_url_decode(payload)?;
    serde_json::from_slice(&bytes).ok()
}

/// 从 access_token 里解出 aud / scp（Microsoft）。Google 的 access_token 不是
/// JWT（返回默认值，界面上显示“未知”即可）。
pub fn access_token_claims(token: &str) -> AccessTokenClaims {
    let token = token.trim();
    if token.is_empty() {
        return AccessTokenClaims::default();
    }
    let Some(json) = jwt_payload(token) else {
        return AccessTokenClaims::default();
    };
    let mut audience = Vec::new();
    match json.get("aud") {
        Some(Value::String(value)) => audience.push(value.clone()),
        Some(Value::Array(values)) => {
            for value in values {
                if let Some(text) = value.as_str() {
                    audience.push(text.to_string());
                }
            }
        }
        _ => {}
    }
    let mut scopes: Vec<String> = Vec::new();
    for key in ["scp", "scope"] {
        if let Some(text) = json.get(key).and_then(Value::as_str) {
            for scope in text.split_whitespace() {
                if !scope.is_empty() && !scopes.iter().any(|s| s == scope) {
                    scopes.push(scope.to_string());
                }
            }
        }
    }
    let has_imap_scope = scopes
        .iter()
        .any(|scope| scope.eq_ignore_ascii_case("IMAP.AccessAsUser.All"));
    let has_smtp_scope = scopes
        .iter()
        .any(|scope| scope.eq_ignore_ascii_case("SMTP.Send"));
    AccessTokenClaims {
        audience,
        scopes,
        has_imap_scope,
        has_smtp_scope,
    }
}

// ============================================================
// needs_reauth 状态（进程内标记，UI 读取；写回 settings 由 app 做）
// ============================================================

static NEEDS_REAUTH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 标记“需要重新授权”（缺 IMAP 权限/受众不对/refresh 失效等）。
pub fn mark_needs_reauth(reason: &str) {
    NEEDS_REAUTH.store(true, std::sync::atomic::Ordering::SeqCst);
    tracing::warn!(reason, "邮箱 OAuth 需要重新授权");
}

/// 清除 needs_reauth（授权成功/apply_oauth_result 后调用）。
pub fn clear_needs_reauth() {
    NEEDS_REAUTH.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// 当前是否处于“需要重新授权”状态。
pub fn needs_reauth() -> bool {
    NEEDS_REAUTH.load(std::sync::atomic::Ordering::SeqCst)
}

/// 错误文本是否表示需要重新授权（Microsoft: "User is authenticated but not connected"、
/// scope/受众错误、invalid_grant 等）。
pub fn error_text_needs_reauth(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("authenticated but not connected")
        || lower.contains("user is authenticated but not connected")
        || lower.contains("invalid_grant")
        || lower.contains("insufficient")
        || lower.contains("missing scope")
        || lower.contains("scope")
        || lower.contains("audience")
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn header_end_pos(buf: &[u8]) -> Option<usize> {
        buf.windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|pos| pos + 4)
    }

    /// 请求是否已经完整（头 + Content-Length 指定的 body）：reqwest 会先发头再发 body，
    /// mock 必须读完整个请求再响应，否则关闭连接会触发客户端 RST（os error 10053）。
    fn request_complete(buf: &[u8]) -> bool {
        let Some(end) = header_end_pos(buf) else {
            return false;
        };
        let headers = String::from_utf8_lossy(&buf[..end]);
        if headers
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            return buf[end..].windows(5).any(|window| window == b"\r\n0\r\n\r\n");
        }
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().to_string())
            })
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        buf.len() >= end + content_length
    }

    fn base64_url(data: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
    }

    fn fake_id_token(claims: &Value) -> String {
        let header = base64_url(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = base64_url(claims.to_string().as_bytes());
        format!("{header}.{payload}.signature")
    }

    // ---------- 纯函数 ----------

    #[test]
    fn xoauth2_string_matches_contract() {
        assert_eq!(
            build_xoauth2("user@example.com", "ya29.token"),
            "user=user@example.com\u{1}auth=Bearer ya29.token\u{1}\u{1}"
        );
    }

    #[test]
    fn access_token_claims_parse_aud_and_scp() {
        let token = fake_id_token(&serde_json::json!({
            "aud": "https://outlook.office.com",
            "scp": "IMAP.AccessAsUser.All SMTP.Send offline_access"
        }));
        let claims = access_token_claims(&token);
        assert_eq!(claims.audience, vec!["https://outlook.office.com"]);
        assert!(claims.has_imap_scope);
        assert!(claims.has_smtp_scope);
        assert!(claims
            .scopes
            .iter()
            .any(|s| s == "IMAP.AccessAsUser.All"));

        // aud 可能是数组；缺 SMTP scope 也要能识别
        let token2 = fake_id_token(&serde_json::json!({
            "aud": ["https://outlook.office.com", "https://graph.microsoft.com"],
            "scp": "SMTP.Send"
        }));
        let claims2 = access_token_claims(&token2);
        assert_eq!(claims2.audience.len(), 2);
        assert!(!claims2.has_imap_scope);
        assert!(claims2.has_smtp_scope);

        // Google 的 access_token 不是 JWT：返回空 claims，不 panic
        let opaque = access_token_claims("ya29.opaque-token");
        assert!(opaque.audience.is_empty());
        assert!(opaque.scopes.is_empty());
        assert!(jwt_payload("not-a-jwt").is_none());
    }

    #[test]
    fn needs_reauth_texts_and_flag() {
        assert!(error_text_needs_reauth(
            "NO User is authenticated but not connected."
        ));
        assert!(error_text_needs_reauth("invalid_grant"));
        assert!(!error_text_needs_reauth("connection timed out"));

        clear_needs_reauth();
        assert!(!needs_reauth());
        mark_needs_reauth("unit test");
        assert!(needs_reauth());
        clear_needs_reauth();
        assert!(!needs_reauth());
    }

    #[test]
    fn http_post_form_uses_reqwest_and_classifies_errors() {
        // 127.0.0.1:1 通常立刻拒绝连接：验证走的是 reqwest，且错误带分类/source 链。
        let form = vec![("grant_type", "refresh_token".to_string())];
        let err = http_post_form(
            "http://127.0.0.1:1/oauth2/v2.0/token",
            &form,
            Duration::from_secs(2),
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("OAuth POST"), "got: {err}");
        assert!(
            err.contains("连接失败") || err.contains("超时"),
            "错误应带 reqwest 分类: {err}"
        );
    }

    #[test]
    fn insecure_retry_only_when_enabled() {
        let form = vec![("a", "b".to_string())];
        let url = "http://127.0.0.1:1/oauth/token";
        // 默认不允许降级：只报第一次错误
        let err = http_post_form(url, &form, Duration::from_secs(2), false)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("allow_insecure_tls"), "got: {err}");
        // 开关打开后会在失败后重试一次，并把“已按配置…”写进错误
        let err2 = http_post_form(url, &form, Duration::from_secs(2), true)
            .unwrap_err()
            .to_string();
        assert!(err2.contains("allow_insecure_tls=true"), "got: {err2}");
        assert!(err2.contains("仍失败"), "got: {err2}");
    }

    #[test]
    fn xoauth2_authenticator_returns_token_then_empty() {
        let auth = XOAuth2Authenticator::new("u@example.com", "tok");
        use imap::Authenticator as _;
        assert_eq!(
            auth.process(b""),
            "user=u@example.com\u{1}auth=Bearer tok\u{1}\u{1}"
        );
        assert_eq!(auth.process(b"error"), "");
    }

    #[test]
    fn pkce_rfc7636_known_vector() {
        // RFC 7636 附录 B 的测试向量。
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let generated = pkce_verifier();
        assert!(generated.len() >= 43 && generated.len() <= 128);
        assert_ne!(pkce_challenge(&generated), generated);
    }

    #[test]
    fn form_encoding_is_rfc3986() {
        assert_eq!(form_encode("a b"), "a%20b");
        assert_eq!(form_encode("a+b&c=d"), "a%2Bb%26c%3Dd");
        assert_eq!(form_encode("https://x/y?z=1"), "https%3A%2F%2Fx%2Fy%3Fz%3D1");
        assert_eq!(form_encode("中文"), "%E4%B8%AD%E6%96%87");
    }

    #[test]
    fn query_param_roundtrip() {
        let url = "http://x/auth?client_id=abc&redirect_uri=http%3A%2F%2F127.0.0.1%3A1234&state=ff";
        assert_eq!(query_param(url, "client_id").as_deref(), Some("abc"));
        assert_eq!(
            query_param(url, "redirect_uri").as_deref(),
            Some("http://127.0.0.1:1234")
        );
        assert_eq!(query_param(url, "missing"), None);
    }

    #[test]
    fn device_code_json_parses_and_reports_errors() {
        let body = r#"{
            "device_code": "dev123",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://microsoft.com/devicelogin",
            "expires_in": 900,
            "interval": 5,
            "message": "请打开页面输入代码"
        }"#;
        let info = parse_device_code(body).expect("parse");
        assert_eq!(info.user_code, "ABCD-EFGH");
        assert_eq!(info.device_code, "dev123");
        assert_eq!(info.interval, 5);
        assert_eq!(info.verification_uri, "https://microsoft.com/devicelogin");

        let err = parse_device_code(r#"{"error":"invalid_client","error_description":"bad id"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid_client"), "got: {err}");
        assert!(parse_device_code("not json").is_err());
    }

    #[test]
    fn print_authorize_urls() {
        // 不联网也能拼出的授权 URL（供人工核对；--nocapture 可见）。
        let ms = MailOAuthSettings::default();
        println!("=== Microsoft device code 端点（联网后由服务端返回 verification_uri + user_code） ===");
        println!("devicecode: {}", resolved_auth_url(&ms));
        println!("token:      {}", resolved_token_url(&ms));
        println!("scope:      {}", resolved_scope(&ms));

        let verifier = pkce_verifier();
        let challenge = pkce_challenge(&verifier);
        let google = build_authorize_url(
            OAuthProvider::Google,
            GOOGLE_AUTH_URL,
            "demo-client-id",
            GOOGLE_SCOPE,
            "http://127.0.0.1:45678",
            &challenge,
            "demo-state",
        );
        println!("=== Google loopback PKCE 授权 URL（redirect_uri 实际端口运行时随机） ===");
        println!("{google}");
        println!("code_verifier(演示，实际运行时随机): {verifier}");
    }

    #[test]
    fn force_refresh_ignores_valid_cache() {
        let mock = MockServer::start(vec![
            serde_json::json!({
                "access_token": "forced-new",
                "expires_in": 3600,
                "token_type": "Bearer",
            })
            .to_string(),
        ]);
        let cfg = MailOAuthSettings {
            enabled: true,
            provider: OAuthProvider::Custom,
            client_id: "cid".into(),
            token_url: mock.url("/token"),
            access_token: "cached-valid".into(),
            refresh_token: "refresh-1".into(),
            expires_at_ms: now_ms() + 3_600_000,
            ..MailOAuthSettings::default()
        };
        let oauth = OAuth::from_settings(&cfg);
        assert!(oauth.access_token_valid());
        // access_token 缓存有效时不请求网络
        let mut oauth_mut = OAuth::from_settings(&cfg);
        assert_eq!(oauth_mut.access_token().unwrap(), "cached-valid");
        // 强制刷新会忽略缓存
        assert_eq!(oauth.access_token_force().unwrap(), "forced-new");
        mock.finish();
    }

    #[test]
    fn device_code_forms_include_offline_access() {
        // 单测断言：devicecode 请求体与 token 轮询请求体里的 scope 都包含 offline_access
        // 以及 IMAP/SMTP 权限（否则 Microsoft 不返回 refresh_token）。
        let form = build_device_code_form("client-x", MICROSOFT_SCOPE);        let scope = form
            .iter()
            .find(|(key, _)| *key == "scope")
            .map(|(_, value)| value.clone())
            .expect("scope field");
        assert!(scope.contains("offline_access"), "devicecode scope: {scope}");
        assert!(scope.contains("IMAP.AccessAsUser.All"), "devicecode scope: {scope}");
        assert!(scope.contains("SMTP.Send"), "devicecode scope: {scope}");

        let form = build_device_token_form("client-x", "device-code", MICROSOFT_SCOPE);
        let scope = form
            .iter()
            .find(|(key, _)| *key == "scope")
            .map(|(_, value)| value.clone())
            .expect("token scope field");
        assert!(scope.contains("offline_access"), "token scope: {scope}");
        assert!(form.iter().any(|(key, value)| *key == "grant_type"
            && value == "urn:ietf:params:oauth:grant-type:device_code"));
    }

    #[test]
    fn interpret_token_response_keeps_refresh_token() {
        let body = serde_json::json!({
            "token_type": "Bearer",
            "scope": "https://outlook.office.com/IMAP.AccessAsUser.All offline_access",
            "expires_in": 3600,
            "access_token": "at-1",
            "refresh_token": "rt-1",
        })
        .to_string();
        let result = token_result_from_body(&body, OAuthProvider::Microsoft).expect("token");
        assert_eq!(result.access_token, "at-1");
        assert_eq!(result.refresh_token, "rt-1");
        assert!(result.scope.contains("offline_access"));
    }

    #[test]
    fn token_response_parses_success_and_errors() {
        let id_token = fake_id_token(&serde_json::json!({"preferred_username": "demo@outlook.com"}));
        let body = serde_json::json!({
            "token_type": "Bearer",
            "scope": "scope-a",
            "expires_in": 3600,
            "access_token": "at-1",
            "refresh_token": "rt-1",
            "id_token": id_token,
        })
        .to_string();
        let result = token_result_from_body(&body, OAuthProvider::Microsoft).expect("token");
        assert_eq!(result.access_token, "at-1");
        assert_eq!(result.refresh_token, "rt-1");
        assert_eq!(result.email, "demo@outlook.com");
        assert!(result.expires_at_ms > now_ms());

        let pending = token_result_from_body(
            r#"{"error":"authorization_pending"}"#,
            OAuthProvider::Microsoft,
        );
        assert!(pending.is_err());
        let invalid = token_result_from_body(
            r#"{"error":"invalid_grant","error_description":"revoked"}"#,
            OAuthProvider::Google,
        )
        .unwrap_err()
        .to_string();
        assert!(invalid.contains("invalid_grant"), "got: {invalid}");
        assert!(token_result_from_body(
            r#"{"error":"expired_token"}"#,
            OAuthProvider::Microsoft
        )
        .is_err());
    }

    #[test]
    fn jwt_email_extraction() {
        let token = fake_id_token(&serde_json::json!({"email": "a@b.com"}));
        assert_eq!(jwt_email(&token).as_deref(), Some("a@b.com"));
        let token = fake_id_token(&serde_json::json!({"upn": "u@corp.com"}));
        assert_eq!(jwt_email(&token).as_deref(), Some("u@corp.com"));
        assert_eq!(jwt_email("garbage"), None);
    }

    #[test]
    fn client_id_defaults_and_overrides() {
        // Microsoft 默认使用 Thunderbird 公开桌面客户端 id（Office 公共 id 不可用于 IMAP，已回退）。
        let ms = MailOAuthSettings::default();
        assert_eq!(resolved_client_id(&ms), MICROSOFT_CLIENT_ID);
        assert_eq!(
            MICROSOFT_CLIENT_ID,
            "9e5f94bc-e8a4-4e73-b8be-63364c29d753"
        );
        assert!(resolved_scope(&ms).contains("IMAP.AccessAsUser.All"));
        assert!(resolved_auth_url(&ms).ends_with("/devicecode"));
        assert!(resolved_token_url(&ms).ends_with("/token"));
        assert!(resolved_auth_url(&ms).contains("/common/"));

        // 用户自建 Azure 应用可覆盖（授权页显示自己的应用名）
        let mut custom_ms = ms.clone();
        custom_ms.client_id = "my-azure-client".into();
        assert_eq!(resolved_client_id(&custom_ms), "my-azure-client");

        // Google / Custom 没有默认公开 id：必须用户自建
        let mut google = MailOAuthSettings {
            provider: OAuthProvider::Google,
            tenant: String::new(),
            ..MailOAuthSettings::default()
        };
        assert!(resolved_client_id(&google).is_empty());
        assert_eq!(resolved_auth_url(&google), GOOGLE_AUTH_URL);
        assert_eq!(resolved_token_url(&google), GOOGLE_TOKEN_URL);
        assert!(resolved_scope(&google).contains("mail.google.com"));

        google.client_id = "custom-client".into();
        assert_eq!(resolved_client_id(&google), "custom-client");

        let custom = MailOAuthSettings {
            provider: OAuthProvider::Custom,
            ..MailOAuthSettings::default()
        };
        assert!(resolved_client_id(&custom).is_empty());
        assert!(resolved_auth_url(&custom).is_empty());
        assert!(resolved_token_url(&custom).is_empty());
    }

    #[test]
    fn legacy_thunderbird_client_id_is_ignored() {
        assert!(is_legacy_thunderbird_client_id(
            LEGACY_THUNDERBIRD_MICROSOFT_CLIENT_ID
        ));
        assert!(is_legacy_thunderbird_client_id(
            LEGACY_THUNDERBIRD_GOOGLE_CLIENT_ID
        ));
        // Thunderbird id 现在就是内置默认值，因此属于"内置 id"而不是需要忽略的遗留值。
        assert!(is_legacy_thunderbird_client_id(MICROSOFT_CLIENT_ID));

        // Thunderbird id 现在是内置默认值：配置里写它也应直接采用（不再"忽略"）。
        let cfg = MailOAuthSettings {
            client_id: LEGACY_THUNDERBIRD_MICROSOFT_CLIENT_ID.into(),
            ..MailOAuthSettings::default()
        };
        assert_eq!(
            resolved_client_id(&cfg),
            LEGACY_THUNDERBIRD_MICROSOFT_CLIENT_ID
        );

        // 配置为空时 Microsoft 仍回退到同一个内置 id。
        let empty = MailOAuthSettings {
            client_id: String::new(),
            ..MailOAuthSettings::default()
        };
        assert_eq!(resolved_client_id(&empty), MICROSOFT_CLIENT_ID);
    }

    #[test]
    fn microsoft_scope_always_keeps_offline_access() {
        // 历史配置里 scope 可能来自 token 响应的 granted scope，不含 offline_access；
        // 重新授权时必须补上，否则 Microsoft 不返回 refresh_token。
        let cfg = MailOAuthSettings {
            provider: OAuthProvider::Microsoft,
            scope: "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send"
                .into(),
            ..MailOAuthSettings::default()
        };
        let scope = resolved_scope(&cfg);
        assert!(scope.contains("offline_access"), "scope: {scope}");
        assert!(scope.contains("IMAP.AccessAsUser.All"));
        // devicecode 请求体用的是同一个 resolved_scope
        let form = build_device_code_form("cid", &scope);
        let sent = &form.iter().find(|(k, _)| *k == "scope").unwrap().1;
        assert!(sent.contains("offline_access"), "sent scope: {sent}");
    }

    #[tokio::test]
    async fn missing_client_id_gives_chinese_hint() {
        // Google：自建客户端必须有 client_id（会在联网前报错）
        let google = MailOAuthSettings {
            provider: OAuthProvider::Google,
            ..MailOAuthSettings::default()
        };
        let err = OAuth::start_flow_with_settings(&google)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("client_id"), "got: {err}");
        assert!(err.contains("Google"), "got: {err}");

        // Custom：填好端点/scope 但 client_id 为空 → 明确提示
        let custom = MailOAuthSettings {
            provider: OAuthProvider::Custom,
            auth_url: "https://example.com/authorize".into(),
            token_url: "https://example.com/token".into(),
            scope: "mail".into(),
            ..MailOAuthSettings::default()
        };
        let err = OAuth::start_flow_with_settings(&custom)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("client_id"), "got: {err}");
    }

    #[test]
    fn xoauth2_uses_mailledger_sasl() {
        // 库函数返回 base64；我们解码回 raw 供 imap/lettre 再编码。
        let user = "u@outlook.com";
        let token = "token-abc";
        let library_encoded = mailledger_oauth::sasl::xoauth2_response(user, token);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(library_encoded.as_bytes())
            .expect("decode lib sasl");
        assert_eq!(String::from_utf8(decoded).unwrap(), build_xoauth2(user, token));
        assert_eq!(
            build_xoauth2(user, token),
            "user=u@outlook.com\u{1}auth=Bearer token-abc\u{1}\u{1}"
        );
    }

    #[test]
    fn token_to_result_email_fallback_order() {
        let token = MlToken::new("opaque-access", "Bearer")
            .with_refresh_token("rt")
            .with_scope("scope-z");
        // 1) fallback_email 优先
        let result = token_to_result(&token, OAuthProvider::Microsoft, "fallback@x.com", "req");
        assert_eq!(result.email, "fallback@x.com");
        assert_eq!(result.refresh_token, "rt");
        assert_eq!(result.scope, "scope-z");
        assert_eq!(result.token_type, "Bearer");
        // 2) 空 fallback 时尝试 access_token JWT 的 upn
        let jwt = fake_id_token(&serde_json::json!({"upn": "jwt@x.com"}));
        let token = MlToken::new(jwt, "Bearer");
        let result = token_to_result(&token, OAuthProvider::Microsoft, "", "scope-req");
        assert_eq!(result.email, "jwt@x.com");
        assert_eq!(result.scope, "scope-req");
        // 3) 都拿不到就留空（app 会提示用户填邮箱）
        let token = MlToken::new("opaque", "Bearer");
        let result = token_to_result(&token, OAuthProvider::Google, "", "");
        assert!(result.email.is_empty());
    }

    #[test]
    fn run_async_blocking_executes_future() {
        let value = run_async_blocking(async { 41 + 1 }).expect("async");
        assert_eq!(value, 42);
    }

    #[test]
    fn authorize_url_contains_pkce_params() {
        let verifier = pkce_verifier();
        let challenge = pkce_challenge(&verifier);
        let url = build_authorize_url(
            OAuthProvider::Google,
            GOOGLE_AUTH_URL,
            "demo-client-id",
            GOOGLE_SCOPE,
            "http://127.0.0.1:43210",
            &challenge,
            "state123",
        );
        assert!(url.starts_with(GOOGLE_AUTH_URL));
        assert_eq!(query_param(&url, "response_type").as_deref(), Some("code"));
        assert_eq!(
            query_param(&url, "client_id").as_deref(),
            Some("demo-client-id")
        );
        assert_eq!(
            query_param(&url, "redirect_uri").as_deref(),
            Some("http://127.0.0.1:43210")
        );
        assert_eq!(
            query_param(&url, "code_challenge").as_deref(),
            Some(challenge.as_str())
        );
        assert_eq!(
            query_param(&url, "code_challenge_method").as_deref(),
            Some("S256")
        );
        assert_eq!(query_param(&url, "state").as_deref(), Some("state123"));
        assert_eq!(
            query_param(&url, "access_type").as_deref(),
            Some("offline")
        );
    }

    // ---------- token 缓存 ----------

    #[test]
    fn valid_cached_token_is_returned_without_network() {
        let cfg = MailOAuthSettings {
            enabled: true,
            provider: OAuthProvider::Custom,
            client_id: "cid".into(),
            token_url: "http://127.0.0.1:1/token".into(),
            access_token: "cached-token".into(),
            refresh_token: "rt".into(),
            expires_at_ms: now_ms() + 3_600_000,
            ..MailOAuthSettings::default()
        };
        let mut oauth = OAuth::from_settings(&cfg);
        assert!(oauth.access_token_valid());
        assert_eq!(oauth.access_token().expect("cached"), "cached-token");
    }

    #[test]
    fn expired_token_without_refresh_token_errors() {
        let cfg = MailOAuthSettings {
            enabled: true,
            provider: OAuthProvider::Microsoft,
            access_token: "old".into(),
            refresh_token: String::new(),
            expires_at_ms: now_ms() - 1000,
            ..MailOAuthSettings::default()
        };
        let mut oauth = OAuth::from_settings(&cfg);
        let err = oauth.access_token().unwrap_err().to_string();
        assert!(err.contains("refresh_token"), "got: {err}");
    }

    #[test]
    fn expired_token_refreshes_via_mock_endpoint() {
        let mock = MockServer::start(vec![
            serde_json::json!({
                "access_token": "new-access",
                "expires_in": 3600,
                "token_type": "Bearer",
                "scope": "s",
            })
            .to_string(),
        ]);
        let cfg = MailOAuthSettings {
            enabled: true,
            provider: OAuthProvider::Custom,
            client_id: "cid".into(),
            token_url: mock.url("/token"),
            access_token: "old".into(),
            refresh_token: "refresh-1".into(),
            expires_at_ms: now_ms() - 1000,
            ..MailOAuthSettings::default()
        };
        let mut oauth = OAuth::from_settings(&cfg);
        let token = oauth.access_token().expect("refresh");
        assert_eq!(token, "new-access");
        let snapshot = oauth.snapshot();
        assert_eq!(snapshot.access_token, "new-access");
        assert_eq!(snapshot.refresh_token, "refresh-1");
        assert!(snapshot.expires_at_ms > now_ms());
        mock.finish();
    }

    // ---------- 完整流程（mock 端点） ----------

    #[tokio::test]
    async fn microsoft_device_code_flow_with_mock_endpoints() {
        let mock = MockServer::start_with_status(vec![
            // POST /devicecode
            (
                200,
                serde_json::json!({
                    "device_code": "device-abc",
                    "user_code": "ABCD-EFGH",
                    "verification_uri": "https://microsoft.com/devicelogin",
                    "expires_in": 120,
                    "interval": 1,
                    "message": "输入代码",
                })
                .to_string(),
            ),
            // 第 1 次轮询：还没授权（OAuth 规范用 400 + error）
            (400, r#"{"error":"authorization_pending"}"#.to_string()),
            // 第 2 次轮询：成功
            (
                200,
                serde_json::json!({
                    "token_type": "Bearer",
                    "scope": "scope-x",
                    "expires_in": 3600,
                    "access_token": "ms-access",
                    "refresh_token": "ms-refresh",
                })
                .to_string(),
            ),
        ]);
        let cfg = MailOAuthSettings {
            enabled: true,
            provider: OAuthProvider::Microsoft,
            client_id: "test-client".into(),
            tenant: "common".into(),
            email: "dev@example.com".into(),
            auth_url: mock.url("/devicecode"),
            token_url: mock.url("/token"),
            ..MailOAuthSettings::default()
        };
        let flow = OAuth::start_flow_with_settings(&cfg)
            .await
            .expect("device code flow");
        assert_eq!(flow.user_code.as_deref(), Some("ABCD-EFGH"));
        assert_eq!(flow.authorize_url, "https://microsoft.com/devicelogin");
        assert!(flow.expires_in >= 30);

        let result = tokio::time::timeout(Duration::from_secs(20), flow.wait)
            .await
            .expect("device code 等待超时")
            .expect("join")
            .expect("oauth result");
        assert_eq!(result.email, "dev@example.com");
        assert_eq!(result.access_token, "ms-access");
        assert_eq!(result.refresh_token, "ms-refresh");
        assert!(result.expires_at_ms > now_ms());
        mock.finish();
    }

    #[tokio::test]
    async fn loopback_pkce_flow_with_mock_endpoints() {
        let mock = MockServer::start(vec![serde_json::json!({
            "token_type": "Bearer",
            "scope": "scope-y",
            "expires_in": 1800,
            "access_token": "lb-access",
            "refresh_token": "lb-refresh",
        })
        .to_string()]);
        let cfg = MailOAuthSettings {
            enabled: true,
            provider: OAuthProvider::Custom,
            client_id: "custom-client".into(),
            email: "loop@example.com".into(),
            auth_url: "https://mock.example.com/authorize".into(),
            token_url: mock.url("/token"),
            scope: "mail custom".into(),
            ..MailOAuthSettings::default()
        };
        let flow = OAuth::start_flow_with_settings(&cfg)
            .await
            .expect("loopback flow");
        let redirect = flow.redirect_uri.clone().expect("redirect uri");
        assert!(redirect.starts_with("http://127.0.0.1:"));
        let state = query_param(&flow.authorize_url, "state").expect("state");
        let redirect_uri = query_param(&flow.authorize_url, "redirect_uri").expect("redirect uri");
        assert_eq!(redirect_uri, redirect);

        // 模拟浏览器回调：访问 loopback 端口。
        let addr = redirect.trim_start_matches("http://").to_string();
        let mut stream = tokio::net::TcpStream::connect(&addr)
            .await
            .expect("connect loopback");
        stream
            .write_all(
                format!(
                    "GET /?code=test-code&state={state} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("write callback");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response).await;
        assert!(response.contains("授权完成"), "callback page: {response}");

        let result = tokio::time::timeout(Duration::from_secs(20), flow.wait)
            .await
            .expect("loopback 等待超时")
            .expect("join")
            .expect("oauth result");
        assert_eq!(result.email, "loop@example.com");
        assert_eq!(result.access_token, "lb-access");
        assert_eq!(result.refresh_token, "lb-refresh");
        mock.finish();
    }

    #[tokio::test]
    async fn loopback_flow_rejects_state_mismatch() {
        let mock = MockServer::start(vec![]);
        let cfg = MailOAuthSettings {
            enabled: true,
            provider: OAuthProvider::Custom,
            client_id: "custom-client".into(),
            auth_url: "https://mock.example.com/authorize".into(),
            token_url: mock.url("/token"),
            scope: "mail".into(),
            ..MailOAuthSettings::default()
        };
        let flow = OAuth::start_flow_with_settings(&cfg)
            .await
            .expect("loopback flow");
        let redirect = flow.redirect_uri.clone().expect("redirect uri");
        let addr = redirect.trim_start_matches("http://").to_string();
        let mut stream = tokio::net::TcpStream::connect(&addr)
            .await
            .expect("connect loopback");
        stream
            .write_all(
                b"GET /?code=test-code&state=wrong-state HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write callback");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response).await;
        let err = tokio::time::timeout(Duration::from_secs(10), flow.wait)
            .await
            .expect("等待超时")
            .expect("join")
            .unwrap_err()
            .to_string();
        assert!(err.contains("state 校验失败"), "got: {err}");
        mock.finish();
    }

    #[tokio::test]
    async fn custom_provider_without_endpoints_errors() {
        let cfg = MailOAuthSettings {
            enabled: true,
            provider: OAuthProvider::Custom,
            client_id: "cid".into(),
            ..MailOAuthSettings::default()
        };
        let err = OAuth::start_flow_with_settings(&cfg)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("token_url") || err.contains("授权端点"), "got: {err}");
    }

    // ---------- 真机（默认 ignore；NEOSONIA_VERIFY=1 + 自建 client_id 才执行） ----------
    //
    // 只请求 Microsoft device code，**不打开浏览器、不完成授权、不写 settings**；
    // 不再使用任何内置/伪装 client_id：需要用户自己的 Azure public client ID：
    //   $env:NEOSONIA_VERIFY="1"; $env:NEOSONIA_MS_CLIENT_ID="<自己的 Azure client_id>"
    //   cargo test --offline -p neosonia-mail -- --ignored --nocapture verify_microsoft_device_code_network

    #[tokio::test]
    #[ignore = "真机验证：需要自己的 Azure public client；NEOSONIA_VERIFY=1 + NEOSONIA_MS_CLIENT_ID + 联网"]
    async fn verify_microsoft_device_code_network() {
        if !std::env::var("NEOSONIA_VERIFY")
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
        {
            eprintln!("[skip] NEOSONIA_VERIFY != 1");
            return;
        }
        let client_id = std::env::var("NEOSONIA_MS_CLIENT_ID").unwrap_or_default();
        let client_id = if client_id.trim().is_empty() {
            // 默认用 Microsoft Office 公共客户端 ID（用户指定）；也可用环境变量覆盖成自己的 Azure 应用。
            MICROSOFT_CLIENT_ID.to_string()
        } else {
            client_id
        };
        let cfg = MailOAuthSettings {
            enabled: true,
            provider: OAuthProvider::Microsoft,
            client_id,
            tenant: "common".into(),
            ..MailOAuthSettings::default()
        };
        let flow = OAuth::start_flow_with_settings(&cfg)
            .await
            .expect("真实 Microsoft device code 请求失败");
        println!("=== Microsoft device code 真机结果（不会完成授权、不会写 settings） ===");
        println!("verification_url: {}", flow.authorize_url);
        println!("user_code: {:?}", flow.user_code);
        println!("expires_in: {} 秒", flow.expires_in);
        println!("token_endpoint: {}", resolved_token_url(&cfg));
        println!("client_id: {}", resolved_client_id(&cfg));
        println!(
            "scope: {}（has_offline_access={}）",
            resolved_scope(&cfg),
            resolved_scope(&cfg).contains("offline_access")
        );
        assert!(flow.user_code.is_some());
        assert!(!flow.authorize_url.is_empty());
        flow.abort();
    }

    // ============================================================
    // 极简 mock HTTP 服务器（测试用）
    // ============================================================

    struct MockServer {
        addr: std::net::SocketAddr,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl MockServer {
        fn start(responses: Vec<String>) -> Self {
            Self::start_with_status(
                responses
                    .into_iter()
                    .map(|body| (200u16, body))
                    .collect(),
            )
        }

        fn start_with_status(responses: Vec<(u16, String)>) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock");
            listener.set_nonblocking(true).expect("nonblocking");
            let addr = listener.local_addr().expect("addr");
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = stop.clone();
            let handle = std::thread::spawn(move || {
                let mut queue = responses;
                let deadline = std::time::Instant::now() + Duration::from_secs(25);
                while !stop_thread.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                            let (status, body) = if queue.is_empty() {
                                (
                                    200u16,
                                    r#"{"error":"unexpected_request"}"#.to_string(),
                                )
                            } else {
                                queue.remove(0)
                            };
                            let mut buf = Vec::new();
                            let mut chunk = [0u8; 4096];
                            loop {
                                match stream.read(&mut chunk) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        buf.extend_from_slice(&chunk[..n]);
                                        if request_complete(&buf) {
                                            break;
                                        }
                                    }
                                    Err(e)
                                        if e.kind() == std::io::ErrorKind::WouldBlock
                                            || e.kind() == std::io::ErrorKind::TimedOut =>
                                    {
                                        // 非阻塞 listener + async reqwest：body 可能分多次到达，继续等。
                                        std::thread::sleep(Duration::from_millis(10));
                                        continue;
                                    }
                                    Err(_) => break,
                                }
                            }
                            let reason = if status == 200 {
                                "OK"
                            } else if status >= 400 && status < 500 {
                                "Bad Request"
                            } else {
                                "Error"
                            };
                            let response = format!(
                                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.as_bytes().len(),
                                body
                            );
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.flush();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(_) => break,
                    }
                }
            });
            MockServer {
                addr,
                stop,
                handle: Some(handle),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.addr.port())
        }

        fn finish(mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
}
