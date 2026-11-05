//! 第二阶段新增的公共类型：对话历史、技能、远程设备。
//!
//! 这些类型同时被 `neosonia-core` 的存储实现、`neosonia-web` 的 HTTP API、
//! `neosonia-app` 的界面使用，所以放在 model 里作为契约。
//! 修改这里的字段需要同步 core / web / app 三处。

use serde::{Deserialize, Serialize};

// ============================================================
// 对话历史
// ============================================================

/// 一条会话（一个"对话"）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub message_count: u32,
    /// 最后一条消息的前 80 字，列表里做预览。
    pub preview: String,
    /// 会话里出现过的人设 id（可为空）。
    #[serde(default)]
    pub persona: String,
}

/// 历史搜索命中的一条消息。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationHit {
    pub conversation_id: String,
    pub conversation_title: String,
    /// user / assistant / tool
    pub role: String,
    /// 命中处前后各 40 字的片段。
    pub snippet: String,
    pub at_ms: i64,
}

// ============================================================
// 技能（Skill）
// ============================================================

/// 技能摘要（系统提示里只放这些，正文按需加载）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    /// 什么时候该用这个技能（给模型的选择依据）。
    pub when_to_use: String,
    /// 来源：builtin / local / url
    pub source: String,
    pub installed_at_ms: i64,
    /// SKILL.md 的绝对路径。
    pub path: String,
    /// 适用模式：daily / coding / computer_use；空 = 视为 daily（兼容旧技能）。
    #[serde(default)]
    pub modes: Vec<String>,
}

/// 技能全文（`skill_load` 返回给模型）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillDetail {
    pub summary: SkillSummary,
    /// 技能正文（markdown，可能很长）。
    pub body: String,
    /// 技能目录里附带的文件名列表（脚本/模板等）。
    pub files: Vec<String>,
}

// ============================================================
// 远程设备
// ============================================================

/// 已信任的局域网设备。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub id: String,
    /// 设备名（用户配对时填，例如 "iPhone 15" / "客厅 iPad"）。
    pub name: String,
    /// 设备上报的 UA 摘要。
    #[serde(default)]
    pub user_agent: String,
    /// 最近一次连接的 IP。
    #[serde(default)]
    pub last_addr: String,
    pub created_at_ms: i64,
    pub last_seen_ms: i64,
    /// 是否是当前提供配对码的这台设备。
    #[serde(default)]
    pub online: bool,
}

impl TrustedDevice {
    pub fn display(&self) -> String {
        if self.name.trim().is_empty() {
            self.id.clone()
        } else {
            self.name.clone()
        }
    }
}

// ============================================================
// 附件（聊天里上传的文件 / 文件夹 / 截图）
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentKind {
    Image,
    Text,
    File,
    Folder,
    Screenshot,
}

/// 待发送的附件（UI 状态，不发给模型本身）。
#[derive(Clone, Debug)]
pub struct Attachment {
    pub path: std::path::PathBuf,
    pub name: String,
    pub kind: AttachmentKind,
    pub size: u64,
    /// 文本类附件的内容预览（前 2000 字）。
    pub text_preview: Option<String>,
    /// 图片类附件解码后的像素（宽, 高, RGBA）。
    pub pixels: Option<(u32, u32, Vec<u8>)>,
}

impl Attachment {
    pub fn human_size(&self) -> String {
        crate::util::human_bytes(self.size)
    }
}
