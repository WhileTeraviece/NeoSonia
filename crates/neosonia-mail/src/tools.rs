//! 8 个邮件工具（注册进 `ToolRegistry`，名字与参数严格按 `docs/contracts.md` 6.7 节）。
//!
//! 所有工具都从 `ToolCtx::settings()` 取最新配置快照，UI 改了连接参数立刻生效。
//! 白名单规则：
//! - `mail_send`：收件人/抄送里有白名单外地址且 `allow_outbound_to_anyone = false` 时
//!   **明确拒绝**（`ok = false`）并说明怎么加白名单，绝不静默降级；
//! - `mail_reply`：只对白名单来信自动回复（`allow_outbound_to_anyone = true` 时放行任意地址）。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use neosonia_core::config::Settings;
use neosonia_core::model::{ToolCtx, ToolOutcome, ToolSpec};
use neosonia_core::registry::{Tool, ToolRegistry};
use serde_json::{json, Value};

use crate::service::MailService;
use crate::{address_of, split_addresses, truncate_chars, MailSummary, OutgoingMail, DEFAULT_WAIT_SECS, MAX_WAIT_SECS};

// ============================================================
// 注册
// ============================================================

/// 全部工具实例。
fn all_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(MailSendTool),
        Arc::new(MailListTool),
        Arc::new(MailReadTool),
        Arc::new(MailReplyTool),
        Arc::new(MailMarkReadTool),
        Arc::new(MailFoldersTool),
        Arc::new(MailTestConnectionTool),
        Arc::new(MailWaitForTool),
    ]
}

/// 把 8 个邮件工具注册进 registry。
pub fn register(reg: &ToolRegistry) {
    for tool in all_tools() {
        reg.register(tool);
    }
}

/// 全部工具名（固定顺序按字母排序，测试 / 文档用）。
pub const TOOL_NAMES: [&str; 8] = [
    "mail_folders",
    "mail_list",
    "mail_mark_read",
    "mail_read",
    "mail_reply",
    "mail_send",
    "mail_test_connection",
    "mail_wait_for",
];

/// 工具名清单。
pub fn tool_names() -> Vec<&'static str> {
    TOOL_NAMES.to_vec()
}

/// 全部工具规格（按名字排序，便于测试和 prompt 稳定）。
pub fn mail_tool_specs() -> Vec<ToolSpec> {
    let mut specs: Vec<ToolSpec> = all_tools().iter().map(|t| t.spec()).collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}

// ============================================================
// 参数解析小工具（缺字段给默认值，不 panic）
// ============================================================

fn arg_str(args: &Value, key: &str) -> String {
    args.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| {
        v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

fn arg_bool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// 接受字符串（逗号/分号分隔）或字符串数组，统一拆成地址列表。
fn arg_addresses(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(s) = item.as_str() {
                    out.extend(split_addresses(s));
                }
            }
            out
        }
        Some(Value::String(s)) => split_addresses(s),
        _ => Vec::new(),
    }
}

fn folder_from(args: &Value) -> String {
    let f = arg_str(args, "folder");
    if f.is_empty() { "INBOX".to_string() } else { f }
}

/// 附件参数：接受字符串数组（绝对路径），也兼容单个字符串路径。
/// 不按逗号/空格拆（Windows 路径可能含空格）。
fn arg_attachment_paths(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() { Vec::new() } else { vec![s.to_string()] }
        }
        _ => Vec::new(),
    }
}

fn service(ctx: &ToolCtx) -> (Settings, MailService) {
    let settings = ctx.settings();
    let svc = MailService::from_settings(&settings);
    (settings, svc)
}

// ============================================================
// 白名单
// ============================================================

fn whitelist_help() -> &'static str {
    "如需放行：打开设置页的「邮件」部分，在「白名单」里加上该地址（也支持 @qq.com 这种整个域名），\
     或者把 mail.allow_outbound_to_anyone 打开（允许代理给白名单外的人发信，风险自负）。"
}

/// 返回被白名单拦截的地址（空 = 全部允许）。
fn blocked_addresses(settings: &Settings, recipients: &[String]) -> Vec<String> {
    if settings.mail.allow_outbound_to_anyone {
        return Vec::new();
    }
    recipients
        .iter()
        .map(|r| address_of(r))
        .filter(|a| !a.is_empty() && !settings.is_whitelisted(a))
        .collect()
}

fn block_outcome(blocked: &[String]) -> ToolOutcome {
    ToolOutcome::err(format!(
        "已拦截：收件人 {} 不在邮件白名单里，且当前配置不允许给白名单外的人发信。{}",
        blocked.join("、"),
        whitelist_help()
    ))
    .with_display(format!("已拦截：{} 不在白名单", blocked.join("、")))
}

/// 查白名单备注（地址精确匹配，或 `@domain` 后缀匹配；大小写不敏感）。
fn whitelist_note(settings: &Settings, addr: &str) -> Option<String> {
    let addr = addr.trim();
    if addr.is_empty() {
        return None;
    }
    let addr_lower = addr.to_ascii_lowercase();
    for (key, note) in &settings.mail.whitelist_notes {
        let key_lower = key.trim().to_ascii_lowercase();
        let matched = if let Some(domain) = key_lower.strip_prefix('@') {
            addr_lower.ends_with(&format!("@{domain}")) || addr_lower == domain
        } else {
            key_lower == addr_lower
        };
        if matched && !note.trim().is_empty() {
            return Some(note.trim().to_string());
        }
    }
    None
}

/// 查白名单备注（地址精确匹配，或 `@domain` 后缀匹配；大小写不敏感）。

// ============================================================
// mail_send
// ============================================================

pub struct MailSendTool;

#[async_trait]
impl Tool for MailSendTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "mail_send",
            "用 SMTP 直接发送一封纯文本邮件（不经过 Outlook 或本机邮件客户端）。\
             什么时候用：用户明确要求发邮件、发通知、发测试邮件时。\
             参数：to 收件人（支持 \"a@b.com\" 或 \"名字 <a@b.com>\"，多个用逗号分隔）；subject 主题；\
             body 纯文本正文；cc 可选抄送；attachments 可选附件（绝对路径数组，最多 10 个，\
             单个文件不超过 25MB、合计不超过 40MB）。\
             限制：默认只允许发给设置里白名单内的地址，白名单外会直接失败并提示如何添加；\
             需要先在设置页填好 SMTP 主机/端口/账号/密码。\
             不要用它转发垃圾邮件或批量群发。",
            json!({
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "收件人地址，多个用逗号分隔，例如 \"a@b.com, 张三 <c@d.com>\""},
                    "subject": {"type": "string", "description": "邮件主题"},
                    "body": {"type": "string", "description": "纯文本正文"},
                    "cc": {"type": "string", "description": "抄送地址，多个用逗号分隔（可选）"},
                    "attachments": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "附件绝对路径列表（可选）。最多 10 个，单个 ≤25MB、合计 ≤40MB；例如 [\"C:\\\\Users\\\\me\\\\报告.pdf\", \"C:\\\\Users\\\\me\\\\图片.png\"]"
                    }
                },
                "required": ["to", "subject", "body"]
            }),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let (settings, svc) = service(ctx);
        let to = arg_addresses(&args, "to");
        let cc = arg_addresses(&args, "cc");
        let subject = arg_str(&args, "subject");
        let body = body_arg(&args);

        if to.is_empty() {
            return ToolOutcome::err("收件人 to 为空：请提供至少一个邮箱地址，例如 \"a@b.com\"");
        }
        if body.trim().is_empty() {
            return ToolOutcome::err("邮件正文 body 为空：请提供要发送的正文内容");
        }

        let blocked = blocked_addresses(&settings, &to);
        if !blocked.is_empty() {
            return block_outcome(&blocked);
        }
        let blocked_cc = blocked_addresses(&settings, &cc);
        if !blocked_cc.is_empty() {
            return block_outcome(&blocked_cc);
        }

        let subject = if subject.is_empty() { "(无主题)".to_string() } else { subject };
        let attachments = arg_attachment_paths(&args, "attachments");
        let attachment_count = attachments.len();
        let attachment_names: Vec<String> = attachments
            .iter()
            .map(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.clone())
            })
            .collect();
        let mail = OutgoingMail {
            to: to.clone(),
            cc,
            subject: subject.clone(),
            body,
            attachments,
            ..OutgoingMail::default()
        };

        ctx.phase("正在通过 SMTP 发送邮件...");
        match svc.send(mail).await {
            Ok(()) => {
                let names = to.iter().map(|a| address_of(a)).collect::<Vec<_>>().join("、");
                let attach_text = if attachment_count == 0 {
                    String::new()
                } else {
                    format!(
                        "；附件 {} 个（{}）",
                        attachment_count,
                        truncate_chars(&attachment_names.join("、"), 200)
                    )
                };
                ToolOutcome::ok(format!(
                    "邮件已发送。收件人：{names}；主题：{subject}{attach_text}"
                ))
                .with_display(format!("已发送给 {}", truncate_chars(&names, 60)))
            }
            Err(e) => ToolOutcome::err(format!("发送邮件失败：{e}")),
        }
    }
}

/// 正文既支持 `body` 也接受 `text`（模型有时用后者）。
fn body_arg(args: &Value) -> String {
    let b = arg_str(args, "body");
    if b.is_empty() { arg_str(args, "text") } else { b }
}

// ============================================================
// mail_list
// ============================================================

pub struct MailListTool;

#[async_trait]
impl Tool for MailListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "mail_list",
            "列出某个邮件文件夹（默认 INBOX 收件箱）里最近的邮件摘要（uid/发件人/主题/日期/未读/是否有附件）。\
             什么时候用：想知道邮箱里有什么新邮件、找某封邮件的 uid 时先用它（uid 是后续 mail_read / mail_reply 的钥匙）。\
             参数：folder 文件夹名（可选，默认 INBOX）；limit 返回条数（可选，默认 20，最大 50，取最新的）；\
             unread_only 只看未读（可选，默认 false）。\
             只返回摘要，不返回正文；要正文用 mail_read。",
            json!({
                "type": "object",
                "properties": {
                    "folder": {"type": "string", "description": "文件夹名，默认 INBOX，可用 mail_folders 查询"},
                    "limit": {"type": "integer", "description": "返回条数，默认 20，最大 50"},
                    "unread_only": {"type": "boolean", "description": "是否只看未读，默认 false"}
                }
            }),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let (settings, svc) = service(ctx);
        let folder = folder_from(&args);
        let limit = arg_u64(&args, "limit").unwrap_or(crate::DEFAULT_LIST_LIMIT as u64) as usize;
        let unread_only = arg_bool(&args, "unread_only", false);

        ctx.phase(format!("正在拉取 {folder} 的邮件列表..."));
        match svc.list(&folder, limit, unread_only).await {
            Ok(mails) => {
                if mails.is_empty() {
                    let suffix = if unread_only { "（只看了未读）" } else { "" };
                    return ToolOutcome::ok(format!("文件夹 {folder} 里没有符合条件的邮件{suffix}。"))
                        .with_display(format!("{folder}: 0 封"));
                }
                let mut lines = Vec::with_capacity(mails.len() + 2);
                lines.push(format!(
                    "文件夹 {folder} 最近 {} 封邮件（按 uid 升序，越靠后越新）：",
                    mails.len()
                ));
                // MailService::list 现在返回最新在前（UID 降序）；工具输出保持
                // “按 uid 升序、越靠后越新”的旧契约，模型仍能按顺序找最新邮件。
                for (i, m) in mails.iter().rev().enumerate() {
                    let line = match whitelist_note(&settings, &m.from) {
                        Some(note) => format!(
                            "{}. {}  [白名单备注：{}]",
                            i + 1,
                            m.one_line(),
                            truncate_chars(&note, 80)
                        ),
                        None => format!("{}. {}", i + 1, m.one_line()),
                    };
                    lines.push(line);
                }
                lines.push("提示：用 mail_read(uid=...) 读正文，用 mail_reply(uid=..., body=...) 回复。".to_string());
                let unread = mails.iter().filter(|m| m.unread).count();
                ToolOutcome::ok(lines.join("\n"))
                    .with_display(format!("{folder}: {} 封（未读 {}）", mails.len(), unread))
            }
            Err(e) => ToolOutcome::err(format!("拉取邮件列表失败：{e}")),
        }
    }
}

// ============================================================
// mail_read
// ============================================================

pub struct MailReadTool;

#[async_trait]
impl Tool for MailReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "mail_read",
            "按 uid 读取一封邮件的完整内容（发件人/收件人/抄送/主题/日期/是否有附件/纯文本正文）。\
             什么时候用：拿到 mail_list 或 mail_wait_for 返回的 uid 后，要看具体内容时。\
             参数：uid 邮件编号（必填，来自 mail_list 结果）；folder 文件夹名（可选，默认 INBOX）。\
             正文是纯文本，HTML 邮件会被自动转成纯文本；正文过长会截断并提示。",
            json!({
                "type": "object",
                "properties": {
                    "uid": {"type": "integer", "description": "邮件 UID，来自 mail_list 的结果"},
                    "folder": {"type": "string", "description": "文件夹名，默认 INBOX"}
                },
                "required": ["uid"]
            }),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let (settings, svc) = service(ctx);
        let Some(uid) = arg_u64(&args, "uid") else {
            return ToolOutcome::err("缺少参数 uid：请先用 mail_list 拿到邮件 uid");
        };
        let uid = uid as u32;
        let folder = folder_from(&args);

        ctx.phase(format!("正在读取邮件 uid={uid}..."));
        match svc.fetch(uid, &folder).await {
            Ok(mail) => {
                let mut lines = Vec::new();
                lines.push(format!("uid: {}", mail.uid));
                lines.push(format!("发件人: {}", mail.from));
                if let Some(note) = whitelist_note(&settings, &mail.from) {
                    lines.push(format!("白名单备注: {}", truncate_chars(&note, 300)));
                }
                lines.push(format!("收件人: {}", join_or_none(&mail.to)));
                if !mail.cc.is_empty() {
                    lines.push(format!("抄送: {}", mail.cc.join("、")));
                }
                lines.push(format!("主题: {}", mail.subject));
                lines.push(format!("日期: {}", mail.date));
                lines.push(format!("附件: {}", if mail.has_attachments { "有" } else { "无" }));
                if !mail.message_id.is_empty() {
                    lines.push(format!("Message-ID: {}", mail.message_id));
                }
                lines.push(String::new());
                lines.push("正文:".to_string());
                let limit = svc.max_body_chars();
                lines.push(truncate_chars(&mail.body_text, limit));
                ToolOutcome::ok(lines.join("\n"))
                    .with_display(format!("已读取 uid={uid}：{}", truncate_chars(&mail.subject, 50)))
            }
            Err(e) => ToolOutcome::err(format!("读取邮件 uid={uid} 失败：{e}")),
        }
    }
}

fn join_or_none(list: &[String]) -> String {
    if list.is_empty() { "(未知)".to_string() } else { list.join("、") }
}

// ============================================================
// mail_reply
// ============================================================

pub struct MailReplyTool;

#[async_trait]
impl Tool for MailReplyTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "mail_reply",
            "回复一封已有邮件：自动带上 Re: 主题前缀和 In-Reply-To / References 头，保持邮件线程。\
             什么时候用：用户要求回复某封邮件时。参数：uid 被回复邮件的编号（必填）；body 回复正文（必填）；\
             folder 文件夹名（可选，默认 INBOX）。\
             限制：只有发件人在白名单里的来信才会被执行（除非配置允许给任何人发信）；\
             回复内容由你直接给出，工具不会调用模型。",
            json!({
                "type": "object",
                "properties": {
                    "uid": {"type": "integer", "description": "被回复邮件的 UID，来自 mail_list"},
                    "body": {"type": "string", "description": "回复正文（纯文本）"},
                    "folder": {"type": "string", "description": "文件夹名，默认 INBOX"}
                },
                "required": ["uid", "body"]
            }),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let (settings, svc) = service(ctx);
        let Some(uid) = arg_u64(&args, "uid") else {
            return ToolOutcome::err("缺少参数 uid：请先用 mail_list 拿到邮件 uid");
        };
        let uid = uid as u32;
        let body = body_arg(&args);
        if body.trim().is_empty() {
            return ToolOutcome::err("回复正文 body 为空：请提供要回复的内容");
        }
        let folder = folder_from(&args);

        ctx.phase(format!("正在读取 uid={uid} 的原邮件..."));
        let original = match svc.fetch(uid, &folder).await {
            Ok(m) => m,
            Err(e) => return ToolOutcome::err(format!("回复失败：无法读取 uid={uid} 的原邮件：{e}")),
        };
        if original.from.trim().is_empty() {
            return ToolOutcome::err(format!("回复失败：uid={uid} 的邮件没有可用的发件人地址"));
        }
        if !settings.mail.allow_outbound_to_anyone && !settings.is_whitelisted(&original.from) {
            return ToolOutcome::err(format!(
                "已拦截：这封邮件的发件人 {} 不在白名单里，按配置不能自动回复。{}",
                original.from,
                whitelist_help()
            ))
            .with_display("已拦截：来信发件人不在白名单");
        }

        ctx.phase("正在通过 SMTP 发送回复...");
        match svc.reply(uid, &body, &folder).await {
            Ok(()) => ToolOutcome::ok(format!(
                "已回复 {}（uid={}，主题：{}）",
                original.from,
                uid,
                crate::service::reply_subject(&original.subject)
            ))
            .with_display(format!("已回复 {}", original.from)),
            Err(e) => ToolOutcome::err(format!("回复邮件失败：{e}")),
        }
    }
}

// ============================================================
// mail_mark_read
// ============================================================

pub struct MailMarkReadTool;

#[async_trait]
impl Tool for MailMarkReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "mail_mark_read",
            "把一封邮件标记为已读或未读（默认标记已读）。\
             什么时候用：用户说“把这封标记成已读/未读”，或需要清理未读状态时。\
             参数：uid 邮件编号（必填）；read 是否已读（可选，默认 true，false = 标记未读）；folder 文件夹名（可选）。",
            json!({
                "type": "object",
                "properties": {
                    "uid": {"type": "integer", "description": "邮件 UID"},
                    "read": {"type": "boolean", "description": "true 标记已读（默认），false 标记未读"},
                    "folder": {"type": "string", "description": "文件夹名，默认 INBOX"}
                },
                "required": ["uid"]
            }),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let (_settings, svc) = service(ctx);
        let Some(uid) = arg_u64(&args, "uid") else {
            return ToolOutcome::err("缺少参数 uid：请先用 mail_list 拿到邮件 uid");
        };
        let uid = uid as u32;
        let read = arg_bool(&args, "read", true);
        let folder = folder_from(&args);

        ctx.phase("正在修改邮件标记...");
        match svc.mark_read(uid, read, &folder).await {
            Ok(()) => ToolOutcome::ok(format!(
                "已把 {} 的 uid={} 标记为{}。",
                folder,
                uid,
                if read { "已读" } else { "未读" }
            ))
            .with_display(if read { "已标记为已读" } else { "已标记为未读" }),
            Err(e) => ToolOutcome::err(format!("修改邮件标记失败：{e}")),
        }
    }
}

// ============================================================
// mail_folders
// ============================================================

pub struct MailFoldersTool;

#[async_trait]
impl Tool for MailFoldersTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "mail_folders",
            "列出邮箱服务器上所有可选文件夹（INBOX、Sent、Drafts、Trash 等，名字因服务器而异）。\
             什么时候用：想确认某个文件夹的真实名字、准备用 folder 参数操作别的文件夹时。无参数。",
            json!({"type": "object", "properties": {}}),
        )
    }

    async fn call(&self, ctx: &ToolCtx, _args: Value) -> ToolOutcome {
        let (_settings, svc) = service(ctx);
        ctx.phase("正在获取邮件文件夹列表...");
        match svc.folders().await {
            Ok(folders) => {
                if folders.is_empty() {
                    return ToolOutcome::ok("服务器没有返回任何可选文件夹。").with_display("0 个文件夹");
                }
                let mut lines = Vec::with_capacity(folders.len() + 1);
                lines.push(format!("共 {} 个文件夹：", folders.len()));
                for (i, f) in folders.iter().enumerate() {
                    lines.push(format!("{}. {f}", i + 1));
                }
                ToolOutcome::ok(lines.join("\n")).with_display(format!("{} 个文件夹", folders.len()))
            }
            Err(e) => ToolOutcome::err(format!("获取文件夹列表失败：{e}")),
        }
    }
}

// ============================================================
// mail_test_connection
// ============================================================

pub struct MailTestConnectionTool;

#[async_trait]
impl Tool for MailTestConnectionTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "mail_test_connection",
            "测试邮件服务器连接是否配置正确（IMAP 登录 + SMTP NOOP，不发信）。\
             什么时候用：用户刚在设置页填完邮箱参数，或者收发邮件报错需要定位问题时。\
             参数：target 测试哪一个（可选：imap / smtp / both，默认 both）。\
             返回值会包含具体失败原因（账号密码、授权码、TLS、端口等）。",
            json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "enum": ["imap", "smtp", "both"],
                        "description": "测试哪个协议，默认 both"
                    }
                }
            }),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let (_settings, svc) = service(ctx);
        let target = {
            let t = arg_str(&args, "target").to_ascii_lowercase();
            if t.is_empty() { "both".to_string() } else { t }
        };
        if target != "imap" && target != "smtp" && target != "both" {
            return ToolOutcome::err("target 参数无效：只能是 imap / smtp / both");
        }
        ctx.phase("正在测试邮件服务器连接（IMAP 登录 + SMTP EHLO/AUTH）...");
        let report = svc.test_connection().await;

        let mut lines = vec![report.summary.clone()];
        if target == "imap" || target == "both" {
            lines.push(format!(
                "IMAP：{} - {}",
                if report.imap.ok { "通过" } else { "失败" },
                report.imap.message
            ));
            if let Some(detail) = &report.imap.detail {
                lines.push(format!("  {detail}"));
            }
        }
        if target == "smtp" || target == "both" {
            lines.push(format!(
                "SMTP：{} - {}",
                if report.smtp.ok { "通过" } else { "失败" },
                report.smtp.message
            ));
            if let Some(detail) = &report.smtp.detail {
                lines.push(format!("  {detail}"));
            }
        }
        lines.push(format!(
            "OAuth2：enabled={}，refresh_token={}，access_token={}，邮箱={}",
            report.oauth.enabled,
            if report.oauth.has_refresh_token { "已保存" } else { "缺失" },
            if report.oauth.access_token_valid { "有效" } else { "过期/为空（会自动刷新）" },
            if report.oauth.email.trim().is_empty() { "未填写" } else { report.oauth.email.as_str() }
        ));
        lines.push(format!("客户端 ID：{}", report.oauth.client_id_hint));
        if report.oauth.enabled {
            lines.push(format!(
                "OAuth token 诊断：audience={}；scopes={}；IMAP 权限={}；SMTP 权限={}；需要重新授权={}",
                if report.oauth.audience.is_empty() {
                    "未知（非 JWT）".to_string()
                } else {
                    report.oauth.audience.join(",")
                },
                if report.oauth.scopes.is_empty() {
                    "未知（非 JWT）".to_string()
                } else {
                    report.oauth.scopes.join(" ")
                },
                if report.oauth.has_imap_scope { "有" } else { "缺/未知" },
                if report.oauth.has_smtp_scope { "有" } else { "缺/未知" },
                if report.oauth.needs_reauth { "是" } else { "否" }
            ));
            if report.oauth.needs_reauth {
                lines.push(format!("状态：{}", report.oauth.status_message));
            }
        }
        lines.push(format!("当前配置：{}", svc.configuration_help()));

        let selected_ok = match target.as_str() {
            "imap" => report.imap.ok,
            "smtp" => report.smtp.ok,
            _ => report.all_ok(),
        };
        if selected_ok {
            lines.push("结论：连接正常，可以收/发邮件。".to_string());
            ToolOutcome::ok(lines.join("\n")).with_display("连接测试通过")
        } else {
            lines.push(
                "结论：有失败项。请按上面的原因修改设置；如果刚完成 OAuth 登录，先确认授权结果已写回设置（apply_oauth_result），再重测。"
                    .to_string(),
            );
            ToolOutcome::err(lines.join("\n")).with_display("连接测试失败")
        }
    }
}

// ============================================================
// mail_wait_for
// ============================================================

pub struct MailWaitForTool;

#[async_trait]
impl Tool for MailWaitForTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "mail_wait_for",
            "等待新邮件到达（轮询收件箱，等到匹配的邮件或超时）。这是“允许等待命令”的邮件版。\
             什么时候用：用户说“等一封 XX 发来的邮件”“等验证码邮件”“盯着邮箱，收到就告诉我”时。\
             参数：sender 发件人包含的字符串（可选，大小写不敏感，匹配邮箱或显示名）；\
             subject_contains 主题包含的字符串（可选，大小写不敏感）；\
             timeout_secs 最长等待秒数（可选，默认 120，最大 600）；folder 文件夹名（可选，默认 INBOX）。\
             期间会响应取消；命中后只返回摘要，正文用 mail_read(uid=...) 读取。",
            json!({
                "type": "object",
                "properties": {
                    "sender": {"type": "string", "description": "发件人包含的字符串，例如 \"github\" 或 \"boss@corp.com\""},
                    "subject_contains": {"type": "string", "description": "主题包含的字符串"},
                    "timeout_secs": {"type": "integer", "description": "最长等待秒数，默认 120，最大 600"},
                    "folder": {"type": "string", "description": "文件夹名，默认 INBOX"}
                }
            }),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let (settings, svc) = service(ctx);
        let folder = folder_from(&args);
        let sender = arg_str(&args, "sender").to_ascii_lowercase();
        let subject_contains = arg_str(&args, "subject_contains").to_ascii_lowercase();
        let timeout_secs = arg_u64(&args, "timeout_secs")
            .unwrap_or(DEFAULT_WAIT_SECS)
            .clamp(1, MAX_WAIT_SECS);
        let poll_secs = settings.mail.poll_interval_secs.clamp(3, 30).min(timeout_secs.max(3));

        if ctx.is_cancelled() {
            return ToolOutcome::err("已取消等待新邮件（用户中断）。");
        }

        ctx.phase(format!("正在检查 {folder} 当前未读邮件..."));
        let baseline_list = match svc.list(&folder, crate::MAX_LIST_LIMIT, true).await {
            Ok(list) => list,
            Err(e) => return ToolOutcome::err(format!("等待新邮件失败：无法读取收件箱：{e}")),
        };
        let baseline: HashSet<u32> = baseline_list.iter().map(|m| m.uid).collect();

        let started = Instant::now();
        let mut consecutive_errors = 0usize;
        loop {
            if ctx.is_cancelled() {
                return ToolOutcome::err("已取消等待新邮件（用户中断）。");
            }
            let elapsed = started.elapsed().as_secs();
            ctx.phase(format!(
                "等待新邮件：已等待 {elapsed} 秒 / 最多 {timeout_secs} 秒"
            ));

            match svc.list(&folder, crate::MAX_LIST_LIMIT, true).await {
                Ok(list) => {
                    consecutive_errors = 0;
                    if let Some(found) = list
                        .iter()
                        .rev()
                        .find(|m| !baseline.contains(&m.uid) && matches_wait_filter(m, &sender, &subject_contains))
                    {
                        return waiting_found_outcome(found, &settings);
                    }
                }
                Err(e) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= 3 {
                        return ToolOutcome::err(format!("等待新邮件中断：连续 3 次读取收件箱失败：{e}"));
                    }
                    tracing::warn!(error = %e, "mail_wait_for 轮询失败，稍后重试");
                }
            }

            let elapsed = started.elapsed();
            if elapsed >= Duration::from_secs(timeout_secs) {
                let filters = describe_filters(&sender, &subject_contains);
                return ToolOutcome::err(format!(
                    "等待新邮件超时：{timeout_secs} 秒内 {folder} 里没有收到{filters}的新邮件。\
                     可以延长 timeout_secs（最大 {} 秒）重试，或先用 mail_list 看看邮箱现状。",
                    MAX_WAIT_SECS
                ))
                .with_display("等待新邮件超时");
            }
            let remain = Duration::from_secs(timeout_secs).saturating_sub(elapsed);
            let sleep_for = Duration::from_secs(poll_secs).min(remain);
            ctx.emit_output(format!(
                "[等待中] 已等待 {} 秒，暂未收到符合条件的邮件\n",
                started.elapsed().as_secs()
            ));
            if !sleep_with_cancel(ctx, sleep_for).await {
                return ToolOutcome::err("已取消等待新邮件（用户中断）。");
            }
        }
    }
}

fn matches_wait_filter(m: &MailSummary, sender: &str, subject_contains: &str) -> bool {
    if !sender.is_empty() {
        let from = m.from.to_ascii_lowercase();
        let name = m.from_name.to_ascii_lowercase();
        if !from.contains(sender) && !name.contains(sender) {
            return false;
        }
    }
    if !subject_contains.is_empty() && !m.subject.to_ascii_lowercase().contains(subject_contains) {
        return false;
    }
    true
}

fn describe_filters(sender: &str, subject_contains: &str) -> String {
    let mut parts = Vec::new();
    if !sender.is_empty() {
        parts.push(format!("发件人包含“{sender}”"));
    }
    if !subject_contains.is_empty() {
        parts.push(format!("主题包含“{subject_contains}”"));
    }
    if parts.is_empty() { String::new() } else { parts.join("且") }
}

fn waiting_found_outcome(m: &MailSummary, settings: &Settings) -> ToolOutcome {
    let note_line = match whitelist_note(settings, &m.from) {
        Some(note) => format!("\n白名单备注：{}", truncate_chars(&note, 200)),
        None => String::new(),
    };
    ToolOutcome::ok(format!(
        "收到新邮件：\n{}{}\n\n用 mail_read(uid={}) 读取正文，用 mail_reply(uid={}, body=...) 回复。",
        m.one_line(),
        note_line,
        m.uid,
        m.uid
    ))
    .with_display(format!("收到新邮件：{}", truncate_chars(&m.subject, 50)))
}

/// 可取消的睡眠：每 200ms 检查一次取消标志。
async fn sleep_with_cancel(ctx: &ToolCtx, dur: Duration) -> bool {
    let deadline = Instant::now() + dur;
    loop {
        if ctx.is_cancelled() {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        let remain = deadline.saturating_duration_since(now);
        tokio::time::sleep(remain.min(Duration::from_millis(200))).await;
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use neosonia_core::config::{Paths, Settings};
    use neosonia_core::model::{
        AgentEvent, AgentHost, MemoryApi, MemoryItem, MemoryStats, SubagentRequest, ToolCtx,
    };
    use parking_lot::RwLock;

    struct StubMemory;

    impl MemoryApi for StubMemory {
        fn remember(&self, _: &str, _: &[String], _: f64, _: &str) -> anyhow::Result<i64> {
            Ok(1)
        }
        fn recall(&self, _: &str, _: usize) -> anyhow::Result<Vec<MemoryItem>> {
            Ok(Vec::new())
        }
        fn recent(&self, _: usize) -> anyhow::Result<Vec<MemoryItem>> {
            Ok(Vec::new())
        }
        fn all(&self, _: Option<&str>, _: usize) -> anyhow::Result<Vec<MemoryItem>> {
            Ok(Vec::new())
        }
        fn forget(&self, _: i64) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn update_importance(&self, _: i64, _: f64) -> anyhow::Result<()> {
            Ok(())
        }
        fn get_block(&self, _: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn set_block(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn blocks(&self) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
            Ok(Default::default())
        }
        fn log_conversation(&self, _: &str, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn stats(&self) -> anyhow::Result<MemoryStats> {
            Ok(MemoryStats::default())
        }
    }

    struct StubHost {
        settings: Arc<RwLock<Settings>>,
        cancelled: AtomicBool,
    }

    impl AgentHost for StubHost {
        fn settings(&self) -> Arc<RwLock<Settings>> {
            self.settings.clone()
        }
        fn paths(&self) -> Paths {
            Paths::from_root(std::env::temp_dir().join("neosonia-mail-test-root"))
        }
        fn memory(&self) -> Arc<dyn MemoryApi> {
            Arc::new(StubMemory)
        }
        fn emit(&self, _event: AgentEvent) {}
        fn spawn_subagent(&self, _req: SubagentRequest) -> anyhow::Result<String> {
            Ok("stub".into())
        }
        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }
    }

    fn ctx_with(settings: Settings, cancelled: bool) -> ToolCtx {
        let host = StubHost {
            settings: Arc::new(RwLock::new(settings)),
            cancelled: AtomicBool::new(cancelled),
        };
        ToolCtx::new(Arc::new(host), "turn", "call")
    }

    #[test]
    fn eight_tools_are_registered_with_exact_names() {
        let names = tool_names();
        assert_eq!(
            names,
            vec![
                "mail_folders",
                "mail_list",
                "mail_mark_read",
                "mail_read",
                "mail_reply",
                "mail_send",
                "mail_test_connection",
                "mail_wait_for",
            ]
        );
        let reg = ToolRegistry::new();
        register(&reg);
        assert_eq!(reg.len(), 8);
        for name in names {
            assert!(reg.get(name).is_some(), "missing tool {name}");
        }
    }

    #[test]
    fn specs_have_object_schema_and_required_fields() {
        for spec in mail_tool_specs() {
            assert_eq!(spec.parameters["type"], "object", "tool {}", spec.name);
            assert!(!spec.description.trim().is_empty(), "tool {}", spec.name);
        }
        let send = mail_tool_specs()
            .into_iter()
            .find(|s| s.name == "mail_send")
            .unwrap();
        let required = send.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "to"));
        assert!(required.iter().any(|v| v == "body"));
        let read = mail_tool_specs()
            .into_iter()
            .find(|s| s.name == "mail_read")
            .unwrap();
        assert!(read.parameters["properties"]["uid"].is_object());
    }

    #[test]
    fn folder_default_and_args_parse() {
        let empty = json!({});
        assert_eq!(folder_from(&empty), "INBOX");
        assert_eq!(folder_from(&json!({"folder":"Sent"})), "Sent");
        assert_eq!(
            arg_addresses(&json!({"to":"a@b.com, c@d.com"}), "to"),
            vec!["a@b.com", "c@d.com"]
        );
        assert_eq!(
            arg_addresses(&json!({"to":["a@b.com","b@c.com"]}), "to"),
            vec!["a@b.com", "b@c.com"]
        );
        assert_eq!(arg_u64(&json!({"uid": 12}), "uid"), Some(12));
        assert_eq!(arg_u64(&json!({"uid": "12"}), "uid"), Some(12));
        assert_eq!(arg_bool(&json!({}), "unread_only", false), false);
    }

    #[test]
    fn mail_send_attachments_parameter() {
        // schema 里有 attachments（array of string，非必填）
        let send = mail_tool_specs()
            .into_iter()
            .find(|s| s.name == "mail_send")
            .unwrap();
        assert!(send.parameters["properties"]["attachments"].is_object());
        let required = send.parameters["required"].as_array().unwrap();
        assert!(!required.iter().any(|v| v == "attachments"));
        assert!(send.description.contains("附件"));

        // 参数解析：数组 / 单个字符串 / 缺失
        let array = json!({"attachments": ["C:\\a\\1.pdf", "C:\\b\\2.png"]});
        assert_eq!(
            arg_attachment_paths(&array, "attachments"),
            vec!["C:\\a\\1.pdf", "C:\\b\\2.png"]
        );
        let single = json!({"attachments": "C:\\a\\带 空格 的文件.pdf"});
        assert_eq!(
            arg_attachment_paths(&single, "attachments"),
            vec!["C:\\a\\带 空格 的文件.pdf"]
        );
        assert!(arg_attachment_paths(&json!({}), "attachments").is_empty());
    }

    #[test]
    fn whitelist_note_lookup() {
        let mut settings = Settings::default();
        settings
            .mail
            .whitelist_notes
            .insert("Boss@Corp.com".into(), "老板（只回紧急邮件）".into());
        settings
            .mail
            .whitelist_notes
            .insert("@vip.com".into(), "VIP 客户".into());

        assert_eq!(
            whitelist_note(&settings, "boss@corp.com").as_deref(),
            Some("老板（只回紧急邮件）")
        );
        assert_eq!(
            whitelist_note(&settings, "someone@vip.com").as_deref(),
            Some("VIP 客户")
        );
        assert!(whitelist_note(&settings, "other@example.com").is_none());
    }

    #[tokio::test]
    async fn mail_send_blocks_non_whitelisted_recipient() {
        let mut settings = Settings::default();
        settings.mail.smtp.host = "smtp.example.com".into();
        settings.mail.smtp.user = "me@example.com".into();
        settings.mail.smtp.password = "secret".into();
        settings.mail.smtp.from = "me@example.com".into();
        let ctx = ctx_with(settings, false);
        let out = MailSendTool
            .call(&ctx, json!({"to":"stranger@example.com","subject":"hi","body":"hello"}))
            .await;
        assert!(!out.ok);
        assert!(out.content.contains("白名单"), "got: {}", out.content);
        assert!(out.content.contains("已拦截"));
    }

    #[tokio::test]
    async fn mail_send_accepts_whitelisted_recipient_but_fails_at_network_layer() {
        let mut settings = Settings::default();
        settings.mail.smtp.host = "127.0.0.1".into();
        settings.mail.smtp.port = 1; // 必然连接失败
        settings.mail.smtp.user = "me@example.com".into();
        settings.mail.smtp.password = "secret".into();
        settings.mail.smtp.from = "me@example.com".into();
        settings.mail.smtp.timeout_secs = 1;
        settings.mail.whitelist.push("boss@corp.com".into());
        let ctx = ctx_with(settings, false);
        let out = MailSendTool
            .call(&ctx, json!({"to":"boss@corp.com","subject":"hi","body":"hello"}))
            .await;
        // 白名单放行后会在 SMTP 阶段失败（不会 panic，不会真的发出邮件）
        assert!(!out.ok);
        assert!(out.content.contains("发送邮件失败"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn mail_folders_without_config_returns_hint() {
        let ctx = ctx_with(Settings::default(), false);
        let out = MailFoldersTool.call(&ctx, json!({})).await;
        assert!(!out.ok);
        assert!(out.content.contains("未配置邮件服务器"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn mail_wait_for_honors_cancel() {
        let ctx = ctx_with(Settings::default(), true);
        let out = MailWaitForTool.call(&ctx, json!({"timeout_secs": 5})).await;
        assert!(!out.ok);
        assert!(out.content.contains("取消"), "got: {}", out.content);
    }

    #[test]
    fn wait_filter_matches_sender_and_subject() {
        let m = MailSummary {
            uid: 1,
            from: "GitHub <noreply@github.com>".into(),
            from_name: "GitHub".into(),
            subject: "验证码 123456".into(),
            ..Default::default()
        };
        assert!(matches_wait_filter(&m, "github", ""));
        assert!(matches_wait_filter(&m, "noreply@github.com", ""));
        assert!(matches_wait_filter(&m, "", "验证码"));
        assert!(!matches_wait_filter(&m, "gitlab", ""));
        assert!(!matches_wait_filter(&m, "", "账单"));
    }

    #[test]
    fn blocked_addresses_respects_flag_and_suffix() {
        let mut settings = Settings::default();
        settings.mail.whitelist.push("@corp.com".into());
        settings.mail.whitelist.push("boss@x.com".into());
        assert!(blocked_addresses(&settings, &["a@corp.com".into(), "boss@x.com".into()]).is_empty());
        let blocked = blocked_addresses(&settings, &["evil@y.com".into()]);
        assert_eq!(blocked, vec!["evil@y.com"]);
        settings.mail.allow_outbound_to_anyone = true;
        assert!(blocked_addresses(&settings, &["evil@y.com".into()]).is_empty());
    }
}
