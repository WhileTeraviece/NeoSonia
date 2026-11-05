//! # neosonia-mail
//!
//! NeoSonia 的邮件能力：**直接 IMAP 收信 + SMTP 发信**，不依赖 Outlook、
//! 不依赖 win32com / MAPI、也不调用任何本机邮件客户端。所有连接参数都来自
//! `neosonia_core::config::Settings::mail`（UI 可热改）。
//!
//! 模块划分：
//!
//! - [`parse`]：MIME 解析（mail-parser）+ HTML 去标签 + 折叠头解析，纯函数、可单测；
//! - [`service`]：`MailService`，IMAP（imap 2.4，阻塞 API 包在 `spawn_blocking` 里）
//!   与 SMTP（lettre 0.11，rustls）的实际网络操作；
//! - [`tools`]：8 个模型可调用的工具，注册进 `ToolRegistry`；
//! - [`poller`]：后台轮询器 `MailPoller`，把新邮件通过 [`MailEvent`] 推给 app。
//!
//! 约定：
//! - 所有面向用户/模型的错误都是中文，不带 emoji；
//! - 不 panic：所有失败都转成 `anyhow::Error` 或 `ToolOutcome::err`；
//! - 阻塞的 IMAP 操作全部放进 `tokio::task::spawn_blocking` + `tokio::time::timeout`，
//!   不会卡 UI 线程。

pub mod attachments;
pub mod oauth;
pub mod parse;
pub mod poller;
pub mod presets;
pub mod service;
pub mod tools;

pub use attachments::{
    load_attachments, mime_for_path, summary_line, MailAttachment, MAX_ATTACHMENTS,
    MAX_SINGLE_BYTES, MAX_TOTAL_BYTES,
};
pub use neosonia_core::config::{MailOAuthSettings, OAuthProvider};
pub use oauth::{
    access_token_claims, build_authorize_url, build_xoauth2, clear_needs_reauth,
    error_text_needs_reauth, form_encode, jwt_payload, mark_needs_reauth, needs_reauth,
    pkce_challenge, pkce_verifier, resolved_client_id, resolved_scope,
    token_result_from_body, token_result_from_body_opts, AccessTokenClaims, DeviceCodeInfo, OAuth,
    OAuthFlow, OAuthResult, OAuthTokens, XOAuth2Authenticator, CLIENT_ID_MISSING_HINT,
    GOOGLE_SCOPE, MICROSOFT_CLIENT_ID, MICROSOFT_SCOPE,
};
pub use poller::MailPoller;
pub use presets::{
    apply_oauth_result, apply_preset, clear_oauth_authorization, diagnose_hint, imap_id_command,
    imap_id_required, migrate_settings, missing_config_hint, oauth_provider_preset, preset,
    presets, suggest_provider, MailAuthKind, MailPreset, SmtpSecurity,
};
pub use service::{
    test_connection, ConnectionReport, ConnectionResult, MailService, OAuthStatus,
};
pub use tools::{mail_tool_specs, register, tool_names, TOOL_NAMES};

use serde::{Deserialize, Serialize};

/// `mail_wait_for` 默认等待秒数。
pub const DEFAULT_WAIT_SECS: u64 = 120;
/// `mail_wait_for` 允许的最大等待秒数。
pub const MAX_WAIT_SECS: u64 = 600;
/// 列表类工具默认返回条数。
pub const DEFAULT_LIST_LIMIT: usize = 20;
/// 列表类工具允许的最大条数。
pub const MAX_LIST_LIMIT: usize = 50;

// ============================================================
// 数据模型（docs/contracts.md 6.7 节）
// ============================================================

/// 收件箱列表里的一封邮件（轻量摘要）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MailSummary {
    /// IMAP UID（在同一文件夹内稳定）。
    pub uid: u32,
    /// 发件人邮箱地址。
    pub from: String,
    /// 发件人显示名（没有则为空）。
    pub from_name: String,
    pub subject: String,
    /// 邮件日期，格式化后的字符串（解析失败时是原始 Date 头）。
    pub date: String,
    /// 正文前若干字符，供模型快速判断内容。
    pub snippet: String,
    /// 是否未读。
    pub unread: bool,
    /// 是否带附件。
    pub has_attachments: bool,
}

impl MailSummary {
    /// 单行摘要，给模型和 UI 用。
    pub fn one_line(&self) -> String {
        let from = if self.from_name.trim().is_empty() {
            self.from.clone()
        } else {
            format!("{} <{}>", self.from_name.trim(), self.from)
        };
        let flag = if self.unread { "[未读] " } else { "" };
        let att = if self.has_attachments { " [附件]" } else { "" };
        format!(
            "uid={} {} {} | {}{} | {}",
            self.uid,
            self.date,
            from,
            flag,
            self.subject,
            att
        )
        .trim_end()
        .to_string()
    }
}

/// 一封完整邮件。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MailFull {
    pub uid: u32,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub date: String,
    /// 纯文本正文（HTML 邮件会被转成纯文本）。
    pub body_text: String,
    pub has_attachments: bool,
    pub message_id: String,
    pub references: Vec<String>,
}

/// 要发送的一封邮件。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OutgoingMail {
    /// 收件人，支持 `"a@b.com"` 或 `"名字 <a@b.com>"`。
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub body: String,
    /// 回复时填被回复邮件的 Message-ID。
    #[serde(default)]
    pub in_reply_to: Option<String>,
    /// 回复时填 References 链（按顺序）。
    #[serde(default)]
    pub references: Vec<String>,
    /// 附件绝对路径（最多 10 个，单文件 ≤25MB，合计 ≤40MB；由 `MailService::send` 校验并读取）。
    #[serde(default)]
    pub attachments: Vec<String>,
}

impl OutgoingMail {
    pub fn new(to: impl Into<String>, subject: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            to: split_addresses(&to.into()),
            subject: subject.into(),
            body: body.into(),
            ..Self::default()
        }
    }

    pub fn with_cc(mut self, cc: impl Into<String>) -> Self {
        self.cc = split_addresses(&cc.into());
        self
    }

    /// 追加附件（绝对路径）。
    pub fn with_attachments(mut self, paths: Vec<String>) -> Self {
        self.attachments = paths;
        self
    }

    /// 是否看起来有收件人（只做粗校验，真正的地址合法性由 lettre 报错）。
    pub fn has_recipients(&self) -> bool {
        !self.to.is_empty()
    }
}

// ============================================================
// 轮询事件（app 层订阅）
// ============================================================

/// 后台轮询器推给 app 的事件。
#[derive(Clone, Debug)]
pub enum MailEvent {
    /// 轮询器已启动。
    Started,
    /// 收到一封新（未处理过的）邮件。
    NewMail(MailSummary),
    /// 自动回复已发出（由 app 层在 agent 回复成功后构造/转发）。
    Replied { uid: u32, to: String },
    /// 轮询出错（网络、认证、配置等）。
    Error(String),
    /// 本轮没有新邮件。
    Idle,
}

// ============================================================
// 公共小工具
// ============================================================

/// 把 `"a@b.com, c@d.com; e@f.com"` 拆成地址列表；也接受带显示名
/// 的 `"张三 <a@b.com>"`（显示名里的逗号不会被误拆）。
pub fn split_addresses(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_angle = false;
    let mut in_quote = false;
    for ch in raw.chars() {
        match ch {
            '<' => {
                in_angle = true;
                buf.push(ch);
            }
            '>' => {
                in_angle = false;
                buf.push(ch);
            }
            '"' => {
                in_quote = !in_quote;
                buf.push(ch);
            }
            ',' | ';' | '\n' if !in_angle && !in_quote => {
                let t = buf.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
                buf.clear();
            }
            _ => buf.push(ch),
        }
    }
    let t = buf.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    out
}

/// 从 `"名字 <a@b.com>"` 中取出邮箱地址。
pub fn address_of(raw: &str) -> String {
    let s = raw.trim();
    if let (Some(l), Some(r)) = (s.find('<'), s.rfind('>')) {
        if r > l {
            return s[l + 1..r].trim().to_string();
        }
    }
    s.to_string()
}

/// 从 `"名字 <a@b.com>"` 中取出显示名（没有则为空）。
pub fn display_name_of(raw: &str) -> String {
    let s = raw.trim();
    if let (Some(l), Some(r)) = (s.find('<'), s.rfind('>')) {
        if r > l {
            let name = s[..l].trim().trim_matches('"').trim();
            return name.to_string();
        }
    }
    String::new()
}

/// 按字符截断（保留尾部省略标记），不会破坏 UTF-8。
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max_chars {
            out.push_str("...（已截断）");
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_addresses_handles_display_names() {
        let v = split_addresses("张三 <a@b.com>, c@d.com; e@f.com\n g@h.com");
        assert_eq!(v, vec!["张三 <a@b.com>", "c@d.com", "e@f.com", "g@h.com"]);
        assert_eq!(address_of(&v[0]), "a@b.com");
        assert_eq!(display_name_of(&v[0]), "张三");
        assert_eq!(address_of("plain@x.com"), "plain@x.com");
        assert!(display_name_of("plain@x.com").is_empty());
    }

    #[test]
    fn summary_one_line_is_stable() {
        let s = MailSummary {
            uid: 7,
            from: "a@b.com".into(),
            from_name: "A".into(),
            subject: "你好".into(),
            date: "2026-01-02 03:04".into(),
            unread: true,
            has_attachments: true,
            ..Default::default()
        };
        let line = s.one_line();
        assert!(line.contains("uid=7"));
        assert!(line.contains("未读"));
        assert!(line.contains("[附件]"));
    }

    #[test]
    fn truncate_keeps_utf8() {
        assert_eq!(truncate_chars("中文中文", 2), "中文...（已截断）");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert!(truncate_chars("", 0).is_empty());
    }

    #[test]
    fn outgoing_mail_new_splits_recipients() {
        let m = OutgoingMail::new("a@b.com, c@d.com", "主题", "正文");
        assert_eq!(m.to.len(), 2);
        assert!(m.has_recipients());
    }
}
