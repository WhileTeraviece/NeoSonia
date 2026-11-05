//! IMAP 收信 / SMTP 发信的实际网络实现。
//!
//! 要点：
//! - `imap` 2.4 是阻塞 API，所有网络操作都放进 `tokio::task::spawn_blocking`，
//!   外层再套 `tokio::time::timeout`，绝不阻塞 UI/agent 线程；
//! - TCP 连接用 `connect_timeout`，socket 设 read/write 超时（取 `timeout_secs`）；
//! - TLS 用 `native-tls`（Windows 走 SChannel，Linux 走 OpenSSL），证书由系统证书库校验；
//! - 任何失败都翻译成中文 `anyhow::Error`，不 panic、不带 emoji。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::SmtpTransport;
use lettre::{Address, Message, Transport};
use neosonia_core::config::{ImapSettings, MailOAuthSettings, Settings, SmtpSettings};
use neosonia_core::OAuthProvider;
use serde::{Deserialize, Serialize};

use crate::attachments;
use crate::oauth::{OAuth, XOAuth2Authenticator};
use crate::parse;
use crate::presets::{self, MailPreset};
use crate::{MailFull, MailSummary, OutgoingMail, MAX_LIST_LIMIT};

/// 连接/操作超时的保底值（秒）。
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// `spawn_blocking` 外层额外等待的宽限（秒），让底层 socket 超时先生效。
const TIMEOUT_GRACE_SECS: u64 = 15;

// ============================================================
// MailService
// ============================================================

/// 无状态的邮件服务：每次调用都从 `Settings` 快照构造，天然支持 UI 热改配置。
#[derive(Clone, Debug)]
pub struct MailService {
    imap: ImapSettings,
    smtp: SmtpSettings,
    oauth: MailOAuthSettings,
    /// 服务商预设 id（"outlook"/"gmail"/"163"/"126"/"qq"/"custom"）。
    provider: String,
    imap_password: String,
    smtp_password: String,
    default_folder: String,
    mark_read_on_reply: bool,
    signature: String,
    max_reply_chars: usize,
    max_body_chars: usize,
}

impl MailService {
    /// `docs/contracts.md` 6.7 节约定的构造方式。
    pub fn from_settings(s: &Settings) -> Self {
        let default_folder = {
            let m = s.mail.imap.mailbox.trim();
            if m.is_empty() { "INBOX".to_string() } else { m.to_string() }
        };
        let provider = {
            let p = s.mail.provider.trim().to_ascii_lowercase();
            if p.is_empty() { "custom".to_string() } else { p }
        };
        Self {
            imap: s.mail.imap.clone(),
            smtp: s.mail.smtp.clone(),
            oauth: s.mail.oauth.clone(),
            provider,
            imap_password: s.effective_imap_password(),
            smtp_password: s.effective_smtp_password(),
            default_folder,
            mark_read_on_reply: s.mail.mark_read,
            signature: s.mail.signature.clone(),
            max_reply_chars: s.mail.max_reply_chars.max(200),
            max_body_chars: s.mail.max_body_chars.max(1_000),
        }
    }

    /// 当前邮箱类型预设 id。
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// 当前预设（未知 id 返回 None）。
    pub fn preset(&self) -> Option<&'static MailPreset> {
        presets::preset(&self.provider)
    }

    /// `from_settings` 的别名（契约注释里写的是 `MailService::new(settings)`）。
    pub fn new(s: &Settings) -> Self {
        Self::from_settings(s)
    }

    /// 默认收件箱文件夹名。
    pub fn default_folder(&self) -> &str {
        &self.default_folder
    }

    /// IMAP 参数是否齐全（不校验网络）。OAuth 模式要求 refresh_token 与邮箱地址。
    pub fn imap_configured(&self) -> bool {
        if self.oauth.enabled {
            return !self.imap.host.trim().is_empty()
                && !self.effective_user().is_empty()
                && !self.oauth.refresh_token.trim().is_empty();
        }
        !self.imap.host.trim().is_empty()
            && !self.imap.user.trim().is_empty()
            && !self.imap_password.trim().is_empty()
    }

    /// SMTP 参数是否齐全（不校验网络）。OAuth 模式要求 refresh_token 与邮箱地址。
    pub fn smtp_configured(&self) -> bool {
        if self.oauth.enabled {
            return !self.smtp.host.trim().is_empty()
                && !self.smtp_from_address().is_empty()
                && !self.oauth.refresh_token.trim().is_empty();
        }
        !self.smtp.host.trim().is_empty()
            && !(self.smtp.user.trim().is_empty() && self.smtp_password.trim().is_empty())
    }

    /// 该地址是否允许主动发信（白名单或已允许发给任何人）。
    pub fn outbound_allowed(&self, settings: &Settings, addr: &str) -> bool {
        settings.mail.allow_outbound_to_anyone || settings.is_whitelisted(addr)
    }

    fn folder_or_default(&self, folder: &str) -> String {
        let f = folder.trim();
        if f.is_empty() {
            self.default_folder.clone()
        } else {
            f.to_string()
        }
    }

    fn imap_timeout(&self) -> u64 {
        clamp_timeout(self.imap.timeout_secs)
    }

    fn smtp_timeout(&self) -> u64 {
        clamp_timeout(self.smtp.timeout_secs)
    }

    /// 当前是否启用 OAuth2 登录（替代密码认证）。
    pub fn oauth_enabled(&self) -> bool {
        self.oauth.enabled
    }

    /// 实际用于 IMAP/SMTP 的用户名：优先配置的 user，OAuth 模式下回退到授权得到的 email。
    pub fn effective_user(&self) -> String {
        let configured = self.imap.user.trim();
        if !configured.is_empty() {
            return configured.to_string();
        }
        self.oauth.email.trim().to_string()
    }

    // --------------------------------------------------------
    // 配置校验
    // --------------------------------------------------------

    fn check_imap(&self) -> anyhow::Result<()> {
        if self.imap.host.trim().is_empty() {
            bail!(
                "{}（当前邮箱类型：{}）",
                presets::missing_config_hint(&self.provider),
                self.provider
            );
        }
        if self.oauth.enabled {
            if self.effective_user().is_empty() {
                bail!(
                    "已启用 OAuth2，但缺少邮箱地址：请先在设置页「邮箱账号」里填写完整邮箱地址（OAuth 授权不会返回邮箱），再点「测试连接」"
                );
            }
            if self.oauth.refresh_token.trim().is_empty() {
                bail!(
                    "授权返回缺少 refresh_token（流程未完成）：请重新点「用 Microsoft 登录 / 用 Google 登录」完成授权。\
                     若刚授权过，请确认设备码流程已显示“授权完成”且浏览器没有超时（15 分钟内完成）。"
                );
            }
            return Ok(());
        }
        if self.imap.user.trim().is_empty() {
            bail!("未配置 IMAP 账号：请在设置页填写完整邮箱地址（例如 12345678@qq.com）");
        }
        if self.imap_password.trim().is_empty() {
            let hint = self
                .preset()
                .map(|p| p.imap_hint.clone())
                .unwrap_or_else(|| "请填写邮箱密码/授权码".to_string());
            bail!("未配置 IMAP 密码：{hint}");
        }
        Ok(())
    }

    fn check_smtp(&self) -> anyhow::Result<()> {
        if self.smtp.host.trim().is_empty() {
            bail!(
                "{}（当前邮箱类型：{}）",
                presets::missing_config_hint(&self.provider),
                self.provider
            );
        }
        if !self.oauth.enabled && self.smtp_password.trim().is_empty() && self.smtp.user.trim().is_empty() {
            bail!(
                "未配置 SMTP 账号/密码：请在设置页填写邮箱账号和授权码（QQ/163 需要用授权码，不是登录密码）"
            );
        }
        if self.smtp_from_address().is_empty() {
            bail!("未配置发件人地址：请在设置页填写发件人 from，或填写邮箱账号（会自动作为发件人）");
        }
        if self.oauth.enabled && self.oauth.refresh_token.trim().is_empty() {
            bail!(
                "已启用 OAuth2 但还没有 refresh_token：请重新点「用 Microsoft 登录 / 用 Google 登录」完成授权"
            );
        }
        Ok(())
    }

    /// 给 UI/日志用的一句话配置状态（不含密码/token 明文）。
    pub fn configuration_help(&self) -> String {
        let mut parts = Vec::new();
        parts.push(format!("邮箱类型：{}", self.provider));
        parts.push(format!(
            "IMAP：{}（{}，TLS={}）",
            if self.imap.host.trim().is_empty() { "未配置" } else { self.imap.host.as_str() },
            self.imap.port,
            self.imap.tls
        ));
        parts.push(format!(
            "SMTP：{}（{}，隐式TLS={}，STARTTLS={}）",
            if self.smtp.host.trim().is_empty() { "未配置" } else { self.smtp.host.as_str() },
            self.smtp.port,
            self.smtp.implicit_tls,
            self.smtp.starttls
        ));
        if self.oauth.enabled {
            parts.push(format!(
                "OAuth2：{}，refresh_token={}，access_token={}",
                provider_label(self.oauth.provider),
                if self.oauth.refresh_token.trim().is_empty() { "缺失" } else { "已保存" },
                if self.oauth.access_token.trim().is_empty() { "空（会用 refresh_token 换）" } else { "已缓存" }
            ));
        } else {
            parts.push("登录方式：账号密码/授权码".to_string());
        }
        let imap_user = self.effective_user();
        parts.push(format!(
            "IMAP 用户：{}",
            if imap_user.is_empty() { "未配置" } else { imap_user.as_str() }
        ));
        parts.join("；")
    }

    /// OAuth 运行状态（不含 token 明文，只给 UI 展示/诊断）。
    pub fn oauth_status(&self) -> OAuthStatus {
        let oauth = OAuth::from_settings(&self.oauth);
        let snapshot = oauth.snapshot();
        let claims = crate::oauth::access_token_claims(&self.oauth.access_token);
        let token_present = !self.oauth.access_token.trim().is_empty();
        let claims_ready = !claims.audience.is_empty() || !claims.scopes.is_empty();
        // Microsoft：token 有 JWT claims 但缺 IMAP scope / audience 不是 Outlook 时，
        // 收信必然失败（"User is authenticated but not connected"），直接提示重新授权。
        let audience_bad = self.oauth.provider == OAuthProvider::Microsoft
            && claims_ready
            && !claims
                .audience
                .iter()
                .any(|aud| aud.to_ascii_lowercase().contains("outlook.office"));
        let scope_bad = token_present && claims_ready && !claims.has_imap_scope;
        let needs_reauth = crate::oauth::needs_reauth() || scope_bad || audience_bad;
        let status_message = if needs_reauth {
            if scope_bad {
                "需要重新授权：access_token 缺少 IMAP.AccessAsUser.All 权限（scope 不对或被管理员限制）".to_string()
            } else if audience_bad {
                "需要重新授权：access_token 的 audience 不是 Outlook/Exchange（scope 或资源不对）".to_string()
            } else {
                "需要重新授权：刷新令牌失效或被撤销，请重新点「用 Microsoft 登录」".to_string()
            }
        } else if self.oauth.enabled && !token_present && self.oauth.refresh_token.trim().is_empty() {
            "尚未完成 OAuth2 授权".to_string()
        } else if self.oauth.enabled && !token_present {
            "access_token 为空，将用 refresh_token 自动刷新".to_string()
        } else {
            "OAuth2 状态正常".to_string()
        };
        OAuthStatus {
            enabled: self.oauth.enabled,
            provider: provider_label(self.oauth.provider).to_string(),
            email: if self.oauth.email.trim().is_empty() {
                self.effective_user()
            } else {
                self.oauth.email.clone()
            },
            has_refresh_token: !snapshot.refresh_token.trim().is_empty(),
            access_token_valid: oauth.access_token_valid(),
            expires_at_ms: snapshot.expires_at_ms,
            client_id_hint: client_id_hint(&self.oauth),
            audience: claims.audience,
            scopes: claims.scopes,
            has_imap_scope: claims.has_imap_scope,
            has_smtp_scope: claims.has_smtp_scope,
            needs_reauth,
            status_message,
        }
    }

    /// IMAP 失败时把 access_token 的 aud/scp 打进日志（不含 token 本体），方便判断
    /// 是 Office 公共 client 的 scope 问题，还是账号本身没开 IMAP。
    fn log_oauth_token_claims(&self, context: &str) {
        if self.oauth.access_token.trim().is_empty() {
            tracing::warn!(context, "OAuth 诊断：access_token 为空，无法解析 aud/scp");
            return;
        }
        let claims = crate::oauth::access_token_claims(&self.oauth.access_token);
        tracing::warn!(
            context,
            audience = ?claims.audience,
            scopes = ?claims.scopes,
            has_imap_scope = claims.has_imap_scope,
            has_smtp_scope = claims.has_smtp_scope,
            provider = ?self.oauth.provider,
            "OAuth token claims 诊断（不含 token 明文）；若缺 IMAP.AccessAsUser.All 或 aud 非 Outlook，通常是 client_id/scope 问题；否则检查账号是否开启 IMAP"
        );
    }

    /// 同步连接诊断（IMAP 登录 + 未读数、SMTP EHLO/AUTH）。会发网络请求。
    pub fn test_connection_blocking(&self) -> ConnectionReport {
        let oauth_status = self.oauth_status();
        let imap = match self.check_imap() {
            Err(e) => {
                self.log_oauth_token_claims("IMAP 配置检查失败");
                ConnectionResult::fail("IMAP", e.to_string(), None)
            }
            Ok(()) => {
                let cfg = self.imap.clone();
                let password = self.imap_password.clone();
                let oauth = self.oauth.clone();
                let folder = self.default_folder.clone();
                match connect_imap(&cfg, &password, &oauth) {
                    Err(e) => {
                        self.log_oauth_token_claims("IMAP 登录失败");
                        ConnectionResult::fail("IMAP", e.to_string(), None)
                    }
                    Ok(mut conn) => {
                        let selected = conn.ops().select_folder(&folder);
                        let count = if selected.is_ok() {
                            conn.ops().search_uids("UNSEEN").map(|v| v.len()).unwrap_or(0)
                        } else {
                            0
                        };
                        let all = if selected.is_ok() {
                            conn.ops().search_uids("ALL").map(|v| v.len()).unwrap_or(0)
                        } else {
                            0
                        };
                        conn.logout();
                        match selected {
                            Ok(exists) => ConnectionResult::ok(
                                "IMAP",
                                format!(
                                    "已连接 {}:{}，文件夹 {} 共 {} 封（服务端 EXISTS={}，未读 {}）",
                                    cfg.host.trim(),
                                    cfg.port,
                                    folder,
                                    all,
                                    exists,
                                    count
                                ),
                            ),
                            Err(e) => {
                                self.log_oauth_token_claims("IMAP SELECT 失败");
                                ConnectionResult::fail("IMAP", e.to_string(), None)
                            }
                        }
                    }
                }
            }
        };
        let smtp = match self.check_smtp() {
            Err(e) => ConnectionResult::fail("SMTP", e.to_string(), None),
            Ok(()) => {
                let cfg = self.smtp.clone();
                let password = self.smtp_password.clone();
                let oauth = self.oauth.clone();
                match smtp_probe(&cfg, &password, &oauth) {
                    Err(e) => ConnectionResult::fail("SMTP", e.to_string(), None),
                    Ok(()) => ConnectionResult::ok(
                        "SMTP",
                        format!(
                            "已连接 {}:{} 并完成认证（{}）",
                            cfg.host.trim(),
                            cfg.port,
                            smtp_security_label(&cfg)
                        ),
                    ),
                }
            }
        };

        let mut summary = String::new();
        if imap.ok && smtp.ok {
            summary = format!("连接正常：{}；{}", imap.message, smtp.message);
        } else {
            if !imap.ok {
                summary.push_str(&format!("IMAP 失败：{}", imap.message));
            }
            if !smtp.ok {
                if !summary.is_empty() {
                    summary.push_str("；");
                }
                summary.push_str(&format!("SMTP 失败：{}", smtp.message));
            }
            summary.push_str("（详细建议见上面各条）");
        }
        ConnectionReport { imap, smtp, oauth: oauth_status, summary }
    }

    /// 异步连接诊断（app/UI 用；内部 spawn_blocking）。
    pub async fn test_connection(&self) -> ConnectionReport {
        let this = self.clone();
        match tokio::task::spawn_blocking(move || this.test_connection_blocking()).await {
            Ok(report) => report,
            Err(join_err) => {
                let message = format!("连接诊断后台线程异常结束：{join_err}");
                ConnectionReport {
                    imap: ConnectionResult::fail("IMAP", message.clone(), None),
                    smtp: ConnectionResult::fail("SMTP", message, None),
                    oauth: self.oauth_status(),
                    summary: "连接诊断失败（内部线程错误），请重试".to_string(),
                }
            }
        }
    }

    fn smtp_from_address(&self) -> String {
        let from = self.smtp.from.trim();
        if !from.is_empty() {
            return from.to_string();
        }
        let user = self.smtp.user.trim();
        if !user.is_empty() {
            return user.to_string();
        }
        self.oauth.email.trim().to_string()
    }

    // --------------------------------------------------------
    // IMAP
    // --------------------------------------------------------

    /// 测试 IMAP 连接，返回 `"OK uid=..."`。
    pub async fn test_imap(&self) -> anyhow::Result<String> {
        self.check_imap()?;
        let cfg = self.imap.clone();
        let password = self.imap_password.clone();
        let oauth = self.oauth.clone();
        let folder = self.default_folder.clone();
        let timeout = self.imap_timeout();
        run_blocking(timeout, "测试 IMAP 连接", move || {
            let mut conn = connect_imap(&cfg, &password, &oauth)?;
            conn.ops().select_folder(&folder)?;
            let uids = conn.ops().search_uids("ALL")?;
            let max = uids.iter().copied().max().unwrap_or(0);
            let count = uids.len();
            conn.logout();
            Ok(format!(
                "OK uid={max}（{}:{}，文件夹 {} 共 {} 封邮件）",
                cfg.host.trim(),
                cfg.port,
                folder,
                count
            ))
        })
        .await
    }

    /// 列出所有可选文件夹。
    pub async fn folders(&self) -> anyhow::Result<Vec<String>> {
        self.check_imap()?;
        let cfg = self.imap.clone();
        let password = self.imap_password.clone();
        let oauth = self.oauth.clone();
        let timeout = self.imap_timeout();
        run_blocking(timeout, "获取邮件文件夹列表", move || {
            let mut conn = connect_imap(&cfg, &password, &oauth)?;
            let list = conn.ops().list_folders()?;
            conn.logout();
            Ok(list)
        })
        .await
    }

    /// 列出邮件摘要。`limit` 会被限制在 1..=MAX_LIST_LIMIT，**按 UID 降序返回（最新的在最前）**。
    ///
    /// 每次调用都会**新建 IMAP 连接**（不复用旧 session），SELECT 后先 NOOP 再 UID SEARCH，
    /// 并且列表只取「完整头 + 正文前 1KB」而不是整封邮件——Outlook 上拉 40 封完整正文要
    /// 30-45 秒，会超过 app 的超时导致刷新失败/看起来“列表不更新”；摘要只需头和预览。
    pub async fn list(
        &self,
        folder: &str,
        limit: usize,
        unread_only: bool,
    ) -> anyhow::Result<Vec<MailSummary>> {
        self.check_imap()?;
        let cfg = self.imap.clone();
        let password = self.imap_password.clone();
        let oauth = self.oauth.clone();
        let folder = self.folder_or_default(folder);
        let folder_label = folder.clone();
        let limit = limit.clamp(1, MAX_LIST_LIMIT);
        let timeout = self.imap_timeout();
        run_blocking(timeout, "拉取邮件列表", move || {
            let started = std::time::Instant::now();
            let mut conn = connect_imap(&cfg, &password, &oauth)?;
            let t_connect = started.elapsed().as_millis() as u64;
            let exists = conn.ops().select_folder(&folder)?;
            let t_select = started.elapsed().as_millis() as u64;
            // SELECT 之后 NOOP：服务端会把待推送的 EXISTS/FLAGS 状态吐出来，确保不是旧状态。
            conn.ops().noop()?;
            let query = if unread_only { "UNSEEN" } else { "ALL" };
            let mut uids = conn.ops().search_uids(query)?;
            let t_search = started.elapsed().as_millis() as u64;
            uids.sort_unstable();
            let searched = uids.len();
            let newest = uids.last().copied().unwrap_or(0);
            let skip = uids.len().saturating_sub(limit);
            let selected: Vec<u32> = uids.into_iter().skip(skip).collect();
            let mut out = Vec::new();
            if !selected.is_empty() {
                let fetched = conn.ops().fetch_summaries(&selected)?;
                for f in fetched {
                    let unread = !f.seen;
                    if unread_only && !unread {
                        continue;
                    }
                    let raw = crate::parse::compose_partial_message(&f.header, &f.text);
                    out.push(parse::parse_summary(f.uid, &raw, unread));
                }
            }
            let t_fetch = started.elapsed().as_millis() as u64;
            out.sort_by(|a, b| b.uid.cmp(&a.uid));
            // 最新的在最前：UI/模型直接按返回顺序展示就是“新邮件在最上面”。
            conn.logout();
            tracing::info!(
                host = cfg.host.trim(),
                port = cfg.port,
                folder = %folder,
                exists,
                searched,
                returned = out.len(),
                newest_uid = newest,
                unread_only,
                connect_ms = t_connect,
                select_ms = t_select,
                search_ms = t_search,
                fetch_ms = t_fetch,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "IMAP 拉取邮件列表完成（新建连接 + SELECT + NOOP + UID SEARCH + 头/预览 FETCH）"
            );
            Ok(out)
        })
        .await
        .map_err(|e| {
            tracing::warn!(
                folder = %folder_label,
                error = %e,
                "IMAP 拉取邮件列表失败"
            );
            e
        })
    }

    /// 取一封完整邮件。
    pub async fn fetch(&self, uid: u32, folder: &str) -> anyhow::Result<MailFull> {
        self.check_imap()?;
        let cfg = self.imap.clone();
        let password = self.imap_password.clone();
        let oauth = self.oauth.clone();
        let folder = self.folder_or_default(folder);
        let timeout = self.imap_timeout();
        run_blocking(timeout, "读取邮件正文", move || {
            let mut conn = connect_imap(&cfg, &password, &oauth)?;
            conn.ops().select_folder(&folder)?;
            let fetched = conn.ops().fetch_messages(&[uid])?;
            conn.logout();
            let Some(f) = fetched.into_iter().next() else {
                bail!("文件夹 {} 里没有找到 uid={} 的邮件（可能已被删除或移动）", folder, uid);
            };
            if f.raw.is_empty() {
                bail!("uid={} 的邮件正文为空（服务器可能拒绝返回内容）", uid);
            }
            Ok(parse::parse_full(uid, &f.raw))
        })
        .await
    }

    /// 标记已读/未读。
    pub async fn mark_read(&self, uid: u32, read: bool, folder: &str) -> anyhow::Result<()> {
        self.check_imap()?;
        let cfg = self.imap.clone();
        let password = self.imap_password.clone();
        let oauth = self.oauth.clone();
        let folder = self.folder_or_default(folder);
        let timeout = self.imap_timeout();
        run_blocking(timeout, "修改邮件已读状态", move || {
            let mut conn = connect_imap(&cfg, &password, &oauth)?;
            conn.ops().select_folder(&folder)?;
            conn.ops().store_seen(uid, read)?;
            conn.logout();
            Ok(())
        })
        .await
    }

    // --------------------------------------------------------
    // SMTP
    // --------------------------------------------------------

    /// 发送一封邮件。
    pub async fn send(&self, msg: OutgoingMail) -> anyhow::Result<()> {
        self.check_smtp()?;
        if !msg.has_recipients() {
            bail!("收件人为空：请在 to 字段填写至少一个邮箱地址");
        }
        let cfg = self.smtp.clone();
        let password = self.smtp_password.clone();
        let oauth = self.oauth.clone();
        let timeout = self.smtp_timeout();
        let raw = build_message(&cfg, &oauth, &msg)?;
        run_blocking(timeout, "发送邮件", move || {
            smtp_send_with_retry(&cfg, &password, &oauth, &raw)
        })
        .await
    }

    /// 回复指定 uid 的邮件：自动带 `Re:` 前缀与 `In-Reply-To` / `References`，成功后按配置标记已读。
    pub async fn reply(&self, uid: u32, body: &str, folder: &str) -> anyhow::Result<()> {
        let original = self.fetch(uid, folder).await?;
        if original.from.trim().is_empty() {
            bail!("无法确定 uid={} 这封邮件的发件人地址，不能回复", uid);
        }
        let subject = reply_subject(&original.subject);
        let mut references = original.references.clone();
        if !original.message_id.trim().is_empty() {
            references.push(original.message_id.clone());
        }
        let outgoing = OutgoingMail {
            to: vec![original.from.clone()],
            cc: Vec::new(),
            subject,
            body: compose_reply_body(body, &self.signature, self.max_reply_chars),
            in_reply_to: if original.message_id.trim().is_empty() {
                None
            } else {
                Some(original.message_id.clone())
            },
            references,
            attachments: Vec::new(),
        };
        self.send(outgoing).await?;
        if self.mark_read_on_reply {
            // 标记已读失败不影响"回复已发出"这个事实，只记日志。
            if let Err(e) = self.mark_read(uid, true, folder).await {
                tracing::warn!(uid, error = %e, "回复成功但标记已读失败");
            }
        }
        Ok(())
    }

    /// 截断过长正文用的上限（给工具层用）。
    pub fn max_body_chars(&self) -> usize {
        self.max_body_chars
    }

    /// 测试 SMTP 连接（只发 NOOP，不真的发信）。
    pub async fn test_smtp(&self) -> anyhow::Result<String> {
        self.check_smtp()?;
        let cfg = self.smtp.clone();
        let password = self.smtp_password.clone();
        let oauth = self.oauth.clone();
        let timeout = self.smtp_timeout();
        let mode = smtp_tls_mode(&cfg);
        run_blocking(timeout, "测试 SMTP 连接", move || {
            smtp_probe(&cfg, &password, &oauth)?;
            Ok(format!(
                "OK {}:{}（{}，{}）",
                cfg.host.trim(),
                cfg.port,
                mode.describe(),
                smtp_from(&cfg, &oauth)
            ))
        })
        .await
    }
}

// ============================================================
// IMAP 实现（阻塞，只在 spawn_blocking 里跑）
// ============================================================

struct Fetched {
    raw: Vec<u8>,
}

/// 一次 IMAP 连接（阻塞 socket / TLS 流）。
///
/// - 直连 TLS / 明文连接会被 [`IdFilter`] 包一层，用来吞掉 `ID` 命令的
///   `* ID (...)` 响应（imap-proto 0.10 不认识这个扩展响应，不吞会解析失败）；
/// - STARTTLS 升级后的连接无法再包（imap crate 的 `secure()` 不暴露底层流），
///   这类连接不发送 IMAP ID，163/126 请使用 993 直连 TLS。
enum ImapConn {
    Tls(SessionOps<IdFilter<native_tls::TlsStream<TcpStream>>>),
    StartTls(SessionOps<native_tls::TlsStream<TcpStream>>),
    Plain(SessionOps<IdFilter<TcpStream>>),
}

impl ImapConn {
    fn ops(&mut self) -> &mut dyn ImapOps {
        match self {
            ImapConn::Tls(s) => s,
            ImapConn::StartTls(s) => s,
            ImapConn::Plain(s) => s,
        }
    }

    fn logout(&mut self) {
        self.ops().logout();
    }
}

/// 转发读写流的包装器：在写了 `ID (...)` 命令之后，把紧随其后的
/// `* ID (...)` 未标记响应行吞掉，只把带 tag 的最终响应交给 imap 解析。
struct IdFilter<S> {
    inner: S,
    /// 等待 ID 命令的最终响应（期间吞掉 `* ID` 行）。
    awaiting_id: bool,
    /// 已处理、等待交给 imap 的字节。
    out: Vec<u8>,
    out_pos: usize,
    /// 等待 ID 响应时不完整的尾行。
    tail: Vec<u8>,
}

impl<S> IdFilter<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            awaiting_id: false,
            out: Vec::new(),
            out_pos: 0,
            tail: Vec::new(),
        }
    }

    fn note_command(&mut self, bytes: &[u8]) {
        if bytes.len() < 4 || self.awaiting_id {
            return;
        }
        let upper = bytes.to_ascii_uppercase();
        if find_subslice(&upper, b"ID (").is_some() {
            self.awaiting_id = true;
            self.out.clear();
            self.out_pos = 0;
            self.tail.clear();
        }
    }

    fn process_id_response(&mut self, data: &[u8]) {
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(data);
        let mut offset = 0usize;
        while let Some(pos) = find_subslice(&buf[offset..], b"\r\n") {
            let line_end = offset + pos + 2;
            let line = &buf[offset..line_end];
            offset = line_end;
            if self.awaiting_id {
                if line_starts_ignore_ascii_case(line, b"* ID ") {
                    // 吞掉 ID 扩展响应
                    continue;
                }
                self.out.extend_from_slice(line);
                if !line.starts_with(b"* ") && !line.starts_with(b"+ ") {
                    // 带 tag 的最终响应（OK/BAD/NO）出现，ID 事务结束
                    self.awaiting_id = false;
                }
                if !self.awaiting_id {
                    self.out.extend_from_slice(&buf[offset..]);
                    offset = buf.len();
                    break;
                }
            } else {
                self.out.extend_from_slice(line);
            }
        }
        if offset < buf.len() {
            if self.awaiting_id {
                self.tail = buf[offset..].to_vec();
            } else {
                self.out.extend_from_slice(&buf[offset..]);
            }
        }
    }
}

impl<S: Read> Read for IdFilter<S> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.out_pos < self.out.len() {
                let n = (self.out.len() - self.out_pos).min(out.len());
                out[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
                self.out_pos += n;
                if self.out_pos >= self.out.len() {
                    self.out.clear();
                    self.out_pos = 0;
                }
                return Ok(n);
            }
            if !self.awaiting_id {
                return self.inner.read(out);
            }
            let mut chunk = [0u8; 8192];
            let n = self.inner.read(&mut chunk)?;
            if n == 0 {
                return Ok(0);
            }
            self.process_id_response(&chunk[..n]);
            if self.out.is_empty() {
                // 整段都是 `* ID ...`，继续等最终响应
                continue;
            }
        }
    }
}

impl<S: Write> Write for IdFilter<S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.note_command(buf);
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn line_starts_ignore_ascii_case(line: &[u8], prefix: &[u8]) -> bool {
    line.len() >= prefix.len()
        && line[..prefix.len()].eq_ignore_ascii_case(prefix)
}

struct SessionOps<T: Read + Write> {
    session: imap::Session<T>,
}

/// 列表摘要用的「头 + 正文预览」片段（不下载整封邮件）。
struct SummaryFetched {
    uid: u32,
    seen: bool,
    header: Vec<u8>,
    /// 正文前若干字节（BODY.PEEK[TEXT]<0.N>），只用于列表预览。
    text: Vec<u8>,
}

trait ImapOps {
    fn select_folder(&mut self, folder: &str) -> anyhow::Result<u32>;
    fn noop(&mut self) -> anyhow::Result<()>;
    fn list_folders(&mut self) -> anyhow::Result<Vec<String>>;
    fn search_uids(&mut self, query: &str) -> anyhow::Result<Vec<u32>>;
    fn fetch_summaries(&mut self, uids: &[u32]) -> anyhow::Result<Vec<SummaryFetched>>;
    fn fetch_messages(&mut self, uids: &[u32]) -> anyhow::Result<Vec<Fetched>>;
    fn store_seen(&mut self, uid: u32, seen: bool) -> anyhow::Result<()>;
    fn logout(&mut self);
}

impl<T: Read + Write> ImapOps for SessionOps<T> {
    fn select_folder(&mut self, folder: &str) -> anyhow::Result<u32> {
        self.session
            .select(folder)
            .map(|mailbox| mailbox.exists)
            .map_err(|e| imap_error("选择文件夹", folder, e))
    }

    fn noop(&mut self) -> anyhow::Result<()> {
        self.session
            .noop()
            .map_err(|e| imap_raw_error("发送 NOOP 刷新邮箱状态", e))
    }

    fn list_folders(&mut self) -> anyhow::Result<Vec<String>> {
        let names = self
            .session
            .list(None, Some("*"))
            .map_err(|e| imap_raw_error("获取文件夹列表", e))?;
        let mut out: Vec<String> = names
            .iter()
            .filter(|name| {
                !name
                    .attributes()
                    .iter()
                    .any(|a| matches!(a, imap::types::NameAttribute::NoSelect))
            })
            .map(|name| name.name().to_string())
            .filter(|n| !n.trim().is_empty())
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn search_uids(&mut self, query: &str) -> anyhow::Result<Vec<u32>> {
        let set = self
            .session
            .uid_search(query)
            .map_err(|e| imap_raw_error("搜索邮件", e))?;
        let mut uids: Vec<u32> = set.into_iter().collect();
        uids.sort_unstable();
        Ok(uids)
    }

    fn fetch_summaries(&mut self, uids: &[u32]) -> anyhow::Result<Vec<SummaryFetched>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let set = uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        // 只要头 + 正文前 4KB：比 BODY.PEEK[] 快一个数量级（Outlook 全量 40 封要 30-45s）。
        let fetched = self
            .session
            .uid_fetch(
                &set,
                "(UID FLAGS BODY.PEEK[HEADER] BODY.PEEK[TEXT]<0.1024>)",
            )
            .map_err(|e| imap_raw_error("下载邮件摘要", e))?;
        let mut out = Vec::with_capacity(fetched.len());
        for f in fetched.iter() {
            let uid = f.uid.unwrap_or(0);
            let seen = f
                .flags()
                .iter()
                .any(|flag| matches!(flag, imap::types::Flag::Seen));
            let header = f.header().map(|b| b.to_vec()).unwrap_or_default();
            let text = f.text().map(|b| b.to_vec()).unwrap_or_default();
            out.push(SummaryFetched {
                uid,
                seen,
                header,
                text,
            });
        }
        Ok(out)
    }

    fn fetch_messages(&mut self, uids: &[u32]) -> anyhow::Result<Vec<Fetched>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let set = uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        let fetched = self
            .session
            .uid_fetch(&set, "(UID FLAGS BODY.PEEK[])")
            .map_err(|e| imap_raw_error("下载邮件内容", e))?;
        let mut out = Vec::with_capacity(fetched.len());
        for f in fetched.iter() {
            let raw = f.body().map(|b| b.to_vec()).unwrap_or_default();
            out.push(Fetched { raw });
        }
        Ok(out)
    }

    fn store_seen(&mut self, uid: u32, seen: bool) -> anyhow::Result<()> {
        let op = if seen { "+FLAGS (\\Seen)" } else { "-FLAGS (\\Seen)" };
        self.session
            .uid_store(uid.to_string(), op)
            .map(|_| ())
            .map_err(|e| imap_raw_error("修改邮件标记", e))
    }

    fn logout(&mut self) {
        let _ = self.session.logout();
    }
}

/// IMAP 连接模式：由 `tls` + 端口显式决定，**不做跨模式回退**。
///
/// 之前 993 失败会再试 STARTTLS，结果错误里混进 10060 之类误导信息；
/// 现在 993 只走直连 TLS、143 才走 STARTTLS，错误分别写清楚。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImapMode {
    ImplicitTls,
    StartTls,
    Plain,
}

fn imap_mode(tls: bool, port: u16) -> ImapMode {
    if !tls {
        ImapMode::Plain
    } else if port == 143 {
        ImapMode::StartTls
    } else {
        ImapMode::ImplicitTls
    }
}

/// 建立 IMAP 连接并登录（密码或 XOAUTH2）。
///
/// - `tls = true`：143 端口走 STARTTLS，其它端口（含 993）只走直连 TLS；
/// - `tls = false`：明文连接；
/// - 三种模式互不回退，失败信息分别标注“直连 TLS / STARTTLS / 明文”。
/// - `oauth.enabled = true` 时用 access_token + XOAUTH2 认证，密码可为空；
///   认证失败会自动强制刷新一次 token 再试。
/// - 163/126/QQ 在登录后自动发送 `ID` 命令（自报 NeoSonia）。
fn connect_imap(
    cfg: &ImapSettings,
    password: &str,
    oauth: &MailOAuthSettings,
) -> anyhow::Result<ImapConn> {
    let host = cfg.host.trim();
    if host.is_empty() {
        bail!("IMAP 主机为空，请在设置页选择邮箱类型或手动填写");
    }
    let timeout = clamp_timeout(cfg.timeout_secs);
    let user = imap_login_user(cfg, oauth);
    let id_required = presets::imap_id_required("", host);
    let mode = imap_mode(cfg.tls, cfg.port);
    match mode {
        ImapMode::ImplicitTls => login_implicit(cfg, host, &user, password, oauth, timeout, id_required)
            .map_err(|e| {
                anyhow!(
                    "IMAP 直连 TLS 连接失败（{host}:{}）：{e}\n\
                     该端口按直连 TLS（993 常见）处理，未回退 STARTTLS。\
                     请确认端口与加密方式（993=直连 TLS，143=STARTTLS）、证书受信任、网络/代理正常。",
                    cfg.port
                )
            }),
        ImapMode::StartTls => login_starttls(cfg, host, &user, password, oauth, timeout).map_err(|e| {
            anyhow!(
                "IMAP STARTTLS 连接失败（{host}:{}）：{e}\n\
                 143 端口按 STARTTLS 处理。若服务器要求明文，请在设置页把 IMAP TLS 关闭；\
                 若服务器提供 993 直连 TLS，建议改用 993。",
                cfg.port
            )
        }),
        ImapMode::Plain => {
            let tcp = tcp_connect(host, cfg.port, timeout)?;
            let mut client = imap::Client::new(IdFilter::new(tcp));
            client
                .read_greeting()
                .map_err(|e| imap_error("读取 IMAP 服务器问候语", host, e))?;
            let mut session = authenticate_imap(client, cfg, host, &user, password, oauth)?;
            if id_required {
                send_imap_id(&mut session);
            }
            Ok(ImapConn::Plain(SessionOps { session }))
        }
    }
}

fn login_implicit(
    cfg: &ImapSettings,
    host: &str,
    user: &str,
    password: &str,
    oauth: &MailOAuthSettings,
    timeout: u64,
    id_required: bool,
) -> anyhow::Result<ImapConn> {
    let tcp = tcp_connect(host, cfg.port, timeout)?;
    let connector = tls_connector()?;
    let stream = connector
        .connect(host, tcp)
        .map_err(|e| tls_error(host, cfg.port, e))?;
    let mut client = imap::Client::new(IdFilter::new(stream));
    client
        .read_greeting()
        .map_err(|e| imap_error("读取 IMAP 服务器问候语", host, e))?;
    let mut session = authenticate_imap(client, cfg, host, user, password, oauth)?;
    if id_required {
        send_imap_id(&mut session);
    }
    Ok(ImapConn::Tls(SessionOps { session }))
}

fn login_starttls(
    cfg: &ImapSettings,
    host: &str,
    user: &str,
    password: &str,
    oauth: &MailOAuthSettings,
    timeout: u64,
) -> anyhow::Result<ImapConn> {
    let tcp = tcp_connect(host, cfg.port, timeout)?;
    let connector = tls_connector()?;
    let mut client = imap::Client::new(tcp);
    client
        .read_greeting()
        .map_err(|e| imap_error("读取 IMAP 服务器问候语", host, e))?;
    let client = client
        .secure(host, &connector)
        .map_err(|e| imap_error("STARTTLS 升级", host, e))?;
    let session = authenticate_imap(client, cfg, host, user, password, oauth)?;
    if presets::imap_id_required("", host) {
        tracing::warn!(
            host,
            "该服务商需要 IMAP ID，但 STARTTLS 连接无法发送（imap crate 限制）；请改用 993 直连 TLS"
        );
    }
    Ok(ImapConn::StartTls(SessionOps { session }))
}

/// 登录后发送 IMAP ID（自报 NeoSonia）。失败只记日志，不影响后续命令。
fn send_imap_id<T: Read + Write>(session: &mut imap::Session<T>) -> bool {
    let command = presets::imap_id_command();
    match session.run_command_and_check_ok(&command) {
        Ok(()) => {
            tracing::debug!("IMAP ID 命令已发送（name=NeoSonia）");
            true
        }
        Err(e) => {
            tracing::debug!(error = %e, "IMAP ID 命令失败（继续使用连接）");
            false
        }
    }
}

/// 用密码或 XOAUTH2 登录 IMAP。
///
/// OAuth 模式：先用缓存的 access_token；如果服务器拒绝（NO/BAD），强制刷新
/// refresh_token 后**在同一条连接上重试一次**（微软 token 有时提前失效）。
fn authenticate_imap<T: Read + Write>(
    client: imap::Client<T>,
    cfg: &ImapSettings,
    host: &str,
    user: &str,
    password: &str,
    oauth: &MailOAuthSettings,
) -> anyhow::Result<imap::Session<T>> {
    if !oauth.enabled {
        return match client.login(user, password) {
            Ok(session) => {
                crate::oauth::clear_needs_reauth();
                Ok(session)
            }
            Err((e, _)) => Err(imap_login_error(host, cfg.port, e)),
        };
    }
    if user.trim().is_empty() {
        bail!("OAuth 模式缺少登录邮箱地址：请在设置页填写邮箱地址或重新完成 OAuth 授权");
    }
    let mut oauth_client = OAuth::from_settings(oauth);
    let token = oauth_client
        .access_token()
        .map_err(|e| anyhow!("IMAP OAuth2 获取 access_token 失败：{e}"))?;
    let authenticator = XOAuth2Authenticator::new(user, &token);
    match client.authenticate("XOAUTH2", &authenticator) {
        Ok(session) => {
            crate::oauth::clear_needs_reauth();
            Ok(session)
        }
        Err((first_error, returned_client)) => {
            let retryable = matches!(&first_error, imap::Error::No(_) | imap::Error::Bad(_));
            if !retryable {
                return Err(imap_oauth_error(host, cfg.port, first_error));
            }
            tracing::warn!(
                host,
                "IMAP XOAUTH2 使用缓存 token 失败，强制刷新 access_token 后重试一次"
            );
            let fresh = match oauth_client.access_token_force() {
                Ok(token) => token,
                Err(refresh_error) => {
                    return Err(anyhow!(
                        "IMAP XOAUTH2 认证失败（{host}:{}）：{first_error}；\
                         强制刷新 refresh_token 也失败：{refresh_error}。\
                         请在设置页重新点「用 Microsoft 登录 / 用 Google 登录」完成授权。",
                        cfg.port
                    ))
                }
            };
            let authenticator = XOAuth2Authenticator::new(user, &fresh);
            match returned_client.authenticate("XOAUTH2", &authenticator) {
                Ok(session) => {
                    crate::oauth::clear_needs_reauth();
                    Ok(session)
                }
                Err((e, _)) => Err(imap_oauth_error(host, cfg.port, e)),
            }
        }
    }
}

/// OAuth 模式下取一次 access_token（`force=true` 时忽略缓存强制刷新）。
fn resolve_oauth_token(oauth: &MailOAuthSettings, force: bool) -> anyhow::Result<Option<String>> {
    if !oauth.enabled {
        return Ok(None);
    }
    let oauth_client = OAuth::from_settings(oauth);
    let token = if force {
        oauth_client.access_token_force()
    } else {
        let mut client = OAuth::from_settings(oauth);
        client.access_token()
    }
    .map_err(|e| anyhow!("OAuth2 获取 access_token 失败：{e}"))?;
    if token.trim().is_empty() {
        bail!("OAuth2 返回的 access_token 为空：请在设置页重新完成邮箱授权");
    }
    Ok(Some(token))
}

/// IMAP 登录名：优先 `imap.user`，OAuth 模式回退到 `oauth.email`。
fn imap_login_user(cfg: &ImapSettings, oauth: &MailOAuthSettings) -> String {
    let user = cfg.user.trim();
    if !user.is_empty() {
        return user.to_string();
    }
    oauth.email.trim().to_string()
}

/// IMAP 服务器错误里是否包含“需要重新授权”的信号。
fn imap_auth_needs_reauth_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("authenticated but not connected")
        || lower.contains("invalid_grant")
        || lower.contains("insufficient")
        || lower.contains("user is authenticated")
}

fn apply_imap_reauth_hint(base: String, e: &imap::Error) -> anyhow::Error {
    let text = e.to_string();
    if imap_auth_needs_reauth_text(&text) {
        crate::oauth::mark_needs_reauth(&text);
        anyhow!(
            "{base}\n需要重新授权（缺 IMAP 权限/受众不对）：请在设置页重新点「用 Microsoft 登录 / 用 Google 登录」\
             完成授权，然后点「测试连接」。"
        )
    } else {
        anyhow!("{base}")
    }
}

fn imap_oauth_error(host: &str, port: u16, e: imap::Error) -> anyhow::Error {
    let base = match &e {
        imap::Error::No(msg) => format!(
            "IMAP XOAUTH2 登录失败（{host}:{port}）：服务器拒绝：{msg}。\
             常见原因：access_token 失效/被吊销、该账号未开启 IMAP、OAuth 应用未被该邮箱允许、或 scope 少了 IMAP 权限。\
             请重新点「用 Microsoft 登录 / 用 Google 登录」完成授权"
        ),
        imap::Error::Bad(msg) => format!(
            "IMAP XOAUTH2 登录失败（{host}:{port}）：服务器返回 BAD：{msg}（该服务器可能不支持 XOAUTH2）"
        ),
        imap::Error::Tls(_) | imap::Error::TlsHandshake(_) => format!(
            "IMAP XOAUTH2 登录失败（{host}:{port}）：TLS 握手失败，可能是端口与加密方式不匹配或证书不受信任。底层信息：{e}"
        ),
        imap::Error::Io(err) => format!("IMAP XOAUTH2 登录失败（{host}:{port}）：网络错误：{err}"),
        _ => format!("IMAP XOAUTH2 登录失败（{host}:{port}）：{e}"),
    };
    let base = match presets::diagnose_hint("", host, false, &e.to_string()) {
        Some(hint) => format!("{base}\n提示：{hint}"),
        None => base,
    };
    apply_imap_reauth_hint(base, &e)
}

fn tls_connector() -> anyhow::Result<native_tls::TlsConnector> {
    native_tls::TlsConnector::builder().build().map_err(|e| {
        anyhow!(
            "初始化 TLS 失败：{}。可能是系统缺少根证书库（Linux 请安装 ca-certificates / OpenSSL）",
            e
        )
    })
}

fn tcp_connect(host: &str, port: u16, timeout_secs: u64) -> anyhow::Result<TcpStream> {
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let addrs = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("无法解析 IMAP 服务器地址 {host}:{port}，请检查主机名拼写或网络"))?;
    let mut last_err: Option<std::io::Error> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(timeout));
                let _ = stream.set_write_timeout(Some(timeout));
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow!(
        "连接 IMAP 服务器 {host}:{port} 失败：{}（请检查地址/端口/防火墙，或是否需要代理）",
        last_err.map(|e| e.to_string()).unwrap_or_else(|| "没有可用的地址".to_string())
    ))
}

// ============================================================
// SMTP 实现
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SmtpTlsMode {
    /// 465：连接即 TLS。
    Implicit,
    /// 587：明文连接后 STARTTLS。
    StartTls,
    /// 25 或自建中继：明文（不安全）。
    None,
}

impl SmtpTlsMode {
    fn describe(self) -> &'static str {
        match self {
            SmtpTlsMode::Implicit => "直连 TLS",
            SmtpTlsMode::StartTls => "STARTTLS",
            SmtpTlsMode::None => "明文（不安全）",
        }
    }
}

/// 根据配置推断 SMTP 加密方式：
/// `implicit_tls = true` -> 直连 TLS；`starttls = true` -> STARTTLS；
/// 两者都没开时按端口猜（465 直连 TLS、587 STARTTLS、其它明文）。
fn smtp_tls_mode(cfg: &SmtpSettings) -> SmtpTlsMode {
    if cfg.implicit_tls {
        SmtpTlsMode::Implicit
    } else if cfg.starttls {
        SmtpTlsMode::StartTls
    } else if cfg.port == 465 {
        SmtpTlsMode::Implicit
    } else if cfg.port == 587 {
        SmtpTlsMode::StartTls
    } else {
        SmtpTlsMode::None
    }
}

/// SMTP 登录名：优先 `smtp.user`，其次 `smtp.from`，OAuth 模式回退到 `oauth.email`。
fn smtp_login_user(cfg: &SmtpSettings, oauth: &MailOAuthSettings) -> String {
    let user = cfg.user.trim();
    if !user.is_empty() {
        return user.to_string();
    }
    let from = cfg.from.trim();
    if !from.is_empty() {
        return from.to_string();
    }
    oauth.email.trim().to_string()
}

/// 发件人地址：`smtp.from` 优先，其次 `smtp.user`，OAuth 模式回退到 `oauth.email`。
fn smtp_from(cfg: &SmtpSettings, oauth: &MailOAuthSettings) -> String {
    let from = cfg.from.trim();
    if !from.is_empty() {
        return from.to_string();
    }
    let user = cfg.user.trim();
    if !user.is_empty() {
        return user.to_string();
    }
    oauth.email.trim().to_string()
}

fn build_transport(
    cfg: &SmtpSettings,
    password: &str,
    oauth: &MailOAuthSettings,
    force_refresh: bool,
) -> anyhow::Result<SmtpTransport> {
    let host = cfg.host.trim();
    if host.is_empty() {
        bail!("SMTP 主机为空，请在设置页选择邮箱类型或手动填写");
    }
    let mode = smtp_tls_mode(cfg);
    let mut builder = match mode {
        SmtpTlsMode::Implicit => SmtpTransport::relay(host).map_err(|e| {
            anyhow!("初始化 SMTP TLS 失败（{host}）：{e}。请确认端口与加密方式是否匹配")
        })?,
        SmtpTlsMode::StartTls => SmtpTransport::starttls_relay(host).map_err(|e| {
            anyhow!("初始化 SMTP STARTTLS 失败（{host}）：{e}。请确认端口与加密方式是否匹配")
        })?,
        SmtpTlsMode::None => SmtpTransport::builder_dangerous(host),
    };
    builder = builder
        .port(cfg.port)
        .timeout(Some(Duration::from_secs(clamp_timeout(cfg.timeout_secs))))
        .hello_name(lettre::transport::smtp::extension::ClientId::Domain(
            "NeoSonia".to_string(),
        ));

    let user = smtp_login_user(cfg, oauth);
    if oauth.enabled {
        let token = resolve_oauth_token(oauth, force_refresh)?;
        let Some(token) = token else {
            bail!("已启用 OAuth2 但没有可用的 access_token：请在设置页重新完成邮箱授权");
        };
        builder = builder
            .credentials(Credentials::new(user, token))
            .authentication(vec![Mechanism::Xoauth2]);
    } else if !user.is_empty() {
        builder = builder.credentials(Credentials::new(user, password.to_string()));
    }
    Ok(builder.build())
}

/// 只做一次 SMTP 连接 + NOOP（带 lettre 的 AUTH）。
fn smtp_probe_once(
    cfg: &SmtpSettings,
    password: &str,
    oauth: &MailOAuthSettings,
    force_refresh: bool,
) -> anyhow::Result<()> {
    let transport = build_transport(cfg, password, oauth, force_refresh)?;
    match transport.test_connection() {
        Ok(true) => Ok(()),
        Ok(false) => bail!(
            "SMTP {}:{} 连接成功但服务器未正常响应 NOOP",
            cfg.host.trim(),
            cfg.port
        ),
        Err(e) => Err(smtp_error(cfg, &e)),
    }
}

/// SMTP 连接测试；OAuth 模式下如果认证失败，强制刷新 token 重试一次。
fn smtp_probe(cfg: &SmtpSettings, password: &str, oauth: &MailOAuthSettings) -> anyhow::Result<()> {
    match smtp_probe_once(cfg, password, oauth, false) {
        Ok(()) => Ok(()),
        Err(first) => {
            if oauth.enabled && is_smtp_auth_error(&first.to_string()) {
                tracing::warn!(
                    host = cfg.host.trim(),
                    "SMTP 使用缓存 token 认证失败，强制刷新 access_token 后重试一次"
                );
                return match smtp_probe_once(cfg, password, oauth, true) {
                    Ok(()) => Ok(()),
                    Err(second) => Err(anyhow!(
                        "{first}\n强制刷新 refresh_token 后重试仍失败：{second}\n\
                         请在设置页重新点「用 Microsoft 登录 / 用 Google 登录」完成授权"
                    )),
                };
            }
            Err(first)
        }
    }
}

/// 发信；OAuth 模式下认证失败会强制刷新 token 重试一次。
fn smtp_send_with_retry(
    cfg: &SmtpSettings,
    password: &str,
    oauth: &MailOAuthSettings,
    raw: &Message,
) -> anyhow::Result<()> {
    let send_once = |force_refresh: bool| -> anyhow::Result<()> {
        let transport = build_transport(cfg, password, oauth, force_refresh)?;
        transport
            .send(raw)
            .map(|_| ())
            .map_err(|e| smtp_error(cfg, &e))
    };
    match send_once(false) {
        Ok(()) => Ok(()),
        Err(first) => {
            if oauth.enabled && is_smtp_auth_error(&first.to_string()) {
                tracing::warn!(
                    host = cfg.host.trim(),
                    "SMTP 发信认证失败，强制刷新 access_token 后重试一次"
                );
                return match send_once(true) {
                    Ok(()) => Ok(()),
                    Err(second) => Err(anyhow!(
                        "{first}\n强制刷新 refresh_token 后重试仍失败：{second}"
                    )),
                };
            }
            Err(first)
        }
    }
}

fn is_smtp_auth_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("authentication")
        || lower.contains("invalid credentials")
        || lower.contains("xoauth2")
        || lower.contains("535")
        || lower.contains("5.7.8")
        || lower.contains("5.7.139")
}

fn smtp_security_label(cfg: &SmtpSettings) -> &'static str {
    smtp_tls_mode(cfg).describe()
}

/// 构造 lettre 的 `Message`（纯内存操作，不发网络请求）。
fn build_message(
    cfg: &SmtpSettings,
    oauth: &MailOAuthSettings,
    mail: &OutgoingMail,
) -> anyhow::Result<Message> {
    let from_addr = smtp_from(cfg, oauth);
    if from_addr.trim().is_empty() {
        bail!("发件人为空：请在设置页填写 SMTP 发件人（from）或 SMTP 账号");
    }
    let from = make_from_mailbox(&cfg.from_name, &from_addr)?;
    let mut builder = Message::builder().from(from).subject(mail.subject.clone());
    if mail.to.is_empty() {
        bail!("收件人为空：请在 to 字段填写至少一个邮箱地址");
    }
    for addr in &mail.to {
        builder = builder.to(parse_mailbox(addr)?);
    }
    for addr in &mail.cc {
        builder = builder.cc(parse_mailbox(addr)?);
    }
    if let Some(irt) = mail.in_reply_to.as_deref() {
        let irt = irt.trim();
        if !irt.is_empty() {
            builder = builder.in_reply_to(irt.to_string());
        }
    }
    let references: Vec<&str> = mail
        .references
        .iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .collect();
    if !references.is_empty() {
        builder = builder.references(references.join(" "));
    }
    let loaded_attachments = attachments::load_attachments(&mail.attachments)?;
    if loaded_attachments.is_empty() {
        // 无附件：保持原来的单段纯文本行为（不额外包 multipart）。
        return builder
            .header(ContentType::TEXT_PLAIN)
            .body(mail.body.clone())
            .map_err(|e| anyhow!("构造邮件失败：{e}（常见原因是收件人/发件人地址格式不合法）"));
    }
    let mut mixed = MultiPart::mixed().singlepart(SinglePart::plain(mail.body.clone()));
    for attachment in loaded_attachments {
        let content_type = ContentType::parse(&attachment.mime).map_err(|e| {
            anyhow!(
                "附件「{}」的 MIME 类型不合法（{}）：{e}",
                attachment.filename,
                attachment.mime
            )
        })?;
        mixed = mixed.singlepart(
            Attachment::new(attachment.filename).body(attachment.bytes, content_type),
        );
    }
    builder
        .multipart(mixed)
        .map_err(|e| anyhow!("构造带附件的邮件失败：{e}（常见原因是非 ASCII 文件名/内容编码问题）"))
}

fn make_from_mailbox(name: &str, addr: &str) -> anyhow::Result<Mailbox> {
    let email: Address = addr
        .trim()
        .parse()
        .map_err(|e| anyhow!("发件人地址不合法：{addr}（{e}）"))?;
    let name = name.trim();
    Ok(Mailbox::new(
        if name.is_empty() { None } else { Some(name.to_string()) },
        email,
    ))
}

fn parse_mailbox(raw: &str) -> anyhow::Result<Mailbox> {
    let s = raw.trim();
    if s.is_empty() {
        bail!("收件人地址为空");
    }
    s.parse::<Mailbox>()
        .map_err(|e| anyhow!("收件人地址不合法：{s}（{e}）。支持 \"a@b.com\" 或 \"名字 <a@b.com>\" 两种写法"))
}

fn smtp_error_message(cfg: &SmtpSettings, e: &lettre::transport::smtp::Error) -> String {
    let text = e.to_string();
    let lower = text.to_ascii_lowercase();
    let hint = if lower.contains("authentication")
        || lower.contains("invalid credentials")
        || lower.contains("xoauth2")
        || lower.contains("535")
        || lower.contains("5.7.8")
    {
        "。请检查 SMTP 账号和密码/授权码（QQ/163 用授权码，Gmail 用应用专用密码，Outlook/Gmail 建议用 OAuth2 登录）"
    } else if lower.contains("certificate") || lower.contains("tls") {
        "。TLS/证书校验失败，可能是服务端证书不受信任或端口与加密方式不匹配，也可能是系统缺少根证书"
    } else if lower.contains("timed out") || lower.contains("timeout") || lower.contains("refused") {
        "。网络连接失败，请检查主机/端口/防火墙，或是否需要代理"
    } else {
        ""
    };
    let mut message = format!("SMTP 操作失败（{}:{}）：{}{}", cfg.host.trim(), cfg.port, text, hint);
    if let Some(extra) = presets::diagnose_hint("", &cfg.host, true, &text) {
        message.push_str("\n提示：");
        message.push_str(&extra);
    }
    message
}

fn smtp_error(cfg: &SmtpSettings, e: &lettre::transport::smtp::Error) -> anyhow::Error {
    anyhow!("{}", smtp_error_message(cfg, e))
}

fn imap_error(action: &str, host: &str, e: imap::Error) -> anyhow::Error {
    let base = match &e {
        imap::Error::Tls(_) | imap::Error::TlsHandshake(_) => format!(
            "{action}失败（{host}）：TLS 握手失败。可能是服务端证书不受信任、系统缺少根证书/OpenSSL，\
             或端口与加密方式不匹配（993 用直连 TLS，143 用 STARTTLS 或明文）。底层信息：{e}"
        ),
        imap::Error::No(msg) => format!(
            "{action}失败（{host}）：服务器拒绝：{msg}。请检查账号密码或授权码是否有效、IMAP 是否已在邮箱网页端开启"
        ),
        imap::Error::Bad(msg) => format!("{action}失败（{host}）：服务器返回 BAD：{msg}（可能是服务器不支持该命令）"),
        imap::Error::Io(err) => format!(
            "{action}失败（{host}）：网络错误：{err}。请检查网络、端口、防火墙，或是否需要用代理"
        ),
        _ => format!("{action}失败（{host}）：{e}"),
    };
    apply_imap_reauth_hint(base, &e)
}

fn imap_login_error(host: &str, port: u16, e: imap::Error) -> anyhow::Error {
    let base = match &e {
        imap::Error::No(msg) if msg.to_ascii_lowercase().contains("unsafe login") => format!(
            "IMAP 登录失败（{host}:{port}）：服务器返回“Unsafe Login”。\
             网易 163/126 要求客户端在登录后发送 IMAP ID（本程序会在 993 直连 TLS 下自动发送）；\
             请确认端口是 993、tls=true（不要用 143/STARTTLS），并在网页邮箱开启 IMAP。原始信息：{msg}"
        ),
        imap::Error::No(msg) => format!(
            "IMAP 登录失败（{host}:{port}）：服务器拒绝：{msg}。\
             常见原因：密码错误、需要“授权码”而不是登录密码（QQ/163/126 等）、该邮箱未开启 IMAP"
        ),
        imap::Error::Tls(_) | imap::Error::TlsHandshake(_) => format!(
            "IMAP 登录失败（{host}:{port}）：TLS 握手失败，可能是端口与加密方式不匹配或证书不受信任。底层信息：{e}"
        ),
        imap::Error::Io(err) => format!("IMAP 登录失败（{host}:{port}）：网络错误：{err}"),
        _ => format!("IMAP 登录失败（{host}:{port}）：{e}"),
    };
    let base = match presets::diagnose_hint("", host, false, &e.to_string()) {
        Some(hint) => format!("{base}\n提示：{hint}"),
        None => base,
    };
    apply_imap_reauth_hint(base, &e)
}

fn imap_raw_error(action: &str, e: imap::Error) -> anyhow::Error {
    let base = match &e {
        imap::Error::No(msg) => format!("{action}失败：服务器拒绝：{msg}"),
        imap::Error::Bad(msg) => format!("{action}失败：服务器返回 BAD：{msg}"),
        imap::Error::Io(err) => format!("{action}失败：网络错误：{err}（连接可能已断开，请重试）"),
        _ => format!("{action}失败：{e}"),
    };
    apply_imap_reauth_hint(base, &e)
}

fn tls_error(
    host: &str,
    port: u16,
    e: native_tls::HandshakeError<TcpStream>,
) -> anyhow::Error {
    anyhow!(
        "TLS 握手失败（{host}:{port}）：{e}。可能是服务端证书不受信任、系统根证书缺失，\
         或该端口并不提供直连 TLS（993 才是直连 TLS，143 请用 STARTTLS）；\
         如果装了 HTTPS 拦截/代理软件（如企业代理、杀毒软件），请把它的根证书装进系统证书库"
    )
}

// ============================================================
// 回复内容
// ============================================================

/// 生成回复主题：已有 `Re:` 前缀就保留，否则加 `Re: `。
pub fn reply_subject(subject: &str) -> String {
    let s = subject.trim();
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("re:") || lower.starts_with("re：") {
        s.to_string()
    } else if s.is_empty() {
        "Re: (无主题)".to_string()
    } else {
        format!("Re: {s}")
    }
}

/// 组装回复正文：截断到 `max_chars`，并追加签名。
pub fn compose_reply_body(body: &str, signature: &str, max_chars: usize) -> String {
    let mut text = body.trim().to_string();
    if text.chars().count() > max_chars {
        text = crate::truncate_chars(&text, max_chars);
    }
    let sig = signature.trim();
    if sig.is_empty() {
        text
    } else {
        // 签名里如果已经有了 "-- " 分隔行就不再重复加
        if text.ends_with(sig) {
            text
        } else {
            format!("{text}\n\n-- \n{sig}")
        }
    }
}

// ============================================================
// 异步桥接
// ============================================================

fn clamp_timeout(secs: u64) -> u64 {
    if secs == 0 { DEFAULT_TIMEOUT_SECS } else { secs.clamp(5, 600) }
}

async fn run_blocking<R, F>(timeout_secs: u64, what: &'static str, f: F) -> anyhow::Result<R>
where
    R: Send + 'static,
    F: FnOnce() -> anyhow::Result<R> + Send + 'static,
{
    let limit = Duration::from_secs(timeout_secs.saturating_add(TIMEOUT_GRACE_SECS));
    match tokio::time::timeout(limit, tokio::task::spawn_blocking(f)).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_err)) => Err(anyhow!("{what}的后台线程异常结束：{join_err}")),
        Err(_) => Err(anyhow!(
            "{what}超时（{} 秒）：请检查网络、服务器地址/端口，或在设置页把邮件超时时间调大",
            timeout_secs
        )),
    }
}

// ============================================================
// 连接诊断的公共类型
// ============================================================

/// OAuth 运行状态（给 UI 展示；不含 token 明文）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OAuthStatus {
    pub enabled: bool,
    pub provider: String,
    pub email: String,
    pub has_refresh_token: bool,
    pub access_token_valid: bool,
    pub expires_at_ms: i64,
    /// 客户端 ID 来源说明（只显示掩码，不回显完整 client_id）。
    pub client_id_hint: String,
    /// access_token JWT 的 aud（Microsoft 一般是 https://outlook.office.com）。
    pub audience: Vec<String>,
    /// access_token JWT 的 scp（空格分隔的权限）。
    pub scopes: Vec<String>,
    /// 是否带 IMAP.AccessAsUser.All（缺它收信会报 "User is authenticated but not connected"）。
    pub has_imap_scope: bool,
    /// 是否带 SMTP.Send。
    pub has_smtp_scope: bool,
    /// 是否需要用户重新授权（缺 IMAP 权限/受众不对/refresh 失效等）。
    pub needs_reauth: bool,
    /// 给 UI 的一句话状态建议（不含敏感信息）。
    pub status_message: String,
}

/// 单个协议（IMAP / SMTP）的连接诊断结果。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConnectionResult {
    pub target: String,
    pub ok: bool,
    pub message: String,
    pub detail: Option<String>,
}

impl ConnectionResult {
    pub fn ok(target: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            ok: true,
            message: message.into(),
            detail: None,
        }
    }

    pub fn fail(
        target: impl Into<String>,
        message: impl Into<String>,
        detail: Option<String>,
    ) -> Self {
        Self {
            target: target.into(),
            ok: false,
            message: message.into(),
            detail,
        }
    }
}

/// 一次连接诊断的完整结果（app/UI 直接展示 `summary`，失败时再看 imap/smtp）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConnectionReport {
    pub imap: ConnectionResult,
    pub smtp: ConnectionResult,
    pub oauth: OAuthStatus,
    /// 一句话总结，例如“连接正常：…”。
    pub summary: String,
}

impl ConnectionReport {
    pub fn all_ok(&self) -> bool {
        self.imap.ok && self.smtp.ok
    }
}

/// 便捷函数：用一份 Settings 直接做连接诊断。
pub async fn test_connection(settings: &Settings) -> ConnectionReport {
    MailService::from_settings(settings).test_connection().await
}

/// 服务商显示名（用于诊断）。
fn provider_label(provider: OAuthProvider) -> &'static str {
    match provider {
        OAuthProvider::Microsoft => "Microsoft（Outlook / Microsoft 365）",
        OAuthProvider::Google => "Google（Gmail）",
        OAuthProvider::Custom => "自定义 OAuth2",
    }
}

/// client_id 来源提示（不泄露完整值）。
fn client_id_hint(oauth: &MailOAuthSettings) -> String {
    let configured = oauth.client_id.trim();
    if !configured.is_empty() {
        let visible: String = configured.chars().take(8).collect();
        return format!("已自定义 client_id（{visible}…），授权页显示你自己的应用名");
    }
    match oauth.provider {
        OAuthProvider::Microsoft => "默认使用 Microsoft Office 公共客户端 ID（授权页显示 Microsoft Office，不是 NeoSonia）；可在设置页填自己的 Azure client_id 显示自己的应用名".to_string(),
        OAuthProvider::Google => "未配置 Google client_id：请填写自建「桌面应用」OAuth 客户端的 client_id 和 client_secret".to_string(),
        OAuthProvider::Custom => "未配置 OAuth client_id：请填写自己的 OAuth 客户端 ID".to_string(),
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use neosonia_core::config::Settings;
    use std::io::Write;

    fn service_with_defaults() -> MailService {
        MailService::from_settings(&Settings::default())
    }

    #[test]
    fn oauth_enabled_without_refresh_token_gives_hint() {
        let mut settings = Settings::default();
        settings.mail.oauth.enabled = true;
        settings.mail.imap.host = "outlook.office365.com".into();
        settings.mail.imap.port = 993;
        settings.mail.oauth.email = "me@outlook.com".into();
        let svc = MailService::from_settings(&settings);
        assert!(svc.oauth_enabled());
        let err = svc.check_imap().unwrap_err().to_string();
        assert!(err.contains("refresh_token"), "got: {err}");
        assert!(err.contains("流程未完成"), "got: {err}");

        let mut settings2 = settings.clone();
        settings2.mail.oauth.refresh_token = "rt".into();
        settings2.mail.smtp.host = "smtp.office365.com".into();
        let svc2 = MailService::from_settings(&settings2);
        assert!(svc2.check_imap().is_ok());
        assert!(svc2.check_smtp().is_ok());
        // OAuth 模式用户名回退到授权邮箱
        assert_eq!(svc2.effective_user(), "me@outlook.com");
        assert_eq!(smtp_from(&settings2.mail.smtp, &settings2.mail.oauth), "me@outlook.com");
    }

    #[test]
    fn oauth_full_login_pair_cache_works() {
        // 已缓存 access_token 且未过期时，OAuth 模式不应该发网络请求。
        let mut settings = Settings::default();
        settings.mail.oauth.enabled = true;
        settings.mail.oauth.provider = neosonia_core::OAuthProvider::Microsoft;
        settings.mail.oauth.client_id = "cid".into();
        settings.mail.oauth.access_token = "cached".into();
        settings.mail.oauth.refresh_token = "rt".into();
        settings.mail.oauth.expires_at_ms = neosonia_core::util::now_ms() + 3_600_000;
        let _svc = MailService::from_settings(&settings);
        let mut oauth = OAuth::from_settings(&settings.mail.oauth);
        assert!(oauth.access_token_valid());
        assert_eq!(oauth.access_token().unwrap(), "cached");
    }

    #[test]
    fn unconfigured_service_gives_chinese_hint() {
        let svc = service_with_defaults();
        assert!(!svc.imap_configured());
        assert!(!svc.smtp_configured());
        let err = svc.check_imap().unwrap_err().to_string();
        assert!(err.contains("未配置邮件服务器") || err.contains("未配置"), "got: {err}");
        let err = svc.check_smtp().unwrap_err().to_string();
        assert!(err.contains("未配置邮件服务器") || err.contains("未配置"), "got: {err}");
        assert_eq!(svc.default_folder(), "INBOX");
        assert_eq!(svc.provider(), "custom");
    }

    #[test]
    fn custom_password_config_is_accepted() {
        let mut settings = Settings::default();
        settings.mail.provider = "163".into();
        settings.mail.imap.host = "imap.163.com".into();
        settings.mail.imap.port = 993;
        settings.mail.imap.tls = true;
        settings.mail.imap.user = "user@163.com".into();
        settings.mail.imap.password = "authcode".into();
        settings.mail.smtp.host = "smtp.163.com".into();
        settings.mail.smtp.port = 465;
        settings.mail.smtp.implicit_tls = true;
        settings.mail.smtp.user = "user@163.com".into();
        settings.mail.smtp.password = "authcode".into();
        settings.mail.smtp.from = "user@163.com".into();
        let svc = MailService::from_settings(&settings);
        assert_eq!(svc.provider(), "163");
        assert!(svc.imap_configured());
        assert!(svc.smtp_configured());
        assert!(svc.check_imap().is_ok());
        assert!(svc.check_smtp().is_ok());
        assert_eq!(svc.effective_user(), "user@163.com");
        let help = svc.configuration_help();
        assert!(help.contains("163"));
        assert!(help.contains("imap.163.com"));
        assert!(!help.contains("authcode"), "诊断信息不能包含密码");
    }

    #[test]
    fn missing_password_hint_mentions_authorization_code() {
        let mut settings = Settings::default();
        settings.mail.provider = "qq".into();
        settings.mail.imap.host = "imap.qq.com".into();
        settings.mail.imap.user = "12345@qq.com".into();
        let svc = MailService::from_settings(&settings);
        let err = svc.check_imap().unwrap_err().to_string();
        assert!(err.contains("授权码"), "got: {err}");
    }

    #[test]
    fn oauth_status_does_not_leak_tokens() {
        let mut settings = Settings::default();
        settings.mail.oauth.enabled = true;
        settings.mail.oauth.provider = neosonia_core::OAuthProvider::Microsoft;
        settings.mail.oauth.email = "a@outlook.com".into();
        settings.mail.oauth.refresh_token = "refresh-secret-value".into();
        settings.mail.oauth.access_token = "access-secret-value".into();
        settings.mail.oauth.expires_at_ms = neosonia_core::util::now_ms() + 3_600_000;
        let svc = MailService::from_settings(&settings);
        let status = svc.oauth_status();
        assert!(status.enabled);
        assert!(status.has_refresh_token);
        assert!(status.access_token_valid);
        assert!(!status.client_id_hint.contains("refresh-secret"));
        let text = format!("{status:?}");
        assert!(!text.contains("refresh-secret-value"));
        assert!(!text.contains("access-secret-value"));
    }

    fn fake_access_token_jwt(claims: serde_json::Value) -> String {
        use base64::Engine as _;
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(claims.to_string().as_bytes());
        format!("{header}.{payload}.signature")
    }

    #[test]
    fn oauth_status_reports_missing_imap_scope() {
        crate::oauth::clear_needs_reauth();
        let mut settings = Settings::default();
        settings.mail.oauth.enabled = true;
        settings.mail.oauth.provider = neosonia_core::OAuthProvider::Microsoft;
        settings.mail.oauth.email = "a@outlook.com".into();
        settings.mail.oauth.refresh_token = "rt".into();
        settings.mail.oauth.access_token = fake_access_token_jwt(serde_json::json!({
            "aud": "https://outlook.office.com",
            "scp": "SMTP.Send offline_access"
        }));
        settings.mail.oauth.expires_at_ms = neosonia_core::util::now_ms() + 3_600_000;
        let status = MailService::from_settings(&settings).oauth_status();
        assert!(status.needs_reauth);
        assert!(!status.has_imap_scope);
        assert!(status.has_smtp_scope);
        assert_eq!(status.audience, vec!["https://outlook.office.com"]);
        assert!(status.status_message.contains("IMAP"), "got: {}", status.status_message);
        let text = format!("{status:?}");
        assert!(!text.contains(&settings.mail.oauth.access_token), "不能打印 token 明文");
    }

    #[test]
    fn oauth_status_ok_with_full_scopes() {
        crate::oauth::clear_needs_reauth();
        let mut settings = Settings::default();
        settings.mail.oauth.enabled = true;
        settings.mail.oauth.provider = neosonia_core::OAuthProvider::Microsoft;
        settings.mail.oauth.email = "a@outlook.com".into();
        settings.mail.oauth.refresh_token = "rt".into();
        settings.mail.oauth.access_token = fake_access_token_jwt(serde_json::json!({
            "aud": "https://outlook.office.com",
            "scp": "IMAP.AccessAsUser.All SMTP.Send offline_access"
        }));
        settings.mail.oauth.expires_at_ms = neosonia_core::util::now_ms() + 3_600_000;
        let status = MailService::from_settings(&settings).oauth_status();
        assert!(!status.needs_reauth, "status_message={}", status.status_message);
        assert!(status.has_imap_scope);
        assert!(status.has_smtp_scope);
        assert!(status.scopes.iter().any(|s| s == "SMTP.Send"));
    }

    #[test]
    fn oauth_status_flags_wrong_audience() {
        crate::oauth::clear_needs_reauth();
        let mut settings = Settings::default();
        settings.mail.oauth.enabled = true;
        settings.mail.oauth.provider = neosonia_core::OAuthProvider::Microsoft;
        settings.mail.oauth.email = "a@outlook.com".into();
        settings.mail.oauth.refresh_token = "rt".into();
        settings.mail.oauth.access_token = fake_access_token_jwt(serde_json::json!({
            "aud": "https://graph.microsoft.com",
            "scp": "IMAP.AccessAsUser.All SMTP.Send"
        }));
        settings.mail.oauth.expires_at_ms = neosonia_core::util::now_ms() + 3_600_000;
        let status = MailService::from_settings(&settings).oauth_status();
        assert!(status.needs_reauth);
        assert!(status.status_message.contains("audience") || status.status_message.contains("Outlook"));
    }

    #[test]
    fn imap_mode_never_falls_back_from_993() {
        assert_eq!(imap_mode(true, 993), ImapMode::ImplicitTls);
        assert_eq!(imap_mode(true, 143), ImapMode::StartTls);
        assert_eq!(imap_mode(true, 1993), ImapMode::ImplicitTls); // 非 143 一律直连 TLS，不回退
        assert_eq!(imap_mode(false, 143), ImapMode::Plain);
        assert_eq!(imap_mode(false, 993), ImapMode::Plain);
    }

    #[test]
    fn smtp_xoauth2_mechanism_matches_imap_string() {
        // SMTP 层的 XOAUTH2 报文应与 IMAP 完全一致（lettre Mechanism::Xoauth2）。
        let creds = Credentials::new("u@outlook.com".to_string(), "token-1".to_string());
        let response = Mechanism::Xoauth2
            .response(&creds, None)
            .expect("xoauth2 response");
        assert_eq!(response, crate::oauth::build_xoauth2("u@outlook.com", "token-1"));
        let encoded = base64::engine::general_purpose::STANDARD.encode(response);
        assert!(!encoded.is_empty());
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("decode");
        assert_eq!(String::from_utf8(decoded).unwrap(), crate::oauth::build_xoauth2("u@outlook.com", "token-1"));
    }

    #[test]
    fn id_filter_swallows_untagged_id_response() {
        use std::io::Cursor;
        // 读端：服务器对 ID 命令的响应（未标记行 + 标记行）
        let server_response = b"* ID (\"name\" \"NeoSonia\" \"version\" \"0.6\")\r\nA3 OK ID completed\r\n".to_vec();
        let mut filter = IdFilter::new(Cursor::new(server_response));
        // 模拟 imap 发送 ID 命令
        filter.write_all(b"A3 ID (\"name\" \"NeoSonia\")\r\n").unwrap();
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut filter, &mut out).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("* ID"), "未标记 ID 响应应被吞掉: {text:?}");
        assert!(text.contains("A3 OK ID completed"), "带标记响应应保留: {text:?}");
    }

    #[test]
    fn id_filter_passes_normal_responses_through() {
        use std::io::Cursor;
        let mut filter = IdFilter::new(Cursor::new(b"* OK hello\r\nA1 OK LOGIN completed\r\n".to_vec()));
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut filter, &mut out).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "* OK hello\r\nA1 OK LOGIN completed\r\n"
        );
    }

    #[test]
    fn connection_report_summary_logic() {
        let report = ConnectionReport {
            imap: ConnectionResult::ok("IMAP", "已连接"),
            smtp: ConnectionResult::ok("SMTP", "已连接"),
            oauth: OAuthStatus::default(),
            summary: "连接正常".into(),
        };
        assert!(report.all_ok());
        let fail = ConnectionReport {
            imap: ConnectionResult::fail("IMAP", "错误", None),
            smtp: ConnectionResult::ok("SMTP", "已连接"),
            oauth: OAuthStatus::default(),
            summary: String::new(),
        };
        assert!(!fail.all_ok());
    }

    #[test]
    fn smtp_tls_mode_detection() {
        let mut cfg = SmtpSettings::default();
        cfg.port = 465;
        cfg.implicit_tls = false;
        cfg.starttls = false;
        assert_eq!(smtp_tls_mode(&cfg), SmtpTlsMode::Implicit);

        cfg.port = 587;
        assert_eq!(smtp_tls_mode(&cfg), SmtpTlsMode::StartTls);

        cfg.port = 25;
        assert_eq!(smtp_tls_mode(&cfg), SmtpTlsMode::None);

        cfg.port = 25;
        cfg.implicit_tls = true;
        assert_eq!(smtp_tls_mode(&cfg), SmtpTlsMode::Implicit);

        cfg.implicit_tls = false;
        cfg.starttls = true;
        assert_eq!(smtp_tls_mode(&cfg), SmtpTlsMode::StartTls);
    }

    #[test]
    fn builds_message_with_reply_headers() {
        let cfg = SmtpSettings {
            host: "smtp.example.com".into(),
            port: 465,
            from: "me@example.com".into(),
            from_name: "NeoSonia".into(),
            ..Default::default()
        };
        let mail = OutgoingMail {
            to: vec!["a@b.com".into(), "名字 <c@d.com>".into()],
            cc: vec!["cc@e.com".into()],
            subject: "Re: 测试".into(),
            body: "正文内容".into(),
            in_reply_to: Some("<abc@example.com>".into()),
            references: vec!["<prev@example.com>".into(), "<abc@example.com>".into()],
            ..Default::default()
        };
        let msg = build_message(&cfg, &MailOAuthSettings::default(), &mail).expect("build message");
        let raw = String::from_utf8_lossy(&msg.formatted()).to_string();
        assert!(raw.contains("In-Reply-To: <abc@example.com>"));
        assert!(raw.contains("References: <prev@example.com> <abc@example.com>"));
        assert!(raw.contains("From:"));
        assert!(raw.contains("me@example.com"));
        assert!(raw.contains("To:"));
        assert!(raw.contains("a@b.com"));
        assert!(raw.contains("c@d.com"));
        assert!(raw.contains("Cc:"));
        assert!(raw.contains("cc@e.com"));
        // 主题会被 RFC2047 编码，检查编码后的形态不重要，只确认有 Subject
        assert!(raw.contains("Subject:"));
    }

    #[test]
    fn build_message_rejects_bad_address() {
        let cfg = SmtpSettings {
            host: "smtp.example.com".into(),
            from: "me@example.com".into(),
            ..Default::default()
        };
        let mail = OutgoingMail {
            to: vec!["not-an-address".into()],
            subject: "x".into(),
            body: "y".into(),
            ..Default::default()
        };
        let err = build_message(&cfg, &MailOAuthSettings::default(), &mail)
            .unwrap_err()
            .to_string();
        assert!(err.contains("收件人地址不合法"), "got: {err}");
    }

    // ---------- 附件 / multipart ----------

    fn temp_attach_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "neosonia-mail-msg-{tag}-{}-{}",
            std::process::id(),
            neosonia_core::util::short_id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn plain_cfg() -> SmtpSettings {
        SmtpSettings {
            host: "smtp.example.com".into(),
            from: "me@example.com".into(),
            from_name: "NeoSonia".into(),
            ..Default::default()
        }
    }

    fn base_mail(cfg: &SmtpSettings) -> OutgoingMail {
        let _ = cfg;
        OutgoingMail {
            to: vec!["a@b.com".into()],
            subject: "带附件的邮件".into(),
            body: "正文内容".into(),
            ..Default::default()
        }
    }

    #[test]
    fn build_message_without_attachments_is_single_part() {
        let cfg = plain_cfg();
        let mail = base_mail(&cfg);
        let msg = build_message(&cfg, &MailOAuthSettings::default(), &mail).expect("build");
        let raw = String::from_utf8_lossy(&msg.formatted()).to_string();
        assert!(!raw.contains("multipart/mixed"), "无附件不应包 multipart: {raw}");
        assert!(raw.contains("Content-Type: text/plain"), "应为纯文本单段: {raw}");
        assert!(
            raw.contains("Content-Transfer-Encoding"),
            "应有正文编码头: {raw}"
        );
    }

    #[test]
    fn build_message_with_attachments_uses_multipart_mixed() {
        let cfg = plain_cfg();
        let dir = temp_attach_dir("multipart");
        let pdf = dir.join("invoice.pdf");
        std::fs::write(&pdf, b"%PDF-1.4 fake pdf bytes").unwrap();
        let png = dir.join("photo.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nfake").unwrap();
        let mut mail = base_mail(&cfg);
        mail.attachments = vec![
            pdf.to_string_lossy().to_string(),
            png.to_string_lossy().to_string(),
        ];
        let msg = build_message(&cfg, &MailOAuthSettings::default(), &mail).expect("build");
        let raw = String::from_utf8_lossy(&msg.formatted()).to_string();
        assert!(raw.contains("Content-Type: multipart/mixed;"), "缺 multipart/mixed: {raw}");
        assert!(raw.contains("boundary="), "缺 boundary: {raw}");
        assert!(raw.contains("Content-Disposition: attachment; filename=\"invoice.pdf\""), "缺附件头: {raw}");
        assert!(raw.contains("Content-Disposition: attachment; filename=\"photo.png\""), "缺附件头: {raw}");
        assert!(raw.contains("Content-Type: application/pdf"), "PDF MIME 不对: {raw}");
        assert!(raw.contains("Content-Type: image/png"), "PNG MIME 不对: {raw}");
        // 附件内容编码（lettre 对二进制用 base64）
        assert!(
            raw.to_ascii_lowercase().contains("content-transfer-encoding: base64"),
            "附件应 base64 编码: {raw}"
        );
        // 正文仍在（multipart 第一个 part 是 text/plain）
        assert!(raw.contains("Content-Type: text/plain"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_message_with_chinese_attachment_filename_uses_rfc2231() {
        let cfg = plain_cfg();
        let dir = temp_attach_dir("chinese-name");
        let file = dir.join("中文 报告.txt");
        std::fs::write(&file, "hello 中文内容".as_bytes()).unwrap();
        let mut mail = base_mail(&cfg);
        mail.attachments = vec![file.to_string_lossy().to_string()];
        let msg = build_message(&cfg, &MailOAuthSettings::default(), &mail).expect("build");
        let raw = String::from_utf8_lossy(&msg.formatted()).to_string();
        // lettre 用 RFC2231 编码非 ASCII 文件名（filename*0*=utf-8''...）
        assert!(
            raw.contains("filename*0*=utf-8''") || raw.contains("filename*0*=UTF-8''"),
            "中文文件名应 RFC2231 编码: {raw}"
        );
        assert!(raw.contains("Content-Type: text/plain"), "txt MIME 不对: {raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_message_rejects_missing_attachment() {
        let cfg = plain_cfg();
        let mut mail = base_mail(&cfg);
        let missing = std::env::temp_dir().join("neosonia-mail-no-such-attachment.pdf");
        mail.attachments = vec![missing.to_string_lossy().to_string()];
        let err = build_message(&cfg, &MailOAuthSettings::default(), &mail)
            .unwrap_err()
            .to_string();
        assert!(err.contains("附件不存在"), "got: {err}");
    }

    #[test]
    fn build_message_rejects_too_many_attachments() {
        let cfg = plain_cfg();
        let mut mail = base_mail(&cfg);
        mail.attachments = (0..=crate::attachments::MAX_ATTACHMENTS)
            .map(|i| format!("/tmp/a{i}.txt"))
            .collect();
        let err = build_message(&cfg, &MailOAuthSettings::default(), &mail)
            .unwrap_err()
            .to_string();
        assert!(err.contains("最多"), "got: {err}");
    }

    #[test]
    fn reply_subject_adds_prefix_once() {
        assert_eq!(reply_subject("你好"), "Re: 你好");
        assert_eq!(reply_subject("Re: 你好"), "Re: 你好");
        assert_eq!(reply_subject("RE: 你好"), "RE: 你好");
        assert_eq!(reply_subject("  "), "Re: (无主题)");
    }

    #[test]
    fn reply_body_respects_limit_and_signature() {
        let body = "字".repeat(50);
        let out = compose_reply_body(&body, " Sonia ", 10);
        assert!(out.contains("Sonia"));
        assert!(out.contains("-- "));
        assert!(out.chars().count() < 100);

        let out2 = compose_reply_body("hi", "", 100);
        assert_eq!(out2, "hi");
    }

    #[tokio::test]
    async fn test_imap_without_config_returns_error_not_panic() {
        let svc = service_with_defaults();
        let err = svc.test_imap().await.unwrap_err().to_string();
        assert!(err.contains("未配置"), "got: {err}");
        let err = svc.test_smtp().await.unwrap_err().to_string();
        assert!(err.contains("未配置"), "got: {err}");
    }
}

