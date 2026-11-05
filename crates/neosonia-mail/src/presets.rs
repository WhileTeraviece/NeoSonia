//! 邮箱服务商预设（163 / 126 / QQ / Outlook / Gmail / 自定义）。
//!
//! UI 只需要 `presets()` 拿列表（id/中文名/推荐登录方式），用户选完后调用
//! [`apply_preset`] 把 IMAP/SMTP 的 host/port/加密方式自动填好；
//! OAuth 授权成功后调用 [`apply_oauth_result`] 把账号和服务器参数自动回填。
//!
//! 预设数据只在本 crate 维护，`MailSettings.provider` 只存字符串 id
//! （"outlook" | "gmail" | "163" | "126" | "qq" | "custom"），避免 core 与 mail
//! 两份数据源不一致。

use std::sync::OnceLock;

use anyhow::bail;
use neosonia_core::config::{
    MailOAuthSettings, MailSettings, OAuthProvider, Settings,
};
use serde::{Deserialize, Serialize};

use crate::oauth::OAuthResult;

/// 登录方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailAuthKind {
    /// 只能用账号密码 / 授权码。
    Password,
    /// 推荐（或只能）用 OAuth2 浏览器登录。
    OAuth,
    /// 两种都支持，OAuth 更推荐。
    PasswordOrOAuth,
}

impl MailAuthKind {
    pub fn label(self) -> &'static str {
        match self {
            MailAuthKind::Password => "账号密码 / 授权码",
            MailAuthKind::OAuth => "OAuth2 浏览器登录",
            MailAuthKind::PasswordOrOAuth => "OAuth2 或账号密码",
        }
    }
}

/// SMTP 加密方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmtpSecurity {
    /// 465：连接即 TLS。
    ImplicitTls,
    /// 587：明文连接后 STARTTLS。
    StartTls,
    /// 25 等：明文（不推荐）。
    None,
}

impl SmtpSecurity {
    pub fn label(self) -> &'static str {
        match self {
            SmtpSecurity::ImplicitTls => "直连 TLS（SSL）",
            SmtpSecurity::StartTls => "STARTTLS",
            SmtpSecurity::None => "明文（不推荐）",
        }
    }
}

/// 一个邮箱服务商预设。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MailPreset {
    /// 存进 `settings.mail.provider` 的 id。
    pub id: String,
    /// 中文名（UI 下拉显示）。
    pub label: String,
    /// 推荐登录方式。
    pub auth: MailAuthKind,
    /// OAuth 提供方（Password-only 预设为 None）。
    pub oauth_provider: Option<OAuthProvider>,
    pub imap_host: String,
    pub imap_port: u16,
    /// IMAP 是否直连 TLS（993 = true，143 明文 = false）。
    pub imap_tls: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: SmtpSecurity,
    /// IMAP 侧注意事项（中文）。
    pub imap_hint: String,
    /// SMTP 侧注意事项（中文）。
    pub smtp_hint: String,
    /// 一句话说明。
    pub note: String,
}

impl MailPreset {
    /// 方便 UI：这个预设是否用 OAuth。
    pub fn uses_oauth(&self) -> bool {
        matches!(self.auth, MailAuthKind::OAuth | MailAuthKind::PasswordOrOAuth)
    }

    /// 这个预设是否只用账号密码。
    pub fn password_only(&self) -> bool {
        self.auth == MailAuthKind::Password
    }
}

// ============================================================
// 预设数据
// ============================================================

fn make(
    id: &str,
    label: &str,
    auth: MailAuthKind,
    oauth_provider: Option<OAuthProvider>,
    imap_host: &str,
    imap_port: u16,
    imap_tls: bool,
    smtp_host: &str,
    smtp_port: u16,
    smtp_security: SmtpSecurity,
    imap_hint: &str,
    smtp_hint: &str,
    note: &str,
) -> MailPreset {
    MailPreset {
        id: id.to_string(),
        label: label.to_string(),
        auth,
        oauth_provider,
        imap_host: imap_host.to_string(),
        imap_port,
        imap_tls,
        smtp_host: smtp_host.to_string(),
        smtp_port,
        smtp_security,
        imap_hint: imap_hint.to_string(),
        smtp_hint: smtp_hint.to_string(),
        note: note.to_string(),
    }
}

fn build_presets() -> Vec<MailPreset> {
    vec![
        make(
            "outlook",
            "Outlook / Microsoft 365",
            MailAuthKind::PasswordOrOAuth,
            Some(OAuthProvider::Microsoft),
            "outlook.office365.com",
            993,
            true,
            "smtp.office365.com",
            587,
            SmtpSecurity::StartTls,
            "推荐「用 Microsoft 登录」（OAuth2）。若用密码：Microsoft 已基本关闭 IMAP 基本认证，通常只能 OAuth。",
            "SMTP 用 587 + STARTTLS；465 在 Office 365 上通常不可用。",
            "授权页显示的应用名由 client_id 决定；想显示自己的名字请自建 Azure 应用。",
        ),
        make(
            "gmail",
            "Gmail",
            MailAuthKind::PasswordOrOAuth,
            Some(OAuthProvider::Google),
            "imap.gmail.com",
            993,
            true,
            "smtp.gmail.com",
            587,
            SmtpSecurity::StartTls,
            "推荐「用 Google 登录」（OAuth2）。用密码时需要开两步验证并生成「应用专用密码」。",
            "Gmail SMTP 也支持 465 直连 TLS；587 + STARTTLS 通用。",
            "Google 账号可能要求应用专用密码或 Workspace 管理员放行。",
        ),
        make(
            "163",
            "网易 163 邮箱",
            MailAuthKind::Password,
            None,
            "imap.163.com",
            993,
            true,
            "smtp.163.com",
            465,
            SmtpSecurity::ImplicitTls,
            "需要在网页邮箱「设置 → POP3/SMTP/IMAP」开启 IMAP，密码填「客户端授权码」；不接受登录密码。程序会自动发送 IMAP ID（name=NeoSonia）。",
            "SMTP 用 465 直连 TLS；密码同样是客户端授权码。",
            "网易要求客户端发送 IMAP ID，本程序会在登录后自动发送。",
        ),
        make(
            "126",
            "网易 126 邮箱",
            MailAuthKind::Password,
            None,
            "imap.126.com",
            993,
            true,
            "smtp.126.com",
            465,
            SmtpSecurity::ImplicitTls,
            "需要在网页邮箱开启 IMAP，密码填「客户端授权码」；程序会自动发送 IMAP ID（name=NeoSonia）。",
            "SMTP 用 465 直连 TLS；密码同样是客户端授权码。",
            "网易要求客户端发送 IMAP ID，本程序会在登录后自动发送。",
        ),
        make(
            "qq",
            "QQ 邮箱",
            MailAuthKind::Password,
            None,
            "imap.qq.com",
            993,
            true,
            "smtp.qq.com",
            465,
            SmtpSecurity::ImplicitTls,
            "需要在 QQ 邮箱网页端「设置 → 账号」开启 IMAP/SMTP 服务，密码填「授权码」（16 位，不是 QQ 密码）。",
            "SMTP 用 465 直连 TLS；密码同样填授权码。",
            "QQ 邮箱的授权码在网页端生成，可随时重置。",
        ),
        make(
            "custom",
            "自定义 / 企业邮箱 / 自建服务器",
            MailAuthKind::PasswordOrOAuth,
            None,
            "",
            993,
            true,
            "",
            587,
            SmtpSecurity::StartTls,
            "自定义服务器：请填 IMAP 主机、端口、是否 TLS（993 一般为是，143 一般为否）；需要时填授权码。",
            "自定义服务器：465 选直连 TLS，587 选 STARTTLS，25 选明文（不推荐）。",
            "可配合自定义 OAuth2 端点（auth_url / token_url / client_id / scope）。",
        ),
    ]
}

/// 全部预设（顺序即 UI 下拉顺序）。首次调用时构建，之后共享。
pub fn presets() -> &'static [MailPreset] {
    static PRESETS: OnceLock<Vec<MailPreset>> = OnceLock::new();
    PRESETS.get_or_init(build_presets).as_slice()
}

/// 按 id 查预设。
pub fn preset(id: &str) -> Option<&'static MailPreset> {
    let id = id.trim().to_ascii_lowercase();
    presets().iter().find(|p| p.id == id)
}

/// 按 OAuth 提供方查推荐预设。
pub fn oauth_provider_preset(provider: OAuthProvider) -> Option<&'static MailPreset> {
    match provider {
        OAuthProvider::Microsoft => preset("outlook"),
        OAuthProvider::Google => preset("gmail"),
        OAuthProvider::Custom => preset("custom"),
    }
}

/// 按 IMAP 主机名猜预设 id（用户手填 host 时也能自动建议）。
pub fn suggest_provider(imap_host: &str) -> Option<&'static str> {
    let host = imap_host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let candidates: &[(&str, &str)] = &[
        ("outlook.office365.com", "outlook"),
        ("imap-mail.outlook.com", "outlook"),
        ("imap.gmail.com", "gmail"),
        ("imap.googlemail.com", "gmail"),
        ("imap.163.com", "163"),
        ("imap.126.com", "126"),
        ("imap.qq.com", "qq"),
        ("imap.exmail.qq.com", "qq"),
        ("imap.yeah.net", "163"),
    ];
    for (needle, id) in candidates {
        if host == *needle || host.ends_with(&format!(".{needle}")) {
            return Some(id);
        }
    }
    None
}

/// 该连接是否需要发送 IMAP `ID` 命令（网易 163/126 必需；QQ 也发一下更稳）。
pub fn imap_id_required(provider: &str, imap_host: &str) -> bool {
    let provider = provider.trim().to_ascii_lowercase();
    if matches!(provider.as_str(), "163" | "126" | "qq") {
        return true;
    }
    let host = imap_host.trim().to_ascii_lowercase();
    ["163.com", "126.com", "yeah.net", "qq.com"]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

/// 生成要发送的 IMAP ID 指令（自报 NeoSonia，不冒充第三方客户端）。
pub fn imap_id_command() -> String {
    format!(
        "ID (\"name\" \"NeoSonia\" \"version\" \"{}\" \"vendor\" \"NeoSonia\" \"support-email\" \"support@neosonia.app\")",
        env!("CARGO_PKG_VERSION")
    )
}

// ============================================================
// 应用预设 / OAuth 结果
// ============================================================

/// 把预设写进 `MailSettings`（UI 选完邮箱类型后调用）。
///
/// - 会写 `provider`、IMAP/SMTP 的 host/port/加密方式、`oauth.provider/tenant`；
/// - **不会**覆盖用户已填的账号、密码/授权码、发件人显示名；
/// - `custom` 只设置 provider，不动服务器参数。
pub fn apply_preset(mail: &mut MailSettings, id: &str) -> anyhow::Result<String> {
    let Some(preset) = preset(id) else {
        let ids: Vec<&str> = presets().iter().map(|p| p.id.as_str()).collect();
        bail!("未知的邮箱类型 {id:?}：可选 {}", ids.join(" / "));
    };
    mail.provider = preset.id.clone();
    let mut lines = Vec::new();
    if preset.id != "custom" {
        mail.imap.host = preset.imap_host.clone();
        mail.imap.port = preset.imap_port;
        mail.imap.tls = preset.imap_tls;
        mail.smtp.host = preset.smtp_host.clone();
        mail.smtp.port = preset.smtp_port;
        match preset.smtp_security {
            SmtpSecurity::ImplicitTls => {
                mail.smtp.implicit_tls = true;
                mail.smtp.starttls = false;
            }
            SmtpSecurity::StartTls => {
                mail.smtp.implicit_tls = false;
                mail.smtp.starttls = true;
            }
            SmtpSecurity::None => {
                mail.smtp.implicit_tls = false;
                mail.smtp.starttls = false;
            }
        }
        if mail.imap.mailbox.trim().is_empty() {
            mail.imap.mailbox = "INBOX".to_string();
        }
        lines.push(format!(
            "已套用「{}」：IMAP {}:{}（{}），SMTP {}:{}（{}）",
            preset.label,
            preset.imap_host,
            preset.imap_port,
            if preset.imap_tls { "直连 TLS" } else { "明文" },
            preset.smtp_host,
            preset.smtp_port,
            preset.smtp_security.label()
        ));
    } else {
        lines.push("已切换到「自定义」：请填写 IMAP/SMTP 主机、端口和加密方式".to_string());
    }
    if let Some(provider) = preset.oauth_provider {
        mail.oauth.provider = provider;
        if provider == OAuthProvider::Microsoft && mail.oauth.tenant.trim().is_empty() {
            mail.oauth.tenant = "common".to_string();
        }
        lines.push(format!("登录方式：{}。{}", preset.auth.label(), preset.note));
    } else {
        lines.push(format!("登录方式：{}。{}", preset.auth.label(), preset.note));
        lines.push(preset.imap_hint.clone());
        lines.push(preset.smtp_hint.clone());
    }
    Ok(lines.join("\n"))
}

/// OAuth 授权成功后自动回填 settings（app 的 `mail_oauth_start` 完成时调用）。
///
/// 写入：`oauth.enabled/email/refresh_token/access_token/expires_at_ms/scope`，
/// 并按 provider 自动填 IMAP/SMTP 的 host/port/加密方式 + 用户名/发件人（为空时）。
/// 返回中文摘要给 UI。
pub fn apply_oauth_result(settings: &mut Settings, result: &OAuthResult) -> anyhow::Result<String> {
    let email = result.email.trim().to_string();
    let has_refresh_token = !result.refresh_token.trim().is_empty();

    // 流程未完成（没有 refresh_token）：**一个字段都不改**，只置"需要重新授权"，
    // 保证旧账号仍可用（用户报过"换账号失败后老账号凭证也没了"）。
    if !has_refresh_token {
        crate::oauth::mark_needs_reauth("OAuth 授权结果缺少 refresh_token");
        return Err(anyhow::anyhow!(
            "授权返回缺少 refresh_token（流程未完成）：未修改现有邮箱配置，请重新点「用 Microsoft/Google 登录」重试"
        ));
    }

    let provider = settings.mail.oauth.provider;
    let preset = oauth_provider_preset(provider);
    let preset_id = preset.map(|p| p.id.clone()).unwrap_or_else(|| "custom".to_string());

    let old_email = settings.mail.oauth.email.trim().to_ascii_lowercase();
    let switched = !email.is_empty() && old_email != email.to_ascii_lowercase();

    // 1) 换账号：先原子清空旧凭证，再写新账号（避免旧 refresh_token/scope 混入新账号）。
    if switched {
        let o = &mut settings.mail.oauth;
        o.email.clear();
        o.refresh_token.clear();
        o.access_token.clear();
        o.expires_at_ms = 0;
        o.scope.clear();
    }

    let oauth: &mut MailOAuthSettings = &mut settings.mail.oauth;
    oauth.enabled = true;
    if !email.is_empty() {
        oauth.email = email.clone();
    }
    oauth.refresh_token = result.refresh_token.trim().to_string();
    if !result.access_token.trim().is_empty() {
        oauth.access_token = result.access_token.trim().to_string();
    }
    if result.expires_at_ms > 0 {
        oauth.expires_at_ms = result.expires_at_ms;
    }
    if !result.scope.trim().is_empty() {
        oauth.scope = result.scope.trim().to_string();
    }
    if provider == OAuthProvider::Microsoft && oauth.tenant.trim().is_empty() {
        oauth.tenant = "common".to_string();
    }

    // 2) 服务器参数（只对内置预设自动填；custom 不动用户手填值）
    settings.mail.provider = preset_id;
    if let Some(p) = preset {
        if p.id != "custom" {
            settings.mail.imap.host = p.imap_host.clone();
            settings.mail.imap.port = p.imap_port;
            settings.mail.imap.tls = p.imap_tls;
            settings.mail.smtp.host = p.smtp_host.clone();
            settings.mail.smtp.port = p.smtp_port;
            match p.smtp_security {
                SmtpSecurity::ImplicitTls => {
                    settings.mail.smtp.implicit_tls = true;
                    settings.mail.smtp.starttls = false;
                }
                SmtpSecurity::StartTls => {
                    settings.mail.smtp.implicit_tls = false;
                    settings.mail.smtp.starttls = true;
                }
                SmtpSecurity::None => {
                    settings.mail.smtp.implicit_tls = false;
                    settings.mail.smtp.starttls = false;
                }
            }
        }
    }

    // 3) 用户名 / 发件人：拿到邮箱就**强制同步**（用户报过"换账号后 IMAP/SMTP 还是旧邮箱"）。
    //    没有邮箱（token 里拿不到）时不覆盖，留给用户手填。
    if !email.is_empty() {
        settings.mail.imap.user = email.clone();
        settings.mail.smtp.user = email.clone();
        settings.mail.smtp.from = email.clone();
    }
    if settings.mail.imap.mailbox.trim().is_empty() {
        settings.mail.imap.mailbox = "INBOX".to_string();
    }

    settings.mail.enabled = true;
    crate::oauth::clear_needs_reauth();

    let mut lines = if email.is_empty() {
        vec!["OAuth2 登录已保存（未能自动获取邮箱地址）".to_string()]
    } else if switched {
        vec![format!("已切换 OAuth2 账号：{old_email} -> {email}（旧令牌已清除）")]
    } else {
        vec![format!("OAuth2 登录已保存：{email}")]
    };
    if email.is_empty() {
        lines.push(
            "请在邮件设置里填写邮箱地址（账号/发件人），否则无法收信/发信；OAuth 授权本身已完成"
                .to_string(),
        );
    }
    if switched {
        lines.push("已强制同步 IMAP 用户 / SMTP 用户 / 发件人为新邮箱".to_string());
    }
    if let Some(p) = preset {
        if p.id != "custom" {
            lines.push(format!(
                "IMAP 自动填写 {}:{}（{}）",
                p.imap_host,
                p.imap_port,
                if p.imap_tls { "直连 TLS" } else { "明文" }
            ));
            lines.push(format!(
                "SMTP 自动填写 {}:{}（{}）",
                p.smtp_host,
                p.smtp_port,
                p.smtp_security.label()
            ));
        }
    }
    if !result.scope.trim().is_empty() {
        lines.push(format!("授权 scope：{}", result.scope));
    }
    Ok(lines.join("\n"))
}
// ============================================================
// 旧配置迁移
// ============================================================

/// 加载 / 热更新 settings 后调用：修复旧版本里“OAuth 授权成功、token 有值，
/// 但 imap/smtp host 为空、mail.enabled=false”的配置（用户真实反馈的故障）。
///
/// 返回 Some(中文说明) 表示做了修复，app 应把修复后的 settings 保存回
/// `config/settings.toml` 并重启邮件轮询。
pub fn migrate_settings(settings: &mut Settings) -> Option<String> {
    let mut notes: Vec<String> = Vec::new();

    // 旧版本内置的 Thunderbird client_id：清掉，避免授权页继续显示 Thunderbird。
    if crate::oauth::is_legacy_thunderbird_client_id(&settings.mail.oauth.client_id) {
        settings.mail.oauth.client_id.clear();
        notes.push(
            "已清除历史 Thunderbird client_id（Microsoft 将使用 Office 公共客户端；要自己的应用名请填 Azure client_id）"
                .to_string(),
        );
    }
    // 旧配置里存的 Microsoft scope 可能不含 offline_access（token 响应的 granted scope 常缺它），
    // 会导致重新授权拿不到 refresh_token；这里补齐并保存。
    if settings.mail.oauth.provider == OAuthProvider::Microsoft {
        let scope = settings.mail.oauth.scope.trim().to_string();
        if !scope.is_empty() && !scope.split_whitespace().any(|s| s == "offline_access") {
            settings.mail.oauth.scope = format!("{scope} offline_access");
            notes.push("已为 Microsoft scope 补上 offline_access（否则拿不到 refresh_token）".to_string());
        }
    }

    let oauth_enabled = settings.mail.oauth.enabled;
    let has_refresh = !settings.mail.oauth.refresh_token.trim().is_empty();
    if !oauth_enabled || !has_refresh {
        return if notes.is_empty() { None } else { Some(notes.join("；")) };
    }
    let imap_empty = settings.mail.imap.host.trim().is_empty();
    let smtp_empty = settings.mail.smtp.host.trim().is_empty();
    let provider = settings.mail.oauth.provider;
    if let Some(preset) = oauth_provider_preset(provider) {
        if preset.id != "custom" {
            let mut filled = Vec::new();
            if imap_empty {
                settings.mail.imap.host = preset.imap_host.clone();
                settings.mail.imap.port = preset.imap_port;
                settings.mail.imap.tls = preset.imap_tls;
                filled.push(format!(
                    "IMAP {}:{}（{}）",
                    preset.imap_host,
                    preset.imap_port,
                    if preset.imap_tls { "直连 TLS" } else { "明文" }
                ));
            }
            if smtp_empty {
                settings.mail.smtp.host = preset.smtp_host.clone();
                settings.mail.smtp.port = preset.smtp_port;
                match preset.smtp_security {
                    SmtpSecurity::ImplicitTls => {
                        settings.mail.smtp.implicit_tls = true;
                        settings.mail.smtp.starttls = false;
                    }
                    SmtpSecurity::StartTls => {
                        settings.mail.smtp.implicit_tls = false;
                        settings.mail.smtp.starttls = true;
                    }
                    SmtpSecurity::None => {
                        settings.mail.smtp.implicit_tls = false;
                        settings.mail.smtp.starttls = false;
                    }
                }
                filled.push(format!(
                    "SMTP {}:{}（{}）",
                    preset.smtp_host,
                    preset.smtp_port,
                    preset.smtp_security.label()
                ));
            }
            settings.mail.provider = preset.id.clone();
            if !filled.is_empty() {
                notes.push(format!("已按「{}」自动回填 {}", preset.label, filled.join("、")));
            }
        }
    }
    // 用户点了 OAuth 登录就是要用邮箱：自动启用
    if !settings.mail.enabled {
        settings.mail.enabled = true;
        notes.push("已自动启用邮件服务（mail.enabled=true）".to_string());
    }
    // 用户名/发件人以 OAuth 邮箱为准：为空或与 oauth.email 不一致时覆盖（旧配置常见
    // “换了 OAuth 账号但 imap.user/smtp.user/from 还是旧邮箱”，会导致用旧账号收发信）。
    let email = settings.mail.oauth.email.trim().to_string();
    if !email.is_empty() {
        let mut synced = Vec::new();
        if !settings.mail.imap.user.trim().eq_ignore_ascii_case(&email) {
            settings.mail.imap.user = email.clone();
            synced.push("IMAP 用户");
        }
        if !settings.mail.smtp.user.trim().eq_ignore_ascii_case(&email) {
            settings.mail.smtp.user = email.clone();
            synced.push("SMTP 用户");
        }
        if !settings.mail.smtp.from.trim().eq_ignore_ascii_case(&email) {
            settings.mail.smtp.from = email.clone();
            synced.push("发件人");
        }
        if !synced.is_empty() {
            notes.push(format!(
                "已按 OAuth 邮箱 {email} 同步 {}（覆盖旧账号）",
                synced.join("/")
            ));
        }
    }
    if notes.is_empty() {
        None
    } else {
        Some(notes.join("；"))
    }
}

// ============================================================
// 诊断提示
// ============================================================

/// 根据服务商与错误原文给出可操作的中文建议。
pub fn diagnose_hint(provider: &str, imap_host: &str, smtp_error: bool, error: &str) -> Option<String> {
    let preset = preset(provider).or_else(|| suggest_provider(imap_host).and_then(preset));
    let lower = error.to_ascii_lowercase();
    let mut hints: Vec<String> = Vec::new();
    if lower.contains("unsafe login") {
        hints.push(
            "服务器返回 Unsafe Login：网易 163/126 要求客户端发送 IMAP ID（本程序在 993 直连 TLS 下会自动发送；请确认端口 993、tls=true，不要用 143/STARTTLS）"
                .to_string(),
        );
    }
    if lower.contains("authentication") || lower.contains("invalid credentials") || lower.contains("授权码") {
        if let Some(p) = preset {
            hints.push(p.imap_hint.clone());
            if smtp_error {
                hints.push(p.smtp_hint.clone());
            }
        }
    }
    if lower.contains("no response") || lower.contains("bad response") {
        if let Some(p) = preset {
            hints.push(format!("请确认 {} 的 IMAP 已开启，且密码是授权码而不是登录密码", p.label));
        }
    }
    if hints.is_empty() {
        None
    } else {
        Some(hints.join("\n"))
    }
}

/// 用户点“测试连接”但还没填服务器时给的引导文案。
pub fn missing_config_hint(provider: &str) -> String {
    match preset(provider) {
        Some(preset) if preset.id != "custom" => format!(
            "检测到邮箱类型是「{}」，但 IMAP/SMTP 参数不完整：请在设置页重新选择邮箱类型（会自动填入 {}:{} / {}:{}），再填写账号与密码/授权码。",
            preset.label,
            preset.imap_host,
            preset.imap_port,
            preset.smtp_host,
            preset.smtp_port
        ),
        _ => "未配置邮件服务器：请在设置页选择邮箱类型（QQ/163/126/Outlook/Gmail…）自动填服务器，或手动填写 IMAP/SMTP 主机、端口、账号和密码/授权码；Outlook/Gmail 推荐点 OAuth2 登录按钮。"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_have_expected_ids_and_hosts() {
        let ids: Vec<&str> = presets().iter().map(|p| p.id.as_str()).collect();
        for expected in ["outlook", "gmail", "163", "126", "qq", "custom"] {
            assert!(ids.contains(&expected), "missing preset {expected}");
        }
        let outlook = preset("outlook").unwrap();
        assert_eq!(outlook.imap_host, "outlook.office365.com");
        assert_eq!(outlook.imap_port, 993);
        assert!(outlook.imap_tls);
        assert_eq!(outlook.smtp_host, "smtp.office365.com");
        assert_eq!(outlook.smtp_port, 587);
        assert_eq!(outlook.smtp_security, SmtpSecurity::StartTls);
        assert_eq!(outlook.oauth_provider, Some(OAuthProvider::Microsoft));

        let gmail = preset("gmail").unwrap();
        assert_eq!(gmail.imap_host, "imap.gmail.com");
        assert_eq!(gmail.smtp_host, "smtp.gmail.com");
        assert_eq!(gmail.smtp_port, 587);

        let netease = preset("163").unwrap();
        assert_eq!(netease.imap_host, "imap.163.com");
        assert_eq!(netease.smtp_host, "smtp.163.com");
        assert_eq!(netease.smtp_port, 465);
        assert_eq!(netease.smtp_security, SmtpSecurity::ImplicitTls);
        assert!(netease.password_only());
        assert!(netease.imap_hint.contains("授权码"));

        let qq = preset("QQ").unwrap();
        assert_eq!(qq.id, "qq");
        assert_eq!(qq.imap_host, "imap.qq.com");
        assert_eq!(qq.smtp_host, "smtp.qq.com");
        assert!(qq.imap_hint.contains("授权码"));
    }

    #[test]
    fn suggest_provider_by_host() {
        assert_eq!(suggest_provider("imap.163.com"), Some("163"));
        assert_eq!(suggest_provider("IMAP.126.COM"), Some("126"));
        assert_eq!(suggest_provider("imap.qq.com"), Some("qq"));
        assert_eq!(suggest_provider("imap.gmail.com"), Some("gmail"));
        assert_eq!(suggest_provider("outlook.office365.com"), Some("outlook"));
        assert_eq!(suggest_provider("mail.example.com"), None);
        assert_eq!(suggest_provider(""), None);
    }

    #[test]
    fn imap_id_required_rules() {
        assert!(imap_id_required("163", ""));
        assert!(imap_id_required("126", ""));
        assert!(imap_id_required("qq", ""));
        assert!(imap_id_required("", "imap.163.com"));
        assert!(imap_id_required("", "imap.126.com"));
        assert!(imap_id_required("", "imap.yeah.net"));
        assert!(!imap_id_required("", "imap.gmail.com"));
        assert!(!imap_id_required("custom", "mail.example.com"));
        let cmd = imap_id_command();
        assert!(cmd.starts_with("ID (\"name\" \"NeoSonia\""));
        assert!(cmd.contains("\"vendor\" \"NeoSonia\""));
        assert!(cmd.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn apply_preset_fills_servers_but_keeps_credentials() {
        let mut mail = MailSettings::default();
        mail.imap.user = "me@qq.com".into();
        mail.imap.password = "authcode".into();
        let summary = apply_preset(&mut mail, "qq").expect("apply qq");
        assert!(summary.contains("QQ"));
        assert_eq!(mail.provider, "qq");
        assert_eq!(mail.imap.host, "imap.qq.com");
        assert_eq!(mail.imap.port, 993);
        assert!(mail.imap.tls);
        assert_eq!(mail.smtp.host, "smtp.qq.com");
        assert_eq!(mail.smtp.port, 465);
        assert!(mail.smtp.implicit_tls);
        assert!(!mail.smtp.starttls);
        // 账号/密码不被覆盖
        assert_eq!(mail.imap.user, "me@qq.com");
        assert_eq!(mail.imap.password, "authcode");

        let mut mail2 = MailSettings::default();
        apply_preset(&mut mail2, "outlook").unwrap();
        assert_eq!(mail2.oauth.provider, OAuthProvider::Microsoft);
        assert_eq!(mail2.oauth.tenant, "common");
        assert_eq!(mail2.smtp.port, 587);
        assert!(mail2.smtp.starttls == true);
        assert!(mail2.smtp.implicit_tls == false);

        assert!(apply_preset(&mut mail2, "不存在").is_err());
    }

    #[test]
    fn apply_oauth_result_fills_everything() {
        let mut settings = Settings::default();
        settings.mail.oauth.provider = OAuthProvider::Microsoft;
        let result = OAuthResult {
            email: "user@outlook.com".into(),
            refresh_token: "rt-1".into(),
            access_token: "at-1".into(),
            expires_at_ms: neosonia_core::util::now_ms() + 3_600_000,
            scope: "scope-a".into(),
            token_type: "Bearer".into(),
        };
        let summary = apply_oauth_result(&mut settings, &result).expect("apply oauth");
        assert!(summary.contains("user@outlook.com"));
        assert!(settings.mail.oauth.enabled);
        assert_eq!(settings.mail.oauth.email, "user@outlook.com");
        assert_eq!(settings.mail.oauth.refresh_token, "rt-1");
        assert_eq!(settings.mail.oauth.access_token, "at-1");
        assert_eq!(settings.mail.oauth.scope, "scope-a");
        assert_eq!(settings.mail.provider, "outlook");
        assert_eq!(settings.mail.imap.host, "outlook.office365.com");
        assert_eq!(settings.mail.imap.port, 993);
        assert!(settings.mail.imap.tls);
        assert_eq!(settings.mail.smtp.host, "smtp.office365.com");
        assert_eq!(settings.mail.smtp.port, 587);
        assert!(settings.mail.smtp.starttls);
        assert_eq!(settings.mail.imap.user, "user@outlook.com");
        assert_eq!(settings.mail.smtp.from, "user@outlook.com");
        assert!(settings.mail.enabled);
    }

    #[test]
    fn apply_oauth_result_failure_keeps_old_credentials() {
        // 旧账号 A 已配置好
        let mut settings = Settings::default();
        settings.mail.oauth.provider = OAuthProvider::Microsoft;
        settings.mail.oauth.enabled = true;
        settings.mail.oauth.email = "old-a@outlook.com".into();
        settings.mail.oauth.refresh_token = "rt-old".into();
        settings.mail.oauth.access_token = "at-old".into();
        settings.mail.oauth.expires_at_ms = neosonia_core::util::now_ms() + 3_600_000;
        settings.mail.imap.user = "old-a@outlook.com".into();
        settings.mail.smtp.user = "old-a@outlook.com".into();
        settings.mail.smtp.from = "old-a@outlook.com".into();

        // 新账号 B 的授权流程失败（没有 refresh_token）
        let result = OAuthResult {
            email: "new-b@outlook.com".into(),
            refresh_token: String::new(),
            access_token: "at-new-partial".into(),
            ..OAuthResult::default()
        };
        let err = apply_oauth_result(&mut settings, &result)
            .expect_err("缺少 refresh_token 应返回错误")
            .to_string();
        assert!(err.contains("缺少 refresh_token"), "got: {err}");
        // 旧凭证/旧账号原样保留，一个字段都不动
        assert_eq!(settings.mail.oauth.email, "old-a@outlook.com");
        assert_eq!(settings.mail.oauth.refresh_token, "rt-old");
        assert_eq!(settings.mail.oauth.access_token, "at-old");
        assert!(settings.mail.oauth.enabled);
        assert_eq!(settings.mail.imap.user, "old-a@outlook.com");
        assert_eq!(settings.mail.smtp.user, "old-a@outlook.com");
        assert_eq!(settings.mail.smtp.from, "old-a@outlook.com");
        // 需要重新授权标记已置
        assert!(crate::oauth::needs_reauth());
        crate::oauth::clear_needs_reauth();
    }

    #[test]
    fn apply_oauth_result_switches_account_atomically() {
        let mut settings = Settings::default();
        settings.mail.oauth.provider = OAuthProvider::Microsoft;
        settings.mail.oauth.enabled = true;
        settings.mail.oauth.email = "old-a@outlook.com".into();
        settings.mail.oauth.refresh_token = "rt-old".into();
        settings.mail.oauth.access_token = "at-old".into();
        settings.mail.oauth.scope = "scope-old".into();
        settings.mail.oauth.expires_at_ms = 111;
        settings.mail.imap.user = "old-a@outlook.com".into();
        settings.mail.smtp.user = "old-a@outlook.com".into();
        settings.mail.smtp.from = "old-a@outlook.com".into();

        let result = OAuthResult {
            email: "new-b@outlook.com".into(),
            refresh_token: "rt-new".into(),
            access_token: "at-new".into(),
            expires_at_ms: neosonia_core::util::now_ms() + 3_600_000,
            scope: "scope-new".into(),
            token_type: "Bearer".into(),
        };
        let summary = apply_oauth_result(&mut settings, &result).expect("switch account");
        assert!(summary.contains("已切换"), "summary: {summary}");
        // 新账号凭证完全替换旧账号
        assert_eq!(settings.mail.oauth.email, "new-b@outlook.com");
        assert_eq!(settings.mail.oauth.refresh_token, "rt-new");
        assert_eq!(settings.mail.oauth.access_token, "at-new");
        assert!(settings.mail.oauth.enabled);
        assert!(settings.mail.oauth.expires_at_ms > 111);
        // IMAP/SMTP 用户/发件人强制变成 B（这是用户报的 bug）
        assert_eq!(settings.mail.imap.user, "new-b@outlook.com");
        assert_eq!(settings.mail.smtp.user, "new-b@outlook.com");
        assert_eq!(settings.mail.smtp.from, "new-b@outlook.com");
        // 服务器配置按 provider 重填
        assert_eq!(settings.mail.imap.host, "outlook.office365.com");
        assert_eq!(settings.mail.smtp.host, "smtp.office365.com");
    }

    #[test]
    fn clear_oauth_authorization_keeps_servers() {
        let mut settings = Settings::default();
        settings.mail.oauth.enabled = true;
        settings.mail.oauth.email = "a@outlook.com".into();
        settings.mail.oauth.refresh_token = "rt".into();
        settings.mail.oauth.access_token = "at".into();
        settings.mail.oauth.scope = "scope".into();
        settings.mail.oauth.expires_at_ms = 123;
        settings.mail.oauth.client_id = "my-client".into();
        settings.mail.imap.host = "outlook.office365.com".into();
        settings.mail.imap.port = 993;
        settings.mail.imap.user = "a@outlook.com".into();
        settings.mail.smtp.host = "smtp.office365.com".into();
        settings.mail.smtp.port = 587;

        let note = clear_oauth_authorization(&mut settings);
        assert!(note.contains("已退出"), "note: {note}");
        assert!(!settings.mail.oauth.enabled);
        assert!(settings.mail.oauth.email.is_empty());
        assert!(settings.mail.oauth.refresh_token.is_empty());
        assert!(settings.mail.oauth.access_token.is_empty());
        assert!(settings.mail.oauth.scope.is_empty());
        assert_eq!(settings.mail.oauth.expires_at_ms, 0);
        // 服务器配置/用户名/客户端 id 保留
        assert_eq!(settings.mail.imap.host, "outlook.office365.com");
        assert_eq!(settings.mail.imap.port, 993);
        assert_eq!(settings.mail.smtp.host, "smtp.office365.com");
        assert_eq!(settings.mail.smtp.port, 587);
        assert_eq!(settings.mail.imap.user, "a@outlook.com");
        assert_eq!(settings.mail.oauth.client_id, "my-client");
    }

    #[test]
    fn apply_oauth_result_without_email_still_fills_servers() {
        let mut settings = Settings::default();
        settings.mail.oauth.provider = OAuthProvider::Microsoft;
        let mut result = OAuthResult {
            email: "a@b.com".into(),
            refresh_token: "rt".into(),
            ..OAuthResult::default()
        };
        result.refresh_token.clear();
        let err = apply_oauth_result(&mut settings, &result).unwrap_err().to_string();
        assert!(err.contains("refresh_token"), "got: {err}");
        crate::oauth::clear_needs_reauth();

        // email 拿不到（Token 不含 id_token 且无兜底）不再阻断：host 照填，提示用户补邮箱。
        result.refresh_token = "rt".into();
        result.email.clear();
        let summary = apply_oauth_result(&mut settings, &result).expect("no email should still work");
        assert!(summary.contains("邮箱地址"), "got: {summary}");
        assert!(settings.mail.oauth.enabled);
        assert!(settings.mail.enabled);
        assert_eq!(settings.mail.imap.host, "outlook.office365.com");
        // 没有新邮箱就不覆盖旧用户名
        assert!(settings.mail.oauth.email.is_empty());
        assert!(settings.mail.imap.user.is_empty());
        // 没有伪装 client_id 写入
        assert!(settings.mail.oauth.client_id.is_empty());
    }

    #[test]
    fn migrate_syncs_users_to_oauth_email() {
        // 旧配置：OAuth 已换成 B，但 imap/smtp 用户仍是 A（用户报的“换了登录但没变”）
        let mut settings = Settings::default();
        settings.mail.oauth.enabled = true;
        settings.mail.oauth.provider = OAuthProvider::Microsoft;
        settings.mail.oauth.email = "new-b@outlook.com".into();
        settings.mail.oauth.refresh_token = "rt".into();
        settings.mail.imap.host = "outlook.office365.com".into();
        settings.mail.smtp.host = "smtp.office365.com".into();
        settings.mail.imap.user = "old-a@outlook.com".into();
        settings.mail.smtp.user = "old-a@outlook.com".into();
        settings.mail.smtp.from = "old-a@outlook.com".into();
        let note = migrate_settings(&mut settings).expect("should migrate");
        assert!(note.contains("new-b@outlook.com"), "note: {note}");
        assert_eq!(settings.mail.imap.user, "new-b@outlook.com");
        assert_eq!(settings.mail.smtp.user, "new-b@outlook.com");
        assert_eq!(settings.mail.smtp.from, "new-b@outlook.com");
        // 幂等
        assert!(migrate_settings(&mut settings).is_none());
    }

    #[test]
    fn migrate_cleans_legacy_thunderbird_client_id_and_scope() {
        let mut settings = Settings::default();
        settings.mail.oauth.enabled = true;
        settings.mail.oauth.provider = OAuthProvider::Microsoft;
        settings.mail.oauth.client_id = crate::oauth::LEGACY_THUNDERBIRD_MICROSOFT_CLIENT_ID.into();
        settings.mail.oauth.scope =
            "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send"
                .into();
        settings.mail.oauth.refresh_token = "rt".into();
        let note = migrate_settings(&mut settings).expect("should migrate");
        assert!(settings.mail.oauth.client_id.is_empty(), "legacy id should be cleared");
        assert!(settings.mail.oauth.scope.contains("offline_access"));
        assert!(note.contains("offline_access"), "note: {note}");
        assert!(note.contains("Thunderbird"), "note: {note}");
        // 第二次幂等
        assert!(migrate_settings(&mut settings).is_none());
    }

    #[test]
    fn migrate_real_user_legacy_config() {
        // 复刻用户真实故障配置：OAuth token 有值，但 imap/smtp host 为空、mail.enabled=false。
        let mut settings = Settings::default();
        settings.mail.enabled = false;
        settings.mail.oauth.enabled = true;
        settings.mail.oauth.provider = OAuthProvider::Microsoft;
        settings.mail.oauth.email = "user@outlook.com".into();
        settings.mail.oauth.refresh_token = "M.C5xxxx".into();
        settings.mail.oauth.access_token = "EwAo...".into();
        settings.mail.oauth.expires_at_ms = neosonia_core::util::now_ms() + 3_600_000;
        settings.mail.imap.user = "user@outlook.com".into();
        settings.mail.smtp.user = "user@outlook.com".into();
        settings.mail.smtp.from = "user@outlook.com".into();

        let note = migrate_settings(&mut settings).expect("旧配置应被修复");
        assert!(note.contains("Outlook") || note.contains("outlook") || note.contains("自动回填"), "note={note}");
        assert!(settings.mail.enabled, "应自动启用邮件");
        assert_eq!(settings.mail.provider, "outlook");
        assert_eq!(settings.mail.imap.host, "outlook.office365.com");
        assert_eq!(settings.mail.imap.port, 993);
        assert!(settings.mail.imap.tls);
        assert_eq!(settings.mail.smtp.host, "smtp.office365.com");
        assert_eq!(settings.mail.smtp.port, 587);
        assert!(settings.mail.smtp.starttls);
        assert!(settings.mail.smtp.implicit_tls == false);
        // 第二次调用不再重复修复
        assert!(migrate_settings(&mut settings).is_none());
    }

    #[test]
    fn migrate_skips_password_mode() {
        let mut settings = Settings::default();
        settings.mail.oauth.enabled = false;
        settings.mail.imap.host = "imap.qq.com".into();
        assert!(migrate_settings(&mut settings).is_none());
    }

    #[test]
    fn diagnose_hint_for_netease_unsafe_login() {
        let hint = diagnose_hint("163", "imap.163.com", false, "NO [CANNOT] Unsafe Login");
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("IMAP ID"));
        let none = diagnose_hint("custom", "mail.example.com", false, "connection refused");
        assert!(none.is_none());
        let missing = missing_config_hint("qq");
        assert!(missing.contains("QQ"));
    }
}

/// 退出登录：清除 OAuth 凭证，**保留** IMAP/SMTP 服务器地址、端口与账号，
/// 以便用户继续用密码/授权码登录；client_id/client_secret/tenant/端点也保留。
///
/// 返回给 UI 的中文说明。
pub fn clear_oauth_authorization(settings: &mut Settings) -> String {
    {
        let o = &mut settings.mail.oauth;
        o.enabled = false;
        o.email.clear();
        o.refresh_token.clear();
        o.access_token.clear();
        o.expires_at_ms = 0;
        o.scope.clear();
    }
    crate::oauth::clear_needs_reauth();
    "已退出登录：OAuth 凭证已清除；IMAP/SMTP 服务器与账号保留，可继续用密码/授权码登录。".to_string()
}
