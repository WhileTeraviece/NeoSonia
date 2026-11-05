//! 共享数据模型：对话消息、工具调用、事件、记忆条目、子代理请求。
//!
//! 这些类型是 core / tools / mail / app 四个 crate 之间的公共契约。
//! 修改这里会破坏其它 crate，改之前先通知 Lead。

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Paths, Settings};

// ============================================================
// 消息 / 工具调用
// ============================================================

/// 对话角色，与 OpenAI 兼容 API 一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Default for Role {
    fn default() -> Self {
        Role::User
    }
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

fn default_function_type() -> String {
    "function".to_string()
}

/// 模型返回的一次函数调用。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_function_type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

impl FunctionCall {
    /// 解析参数 JSON；解析失败返回空对象。
    pub fn parse_args(&self) -> Value {
        let raw = self.arguments.trim();
        if raw.is_empty() {
            return json!({});
        }
        serde_json::from_str(raw).unwrap_or_else(|_| json!({}))
    }

    pub fn parse_args_strict(&self) -> anyhow::Result<Value> {
        let raw = self.arguments.trim();
        if raw.is_empty() {
            return Ok(json!({}));
        }
        Ok(serde_json::from_str(raw)?)
    }
}

/// 一条对话消息。
///
/// `reasoning_content` 是 DeepSeek 思考模式的字段：带 tool_calls 的 assistant
/// 消息如果丢掉它会 400，所以 `to_wire` 会把它带回去。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
    /// 图片输入（视觉）。只有 user / assistant 消息用得到；序列化时展开成
    /// OpenAI 的 content 数组：`[{type:text},{type:image_url,...}]`。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImagePart>,
    #[serde(default)]
    pub created_at_ms: i64,
    /// 仅 UI 用：工具消息是成功还是失败。
    #[serde(default)]
    pub tool_ok: Option<bool>,
}

/// 一条图片输入（base64 内联，不引用本地文件，便于跨进程/跨设备传输）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImagePart {
    /// 例如 `image/png`
    pub mime: String,
    /// 纯 base64（不带 `data:` 前缀）
    pub data_base64: String,
    pub width: u32,
    pub height: u32,
    /// 可选：来源路径（只是给人看的标签，不参与请求）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl ImagePart {
    pub fn png(base64: String, width: u32, height: u32, source: Option<String>) -> Self {
        Self {
            mime: "image/png".into(),
            data_base64: base64,
            width,
            height,
            source,
        }
    }

    /// `data:` URI（OpenAI image_url 格式用）。
    pub fn data_uri(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.data_base64)
    }

    /// 解码后的字节数估算（base64 → 原始）。
    pub fn approx_bytes(&self) -> usize {
        self.data_base64.len() * 3 / 4
    }
}

impl Default for ChatMessage {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            role: Role::User,
            content: None,
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            images: Vec::new(),
            created_at_ms: crate::util::now_ms(),
            tool_ok: None,
        }
    }
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> Self {
        let mut m = Self::default();
        m.role = Role::System;
        m.content = Some(text.into());
        m
    }

    pub fn user(text: impl Into<String>) -> Self {
        let mut m = Self::default();
        m.role = Role::User;
        m.content = Some(text.into());
        m
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        let mut m = Self::default();
        m.role = Role::Assistant;
        m.content = Some(text.into());
        m
    }

    pub fn assistant_with_calls(
        text: Option<String>,
        reasoning: Option<String>,
        calls: Vec<ToolCall>,
    ) -> Self {
        let mut m = Self::default();
        m.role = Role::Assistant;
        m.content = text.filter(|s| !s.is_empty());
        m.reasoning_content = reasoning.filter(|s| !s.is_empty());
        m.tool_calls = calls;
        m
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
        ok: bool,
    ) -> Self {
        let mut m = Self::default();
        m.role = Role::Tool;
        m.tool_call_id = Some(call_id.into());
        m.name = Some(name.into());
        m.content = Some(content.into());
        m.tool_ok = Some(ok);
        m
    }

    /// 用户消息 + 图片（视觉输入）。
    pub fn user_with_images(text: impl Into<String>, images: Vec<ImagePart>) -> Self {
        let mut m = Self::user(text);
        m.images = images;
        m
    }

    /// 追加图片（链式）。
    pub fn with_image(mut self, image: ImagePart) -> Self {
        self.images.push(image);
        self
    }

    /// 这条消息带不带图片。
    pub fn has_images(&self) -> bool {
        !self.images.is_empty()
    }

    /// 转成 API 需要的 wire 形状（不含内部 id / 时间戳）。
    ///
    /// 带图片的消息会把 `content` 展开成 OpenAI 的内容数组：
    /// `[{type:"text",text:...}, {type:"image_url",image_url:{url:"data:..."}}]`。
    pub fn to_wire(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("role".into(), json!(self.role.as_str()));
        let has_images =
            !self.images.is_empty() && matches!(self.role, Role::User | Role::Assistant);
        match self.role {
            Role::Assistant => {
                if has_images {
                    obj.insert("content".into(), self.content_parts_wire());
                } else {
                    obj.insert(
                        "content".into(),
                        json!(self.content.clone().unwrap_or_default()),
                    );
                }
                if let Some(r) = &self.reasoning_content {
                    if !r.is_empty() {
                        obj.insert("reasoning_content".into(), json!(r));
                    }
                }
                if !self.tool_calls.is_empty() {
                    obj.insert(
                        "tool_calls".into(),
                        json!(self
                            .tool_calls
                            .iter()
                            .map(|c| json!({
                                "id": c.id,
                                "type": "function",
                                "function": { "name": c.function.name, "arguments": c.function.arguments }
                            }))
                            .collect::<Vec<_>>()),
                    );
                }
            }
            Role::Tool => {
                obj.insert(
                    "content".into(),
                    json!(self.content.clone().unwrap_or_default()),
                );
                obj.insert(
                    "tool_call_id".into(),
                    json!(self.tool_call_id.clone().unwrap_or_default()),
                );
            }
            _ => {
                if has_images {
                    obj.insert("content".into(), self.content_parts_wire());
                } else {
                    obj.insert(
                        "content".into(),
                        json!(self.content.clone().unwrap_or_default()),
                    );
                }
            }
        }
        Value::Object(obj)
    }

    /// OpenAI 风格的多段 content（文本 + 图片）。
    fn content_parts_wire(&self) -> Value {
        let mut parts: Vec<Value> = Vec::new();
        let text = self.content.clone().unwrap_or_default();
        if !text.is_empty() {
            parts.push(json!({ "type": "text", "text": text }));
        }
        for img in &self.images {
            parts.push(json!({
                "type": "image_url",
                "image_url": { "url": img.data_uri() }
            }));
        }
        Value::Array(parts)
    }

    pub fn text(&self) -> String {
        self.content.clone().unwrap_or_default()
    }
}

// ============================================================
// 工具规格 / 结果
// ============================================================

/// 暴露给模型的工具说明（JSON Schema 形式）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSpec {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    pub fn to_wire(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

/// 工具执行结果。`content` 会完整喂给模型，`display` 给 UI 用。
#[derive(Clone, Debug)]
pub struct ToolOutcome {
    pub ok: bool,
    pub content: String,
    /// UI 上展示的一行摘要（可为空，UI 会退化成 content 首行）。
    pub display: Option<String>,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            ok: true,
            content: content.into(),
            display: None,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            ok: false,
            content: content.into(),
            display: None,
        }
    }

    pub fn with_display(mut self, display: impl Into<String>) -> Self {
        self.display = Some(display.into());
        self
    }
}

// ============================================================
// Agent -> UI 事件
// ============================================================

/// 代理循环向前端推送的事件。UI 只依赖这个枚举 + `ChatMessage`。
///
/// 同时实现 Serialize/Deserialize（相邻标记：`#\[serde(tag = "type", content = "data")\]`），
/// 这样局域网 Web 端可以把事件直接 JSON 化后通过 WebSocket 推给远端设备。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStarted {
        turn_id: String,
        user_text: String,
    },
    /// 当前在做什么（"思考中" / "调用工具" / "等待命令输出"）。
    Phase(String),
    ReasoningDelta(String),
    TextDelta(String),
    /// 一条 assistant 消息已经完整落库（含思考内容）。
    AssistantMessage {
        text: String,
        reasoning: Option<String>,
    },
    ToolStarted {
        call_id: String,
        name: String,
        args: String,
        /// 人类可读的参数预览（例如命令内容），由工具自己给。
        preview: Option<String>,
    },
    /// 长任务的流式输出（命令 stdout、下载进度）。
    ToolOutput {
        call_id: String,
        chunk: String,
    },
    ToolFinished {
        call_id: String,
        name: String,
        ok: bool,
        output: String,
        summary: Option<String>,
        elapsed_ms: u64,
    },
    SubagentStarted {
        id: String,
        label: String,
        task: String,
    },
    SubagentActivity {
        id: String,
        line: String,
    },
    SubagentFinished {
        id: String,
        label: String,
        ok: bool,
        summary: String,
        elapsed_ms: u64,
    },
    MemoryChanged {
        action: String,
        content: String,
    },
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
        cached_tokens: u32,
    },
    /// 需要用户在 UI 上确认的危险操作（风险审查）。
    ///
    /// 旧字段 `call_id/name/args/reason` 保留兼容；风险相关字段带 serde default，
    /// 旧的 `approve(call_id, bool)` 路径（approval_mode=ask）下风险字段为 0/空。
    ApprovalRequested {
        call_id: String,
        name: String,
        args: String,
        reason: String,
        /// 风险等级 1..=4（0 = 非风险审查产生的旧审批）。
        #[serde(default)]
        risk_level: u8,
        /// 风险原因（中文）。
        #[serde(default)]
        risk_reason: String,
        /// 同类操作类别（用于授权/拒绝记忆）。
        #[serde(default)]
        class: String,
        /// 高风险时 UI 强提示。
        #[serde(default)]
        emphasized: bool,
    },
    Error(String),
    Notice(String),
    TurnFinished {
        turn_id: String,
        cancelled: bool,
    },
    /// 新会话建立（对话历史首次落库时）。
    ConversationStarted {
        conversation_id: String,
        title: String,
    },
    /// 本轮把截图/图片喂给了模型（vision）。
    ImagesAttached {
        count: usize,
        source: String,
    },
    /// 模式切换成功（daily / coding / computer_use），app 更新当前模式显示。
    ModeChanged {
        /// 模式 id：daily / coding / computer_use。
        mode: String,
        /// 中文名：日常 / 编程 / 计算机使用。
        label: String,
    },
    /// 一次风险评分结果（1 分自动执行时也会发，UI 记日志用）。
    RiskReviewed {
        tool: String,
        level: u8,
        class: String,
        reason: String,
    },
    /// 当前轮次已被用户取消/停止（UI 显示"已停止"）。
    Cancelled {
        turn_id: String,
    },
    /// 上下文压缩完成：早前历史被折叠成摘要，最近 N 条保留原文。
    ContextCompressed {
        /// 压缩前的历史估算 token。
        before_tokens: u64,
        /// 压缩后实际发给模型的历史估算 token。
        after_tokens: u64,
        /// 保留原文的最近消息条数。
        kept: usize,
    },
    /// 模型交付产物（图片/视频/文件/链接），UI 负责渲染与打开。
    Delivered {
        files: Vec<DeliverFile>,
        links: Vec<String>,
        caption: String,
    },
    /// 会话计划更新（`update_plan` 工具写入）。
    PlanUpdated {
        steps: Vec<PlanStep>,
    },
    /// 请求 UI 隐藏/恢复主窗口（后台任务用）。
    WindowVisibilityRequested {
        hidden: bool,
        reason: String,
    },
    /// 子代理数量变化（顶栏显示"本会话子代理数量"）。
    SubagentCount {
        /// 当前运行中的子代理数。
        active: u32,
        /// 本会话启动过的子代理总数。
        total: u32,
    },
    /// 模型提供商状态页结果变化（顶栏/设置页小球）。
    ProviderStatusChanged {
        /// 厂商名，例如 "DeepSeek"。
        provider: String,
        /// `green` / `yellow` / `orange` / `red` / `unknown`。
        indicator: String,
        /// 中文状态："正常" / "轻微故障" / "部分中断" / "严重中断" / "未知"。
        label: String,
        /// 人看的 status 页面。
        url: String,
        checked_at_ms: i64,
    },
}

/// 一次交付的文件。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliverFile {
    pub path: String,
    pub name: String,
    /// `image` / `video` / `file`。
    pub kind: String,
    pub size: u64,
}

/// 计划中的一步。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub title: String,
    /// `pending` / `in_progress` / `done`。
    pub status: String,
}

// ============================================================
// 记忆
// ============================================================

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: i64,
    /// fact / preference / event / task / block
    pub kind: String,
    pub content: String,
    pub tags: Vec<String>,
    pub importance: f64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub hits: i64,
    pub source: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MemoryStats {
    pub facts: i64,
    pub conversations: i64,
    pub blocks: i64,
    /// 数据库字节数
    pub db_bytes: u64,
}

/// 长期记忆接口。`neosonia-core::memory::MemoryStore` 是唯一实现。
pub trait MemoryApi: Send + Sync {
    fn remember(
        &self,
        content: &str,
        tags: &[String],
        importance: f64,
        source: &str,
    ) -> anyhow::Result<i64>;
    fn recall(&self, query: &str, limit: usize) -> anyhow::Result<Vec<MemoryItem>>;
    fn recent(&self, limit: usize) -> anyhow::Result<Vec<MemoryItem>>;
    fn all(&self, kind: Option<&str>, limit: usize) -> anyhow::Result<Vec<MemoryItem>>;
    fn forget(&self, id: i64) -> anyhow::Result<bool>;
    fn update_importance(&self, id: i64, importance: f64) -> anyhow::Result<()>;

    /// 人设/用户画像这类固定块（旧 Sonia 的 persona / human 块）。
    fn get_block(&self, key: &str) -> anyhow::Result<Option<String>>;
    fn set_block(&self, key: &str, value: &str) -> anyhow::Result<()>;
    fn blocks(&self) -> anyhow::Result<BTreeMap<String, String>>;

    /// 追加一条对话记录（与 UI 消息分开存，供压缩/检索）。
    fn log_conversation(&self, role: &str, content: &str, turn_id: &str) -> anyhow::Result<()>;
    fn stats(&self) -> anyhow::Result<MemoryStats>;
}

// ============================================================
// 子代理
// ============================================================

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentRequest {
    /// 短名字，例如 "research-mail"。
    pub label: String,
    /// 交给子代理的完整任务描述。
    pub task: String,
    /// 使用哪个人设（None = 继承父代理）。
    pub persona: Option<String>,
    /// 只允许这些工具；空 = 继承父代理全部工具。
    pub tools: Vec<String>,
    /// 子代理最多跑几轮工具调用。
    pub max_iterations: u32,
    /// 后台运行：立即返回 id，结束后再推事件。
    pub background: bool,
}

impl Default for SubagentRequest {
    fn default() -> Self {
        Self {
            label: "subagent".into(),
            task: String::new(),
            persona: None,
            tools: Vec::new(),
            max_iterations: 12,
            background: false,
        }
    }
}

// ============================================================
// 工具运行上下文 + 宿主
// ============================================================

/// 工具访问宿主能力的唯一入口（配置、路径、记忆、子代理、事件）。
pub trait AgentHost: Send + Sync {
    fn settings(&self) -> Arc<RwLock<Settings>>;
    fn paths(&self) -> Paths;
    fn memory(&self) -> Arc<dyn MemoryApi>;

    /// 抓当前设置的快照（工具里读配置用，避免长时间持锁）。
    fn settings_snapshot(&self) -> Settings {
        self.settings().read().clone()
    }

    /// 同步推送事件到当前轮次。工具可以在长任务里持续调用。
    fn emit(&self, event: AgentEvent);

    /// 启动一个子代理；返回子代理 id。
    fn spawn_subagent(&self, req: SubagentRequest) -> anyhow::Result<String>;

    /// 当前轮次是否已被用户取消（长任务里要轮询）。
    fn is_cancelled(&self) -> bool;

    /// 登记一个可被 UI 管理的外部任务 id（可选）。
    fn note_background_task(&self, _id: &str, _desc: &str) {}
}

/// 每个工具调用都会拿到的上下文。
#[derive(Clone)]
pub struct ToolCtx {
    pub host: Arc<dyn AgentHost>,
    pub turn_id: String,
    pub tool_call_id: String,
}

impl ToolCtx {
    pub fn new(
        host: Arc<dyn AgentHost>,
        turn_id: impl Into<String>,
        tool_call_id: impl Into<String>,
    ) -> Self {
        Self {
            host,
            turn_id: turn_id.into(),
            tool_call_id: tool_call_id.into(),
        }
    }

    pub fn emit(&self, event: AgentEvent) {
        self.host.emit(event);
    }

    /// 往 UI 推一条长任务输出。
    pub fn emit_output(&self, chunk: impl Into<String>) {
        self.host.emit(AgentEvent::ToolOutput {
            call_id: self.tool_call_id.clone(),
            chunk: chunk.into(),
        });
    }

    pub fn phase(&self, phase: impl Into<String>) {
        self.host.emit(AgentEvent::Phase(phase.into()));
    }

    pub fn settings(&self) -> Settings {
        self.host.settings_snapshot()
    }

    pub fn paths(&self) -> Paths {
        self.host.paths()
    }

    pub fn memory(&self) -> Arc<dyn MemoryApi> {
        self.host.memory()
    }

    pub fn is_cancelled(&self) -> bool {
        self.host.is_cancelled()
    }

    /// 工作目录：配置里的 workspace，若不存在则退回 paths.workspace。
    pub fn workspace(&self) -> std::path::PathBuf {
        let s = self.settings();
        let p = std::path::PathBuf::from(&s.agent.workspace_dir);
        if p.is_absolute() {
            p
        } else {
            self.paths().workspace.join(p)
        }
    }
}

// ============================================================
// 人设
// ============================================================

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Persona {
    pub id: String,
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    #[serde(default)]
    pub initial_memory: BTreeMap<String, String>,
    /// 人设文件路径（内置人设为空）。
    #[serde(default)]
    pub source_path: Option<String>,
}

impl Persona {
    pub fn builtin_default() -> Self {
        Self {
            id: "default".into(),
            name: "NeoSonia".into(),
            description: "常驻桌面的 AI Agent 助手".into(),
            system_prompt: String::new(),
            initial_memory: BTreeMap::new(),
            source_path: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape_of_tool_call_turn() {
        let msg = ChatMessage::assistant_with_calls(
            Some("calling".into()),
            Some("think".into()),
            vec![ToolCall {
                id: "c1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: "{\"path\":\"a\"}".into(),
                },
            }],
        );
        let wire = msg.to_wire();
        assert_eq!(wire["role"], "assistant");
        assert_eq!(wire["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(wire["reasoning_content"], "think");
    }

    #[test]
    fn parse_args_tolerates_garbage() {
        let fc = FunctionCall {
            name: "x".into(),
            arguments: "not json".into(),
        };
        assert!(fc.parse_args().is_object());
        assert!(fc.parse_args_strict().is_err());
    }
}
