//! 应用状态与消息类型。
//!
//! `Msg` 是 fenestra 的 UI 消息（必须 `Clone + Send + 'static`）。
//! 面板（设置/记忆/邮件/人设）只通过这里的 `Msg` 变体与主循环通信。

use std::collections::BTreeMap;
use std::sync::Arc;

use fenestra::prelude::Proxy;

/// UI 消息代理（bridge / 托盘 / 热键线程用它把消息投回 UI）。
pub type ProxyMsg = Proxy<Msg>;
use neosonia_core::config::{Paths, Settings, ThemeMode};
use neosonia_core::model::{AgentEvent, MemoryItem, Persona};
use serde_json::Value;

// ============================================================
// 导航
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavPage {
    Chat,
    Mail,
    Memory,
    Persona,
    Settings,
    /// 对话历史（标题栏入口，不在底栏）。
    History,
    /// 局域网设备（设置里入口，不在底栏）。
    Remote,
}

impl NavPage {
    pub fn label(self) -> &'static str {
        match self {
            NavPage::Chat => "对话",
            NavPage::Mail => "邮件",
            NavPage::Memory => "记忆",
            NavPage::Persona => "人设",
            NavPage::Settings => "设置",
            NavPage::History => "历史",
            NavPage::Remote => "设备",
        }
    }

    pub fn all() -> [NavPage; 5] {
        [NavPage::Chat, NavPage::Mail, NavPage::Memory, NavPage::Persona, NavPage::Settings]
    }

    /// 是否是底栏里的页面（History/Remote 通过标题栏/设置入口进入）。
    pub fn is_main(self) -> bool {
        !matches!(self, NavPage::History | NavPage::Remote)
    }
}

// ============================================================
// 聊天条目
// ============================================================

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Failed,
    Cancelled,
}

impl ToolState {
    pub fn label(&self) -> &'static str {
        match self {
            ToolState::Running => "运行中",
            ToolState::Ok => "完成",
            ToolState::Failed => "失败",
            ToolState::Cancelled => "已取消",
        }
    }
}

/// 一次工具调用的卡片。
#[derive(Clone, Debug)]
pub struct ToolCard {
    pub call_id: String,
    pub name: String,
    pub args: String,
    pub preview: Option<String>,
    pub state: ToolState,
    pub output: String,
    pub elapsed_ms: u64,
    pub expanded: bool,
    /// 工具产生的截图缩略图（宽, 高, RGBA），用于卡片里显示"看到了什么"。
    pub thumb: Option<(u32, u32, Vec<u8>)>,
}

#[derive(Clone, Debug)]
pub struct SubagentCard {
    pub id: String,
    pub label: String,
    pub task: String,
    pub state: ToolState,
    pub activity: Vec<String>,
    pub summary: String,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug)]
pub enum ChatItem {
    User {
        text: String,
        /// 用户上传/截图附带的图片缩略图（宽, 高, RGBA），最多几张。
        thumbs: Vec<(u32, u32, Vec<u8>)>,
    },
    Assistant {
        text: String,
        reasoning: String,
        reasoning_open: bool,
        streaming: bool,
        error: Option<String>,
    },
    Tool(ToolCard),
    Subagent(SubagentCard),
    Notice {
        text: String,
        level: NoticeLevel,
    },
    /// 模型交付的文件/链接（图片/视频预览、文件快捷方式、可点开的链接）。
    Delivered {
        files: Vec<DeliverItem>,
        links: Vec<String>,
        caption: String,
    },
}

/// 一条交付物（chat 里渲染成预览卡/快捷方式）。
#[derive(Clone, Debug, PartialEq)]
pub struct DeliverItem {
    pub path: String,
    pub name: String,
    /// image / video / file
    pub kind: String,
    pub size: u64,
}

/// 会话计划的一步（update_plan 写入）。
#[derive(Clone, Debug, PartialEq)]
pub struct PlanStepView {
    pub title: String,
    /// pending / in_progress / done
    pub status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Success,
    Warning,
    Error,
}

impl ChatItem {
    pub fn user(text: impl Into<String>) -> Self {
        ChatItem::User { text: text.into(), thumbs: Vec::new() }
    }

    pub fn assistant() -> Self {
        ChatItem::Assistant {
            text: String::new(),
            reasoning: String::new(),
            reasoning_open: false,
            streaming: true,
            error: None,
        }
    }
}

// ============================================================
// 邮件
// ============================================================

#[derive(Clone, Debug, Default)]
pub struct MailState {
    pub loaded: bool,
    pub loading: bool,
    pub summaries: Vec<neosonia_mail::MailSummary>,
    pub open_uid: Option<u32>,
    pub open: Option<neosonia_mail::MailFull>,
    pub reply_text: String,
    pub status: Option<String>,
    pub unread: u32,
    /// 长文是否展开。
    pub body_expanded: bool,
}

// ============================================================
// 设置草稿
// ============================================================

/// 设置面板改值的值类型。
#[derive(Clone, Debug)]
pub enum SettingValue {
    Str(String),
    Bool(bool),
    Num(f64),
    StrList(Vec<String>),
}

impl SettingValue {
    pub fn as_str(&self) -> String {
        match self {
            SettingValue::Str(s) => s.clone(),
            SettingValue::Bool(b) => b.to_string(),
            SettingValue::Num(n) => n.to_string(),
            SettingValue::StrList(v) => v.join(", "),
        }
    }
}

/// 连通性测试目标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestTarget {
    Llm,
    Imap,
    Smtp,
}

impl TestTarget {
    pub fn label(self) -> &'static str {
        match self {
            TestTarget::Llm => "模型服务",
            TestTarget::Imap => "IMAP 收信",
            TestTarget::Smtp => "SMTP 发信",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TestResult {
    pub target: Option<TestTarget>,
    pub running: bool,
    pub ok: bool,
    pub message: String,
    /// 一句话总结（IMAP/SMTP 各自结果都返回时用）。
    pub summary: String,
    pub imap_ok: bool,
    pub imap_message: String,
    pub smtp_ok: bool,
    pub smtp_message: String,
}

impl TestResult {
    /// 其它测试项（LLM/索引等）只填通用字段时用这个补全。
    pub fn bare(target: Option<TestTarget>, running: bool, ok: bool, message: String) -> Self {
        Self {
            target,
            running,
            ok,
            message,
            summary: String::new(),
            imap_ok: false,
            imap_message: String::new(),
            smtp_ok: false,
            smtp_message: String::new(),
        }
    }
}

// ============================================================
// Msg
// ============================================================

#[derive(Clone)]
pub enum Msg {
    // ---- 窗口 / 生命周期 ----
    /// 全局快捷键轮询（Sub::every 推动）
    Tick,
    ShowWindow,
    HideWindow,
    ToggleWindow,
    DragWindow,
    Quit,
    SetTheme(ThemeMode),

    // ---- 导航 ----
    Nav(NavPage),

    // ---- 聊天 ----
    ComposerChanged(String),
    Submit,
    CancelTurn,
    ClearChat,
    ToggleReasoning(usize),
    ToggleTool(String),
    AgentEvent(Arc<AgentEvent>),
    Approve { call_id: String, ok: bool },

    // ---- 设置 ----
    SettingEdit(SettingValuePath),
    SettingsSave,
    SettingsReload,
    TestConnection(TestTarget),
    TestFinished(TestResult),
    WhitelistAdd(String),
    WhitelistRemove(String),
    /// 白名单输入框草稿（面板持有缓冲，点添加时发 WhitelistAdd）。
    WhitelistInputChanged(String),
    HotkeyApply,

    // ---- 人设 ----
    PersonaSelect(String),
    PersonaReload,
    PersonaSaved(String),

    // ---- 记忆 ----
    MemoryQueryChanged(String),
    MemorySearch,
    MemoryLoaded(Vec<MemoryItem>),
    MemoryDelete(i64),
    /// 固定块编辑缓冲（只写 AppState.memory_blocks，不落库）。
    MemoryBlockChanged { key: String, value: String },
    MemoryBlockSave { key: String, value: String },

    // ---- 邮件 ----
    MailRefresh,
    MailLoaded(Vec<neosonia_mail::MailSummary>),
    MailOpen(u32),
    MailBody(Box<neosonia_mail::MailFull>),
    MailReplyChanged(String),
    MailSendReply,
    MailStatus(String),
    /// 从详情返回收件箱。
    MailClose,
    /// 长文展开/收起。
    MailToggleBody,

    // ---- 附件 ----
    AttachClick,
    AttachFolderClick,
    AttachmentsPicked(Vec<std::path::PathBuf>),
    AttachmentRemove(usize),
    AttachmentClear,

    // ---- 截图 ----
    /// 全局"截图发模型"快捷键触发。
    ScreenshotHotkey,
    /// 截图完成（Ok=文件路径；Err=失败原因）。
    ScreenshotDone(Result<String, String>),

    // ---- 对话历史 ----
    HistoryToggle,
    HistoryQueryChanged(String),
    HistorySearch,
    HistoryLoaded(Vec<neosonia_core::ConversationSummary>),
    HistoryHits(Vec<neosonia_core::ConversationHit>),
    HistorySelect(String),
    HistoryDelete(String),
    HistoryNew,

    // ---- 局域网设备 ----
    RemoteToggle,
    RemoteRefresh,
    RemotePairingReady {
        url: String,
        code: String,
        expires_at_ms: i64,
        qr_png: Option<String>,
    },
    RemoteDevices(Vec<neosonia_core::TrustedDevice>),
    RemoteRevoke(String),
    RemoteStatus(String),

    // ---- 技能 ----
    SkillsLoaded(Vec<neosonia_core::SkillSummary>),
    SkillInstallRequest,
    SkillInstallPath(String),
    SkillInstallUrl(String),
    SkillUninstall(String),
    SkillOpenDir,
    /// 技能 URL 输入缓冲。
    SkillInstallInputChanged(String),

    // ---- 本地文件索引 ----
    /// 索引根目录输入缓冲（多行，一行一个）。
    IndexRootsChanged(String),
    /// 按当前配置重建索引。
    IndexRebuild,
    /// 重启某台 MCP 服务器（按 name）。
    McpRestart(String),
    /// 重新加载 MCP 配置（保存设置后调用）。
    McpReload,
    /// 展开/折叠设置页的一个分组（id：skills/index/mail/remote/persona…）。
    ToggleSection(String),
    /// 切换工作模式（daily / coding / computer_use）。
    SetAgentMode(String),
    /// 开始邮箱 OAuth2 授权（"microsoft" / "google"）。
    MailOAuthStart(String),
    /// 审查弹窗里的决定：Approve / Deny / DenyClass。
    RiskDecision { call_id: String, decision: RiskChoice },
    /// 退出邮箱登录（清除 OAuth 凭证，保留服务器/账号配置）。
    MailLogout,

    /// 粘贴剪贴板内容：图片→作为附件；文件列表→附件；文本→填入输入框。
    PasteClipboard,
    /// 开始重命名设备（参数：设备 id）。
    DeviceRenameStart(String),
    /// 重命名输入变化。
    DeviceRenameChanged(String),
    /// 保存设备重命名。
    DeviceRename { id: String, name: String },
    /// 取消设备重命名。
    DeviceRenameCancel,
    /// 切页过渡动画的补帧消息（短期高频，播完即停）。
    AnimTick,
    /// Agent 请求隐藏/恢复主窗口（计算机使用/后台任务）。
    AgentWindowVisibility(bool),
    /// 应用挂起的页面切换（两段式加载的第二帧）。
    ApplyPendingPage,    /// 复制任意文本到剪贴板（OAuth 授权码、配对码等）。
    CopyText(String),
    /// 打开人设编辑器（None = 新建）。
    PersonaEditStart(Option<String>),
    /// 人设编辑字段变更（field: id/name/description/body）。
    PersonaEditField { field: String, value: String },
    /// 保存人设（新建或覆盖）。
    PersonaSaveEdit,
    /// 取消编辑。
    PersonaCancelEdit,
    /// 删除人设（需要确认）。
    PersonaDeleteRequest(String),
    /// 退出应用（托盘菜单）。
    QuitApp,
    /// 什么都不做（弹窗遮罩点击、占位用）。
    Noop,
    /// 邮箱 OAuth2 授权完成（Ok(邮箱) / Err(错误)）。
    MailOAuthDone(Result<String, String>),
    /// 本地索引运行态（条数、是否在扫描）。
    IndexStats(u64, bool),

    /// 跳到某个页面（含不在底栏的 History/Remote）。
    OpenPage(NavPage),

    // ---- 杂项 ----
    Copy(String),
    OpenLink(String),
    /// 用系统默认程序打开文件。
    OpenPath(String),
    /// 在资源管理器里定位文件。
    RevealPath(String),
    Toast { text: String, level: NoticeLevel },
    PersistError(String),
}

/// 设置路径 + 新值。
#[derive(Clone, Debug)]
pub struct SettingValuePath {
    /// JSON Pointer，例如 `/server/base_url`、`/ui/theme`。
    pub path: String,
    pub value: SettingValue,
}

impl Msg {
    pub fn set(path: &str, value: impl Into<SettingValue>) -> Msg {
        Msg::SettingEdit(SettingValuePath { path: path.to_string(), value: value.into() })
    }
}

impl From<String> for SettingValue {
    fn from(v: String) -> Self {
        SettingValue::Str(v)
    }
}

impl From<&str> for SettingValue {
    fn from(v: &str) -> Self {
        SettingValue::Str(v.to_string())
    }
}

impl From<bool> for SettingValue {
    fn from(v: bool) -> Self {
        SettingValue::Bool(v)
    }
}

impl From<f64> for SettingValue {
    fn from(v: f64) -> Self {
        SettingValue::Num(v)
    }
}

impl From<u64> for SettingValue {
    fn from(v: u64) -> Self {
        SettingValue::Num(v as f64)
    }
}

impl From<Vec<String>> for SettingValue {
    fn from(v: Vec<String>) -> Self {
        SettingValue::StrList(v)
    }
}

// ============================================================
// 状态
// ============================================================

/// 顶部状态行。
#[derive(Clone, Debug, Default)]
pub struct StatusLine {
    pub phase: String,
    pub busy: bool,
    pub turn_started_ms: i64,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub subagents: u32,
}

/// 对话历史页状态。
/// MCP 服务器运行态（设置页展示用；与 neosonia_mcp::McpServerState 字段一致）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct McpServerView {
    pub name: String,
    pub ok: bool,
    pub tools: usize,
    /// 错误文本，空串表示没有错误（设置页直接 trim/is_empty）。
    pub error: String,
    pub pid: Option<u32>,
}

/// MCP 整体运行态。
#[derive(Clone, Debug, Default)]
pub struct McpState {
    pub servers: Vec<McpServerView>,
    pub last_error: Option<String>,
}

/// 审查弹窗里的三种决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiskChoice {
    /// 允许这次操作。
    Approve,
    /// 拒绝这次操作。
    Deny,
    /// 拒绝，并且 1 天内拒绝同类操作。
    DenyClass,
}

/// 等待用户确认的高风险工具调用（审查弹窗内容）。
#[derive(Clone, Debug, PartialEq)]
pub struct RiskPrompt {
    pub call_id: String,
    pub tool: String,
    pub args: String,
    /// 1..=4。
    pub level: u8,
    pub reason: String,
    /// 同类操作标识（例如 `fs_write:C:/Windows`）。
    pub class: String,
    /// 4 分时 UI 要强调风险。
    pub emphasized: bool,
}

/// 人设编辑器的编辑态（新建或编辑已有）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PersonaEditState {
    pub is_new: bool,
    pub id: String,
    pub name: String,
    pub description: String,
    /// 人设正文（对应 Persona.system_prompt）。
    pub body: String,
    /// 校验/保存错误（空 = 无错误）。
    pub error: String,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryState {
    /// 历史列表是否覆盖在对话页上。
    pub open: bool,
    pub query: String,
    pub sessions: Vec<neosonia_core::ConversationSummary>,
    pub hits: Vec<neosonia_core::ConversationHit>,
    pub loading: bool,
    pub active_id: Option<String>,
    /// 当前会话 id（引擎首轮落库后回填）。
    pub current_id: Option<String>,
}

/// 局域网设备页状态。
#[derive(Clone, Debug, Default)]
pub struct RemoteState {
    pub enabled: bool,
    pub url: String,
    pub code: String,
    pub expires_at_ms: i64,
    pub qr_png: Option<std::path::PathBuf>,
    pub qr_pixels: Option<(u32, u32, Vec<u8>)>,
    pub devices: Vec<neosonia_core::TrustedDevice>,
    pub status: String,
    pub busy: bool,
}

pub struct AppState {
    pub paths: Paths,
    /// 设置草稿（serde_json 形状，面板直接指针读写）。
    pub draft: Value,
    /// 上次保存时的设置（用于检测脏状态）。
    pub saved_draft: Value,
    pub settings: Settings,

    pub proxy: Option<Proxy<Msg>>,
    pub theme_mode: ThemeMode,

    pub page: NavPage,
    pub items: Vec<ChatItem>,
    pub composer: String,
    pub status: StatusLine,
    pub tools: BTreeMap<String, usize>,
    pub toast: Option<(String, NoticeLevel, i64)>,
    pub last_error: Option<String>,

    pub memory_query: String,
    pub memory_items: Vec<MemoryItem>,
    /// 固定块编辑缓冲（面板直接写，保存时落库）。
    pub memory_blocks: BTreeMap<String, String>,
    /// 白名单输入框草稿。
    pub whitelist_input: String,

    pub mail: MailState,
    pub personas: Vec<Persona>,
    pub active_persona: String,

    /// 待发送附件（图片/文件/文件夹/截图）。
    pub attachments: Vec<neosonia_core::Attachment>,
    /// 对话历史。
    pub history: HistoryState,
    /// MCP 服务器运行态（设置页展示）。
    pub mcp: McpState,
    /// 设置页展开的分组 id。
    pub settings_open: Vec<String>,
    /// 当前工作模式 id（daily / coding / computer_use），随事件更新。
    pub agent_mode: String,
    /// 最近审查记录（最多 20 条，供设置页展示）。
    pub review_log: Vec<String>,
    /// 等待用户决定的高风险调用（None = 没有弹窗）。
    pub pending_risk: Option<RiskPrompt>,
    /// 模型 API 状态（绿/黄/橙/红小球）：provider / indicator / label / url。
    pub provider_status: Option<(String, String, String, String)>,
    /// 是否已触发过本次 Ctrl+V（防重复）。
    pub paste_armed: bool,
    /// 设备重命名：正在编辑的设备 id（空 = 未编辑）。
    pub device_rename_id: String,
    /// 设备重命名输入框内容。
    pub device_rename_input: String,
    /// OAuth 诊断：是否需要重新授权。
    pub mail_oauth_needs_reauth: bool,
    /// OAuth 诊断：中文建议。
    pub mail_oauth_status_message: String,
    /// token 受众（aud）。
    pub mail_oauth_audience: Vec<String>,
    /// token 权限（scp）。
    pub mail_oauth_scopes: Vec<String>,
    pub mail_oauth_has_imap_scope: bool,
    pub mail_oauth_has_smtp_scope: bool,
    /// 切页过渡动画开始时间（毫秒；0 = 不在动画中）。
    /// 用时间戳手工插值实现"只播一次"的过渡，而不是 Keyframes（循环动画）。
    pub page_anim_start_ms: i64,
    /// 当前会话的任务计划（update_plan 写入）。
    pub plan: Vec<PlanStepView>,
    /// 窗口是否被 Agent（后台任务）隐藏。
    pub window_hidden_by_agent: bool,
    /// 本会话运行中的子代理数（顶栏图标显示）。
    pub subagent_active: u32,
    /// 本会话启动过的子代理总数。
    pub subagent_total: u32,
    /// 页面切换两段式：先渲染当前页 + 加载提示，下一帧再构建目标页。
    pub page_pending: Option<NavPage>,    /// 人设编辑器状态（None = 不在编辑）。
    pub persona_edit: Option<PersonaEditState>,
    /// 邮箱 OAuth2 授权状态文字。
    pub mail_oauth_status: String,
    /// 聊天列表快照缓存：虚拟列表每帧要一份 Rc 快照，只在 items 变化时重建。
    pub chat_snapshot: std::cell::RefCell<Option<(u64, std::rc::Rc<Vec<ChatItem>>)>>,
    /// items 每变一次 +1（用于快照缓存失效）。
    pub items_version: u64,
    /// 本地索引条目数 / 是否在扫描（Tick 里周期性刷新，设置页展示）。
    pub index_count: u64,
    pub index_busy: bool,
    /// 局域网设备。
    pub remote: RemoteState,
    /// 已安装技能。
    pub skills: Vec<neosonia_core::SkillSummary>,
    /// 技能安装 URL 输入缓冲。
    pub skill_install_input: String,
    /// 索引根目录输入缓冲（多行，一行一个）。
    pub index_roots_input: String,

    pub test: TestResult,
    pub window_visible: bool,
    pub started_ms: i64,
    /// 界面是否需要重绘（用于避免无谓的 view 重建）。
    pub dirty: bool,
    /// 渲染优化：本帧是否需要滚动到底部（流式输出时置位一次）。
    pub scroll_bottom_pending: bool,
}

impl AppState {
    pub fn new(paths: Paths, settings: Settings) -> Self {
        let draft = serde_json::to_value(&settings).unwrap_or(Value::Null);
        let theme_mode = settings.ui.theme;
        let active_persona = settings.agent.persona.clone();
        let settings_enabled_remote = settings.remote.enabled;
        Self {
            paths,
            saved_draft: draft.clone(),
            draft,
            settings,
            proxy: None,
            theme_mode,
            page: NavPage::Chat,
            items: Vec::new(),
            composer: String::new(),
            status: StatusLine::default(),
            tools: BTreeMap::new(),
            toast: None,
            last_error: None,
            memory_query: String::new(),
            memory_items: Vec::new(),
            memory_blocks: BTreeMap::new(),
            whitelist_input: String::new(),
            mail: MailState::default(),
            personas: Vec::new(),
            active_persona,
            attachments: Vec::new(),
            history: HistoryState::default(),
            mcp: McpState::default(),
            settings_open: Vec::new(),
            agent_mode: "daily".into(),
            review_log: Vec::new(),
            pending_risk: None,
            provider_status: None,
            paste_armed: false,
            device_rename_id: String::new(),
            device_rename_input: String::new(),
            mail_oauth_needs_reauth: false,
            mail_oauth_status_message: String::new(),
            mail_oauth_audience: Vec::new(),
            mail_oauth_scopes: Vec::new(),
            mail_oauth_has_imap_scope: false,
            mail_oauth_has_smtp_scope: false,
            page_anim_start_ms: 0,
            plan: Vec::new(),
            window_hidden_by_agent: false,
            subagent_active: 0,
            subagent_total: 0,
            page_pending: None,
            persona_edit: None,
            mail_oauth_status: String::new(),
            chat_snapshot: std::cell::RefCell::new(None),
            items_version: 0,
            index_count: 0,
            index_busy: false,
            remote: RemoteState {
                enabled: settings_enabled_remote,
                ..Default::default()
            },
            skills: Vec::new(),
            skill_install_input: String::new(),
            index_roots_input: String::new(),
            test: TestResult::default(),
            window_visible: true,
            started_ms: neosonia_core::util::now_ms(),
            dirty: true,
            scroll_bottom_pending: false,
        }
    }

    // ---------- 设置草稿读写 ----------

    pub fn draft_str(&self, path: &str) -> String {
        self.draft
            .pointer(path)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    }

    pub fn draft_bool(&self, path: &str) -> bool {
        self.draft.pointer(path).and_then(|v| v.as_bool()).unwrap_or(false)
    }

    pub fn draft_num(&self, path: &str) -> f64 {
        self.draft.pointer(path).and_then(|v| v.as_f64()).unwrap_or(0.0)
    }

    pub fn draft_str_list(&self, path: &str) -> Vec<String> {
        self.draft
            .pointer(path)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default()
    }

    /// 把草稿写进 `path`（自动创建中间节点）。
    pub fn apply_setting(&mut self, path: &str, value: SettingValue) -> bool {
        let json = match value {
            SettingValue::Str(s) => Value::String(s),
            SettingValue::Bool(b) => Value::Bool(b),
            // 整数值必须写成 JSON 整数：Settings 里大量 u16/u32/u64/usize 字段，
            // serde_json 不接受 "1.0" 这种浮点形式反序列化成整数。
            SettingValue::Num(n) => {
                if n.is_finite() && n.fract() == 0.0 && n.abs() < 9.0e15 {
                    serde_json::json!(n as i64)
                } else {
                    serde_json::json!(n)
                }
            }
            SettingValue::StrList(v) => serde_json::json!(v),
        };
        let ok = set_pointer(&mut self.draft, path, json);
        if ok {
            self.dirty = true;
        }
        ok
    }

    /// 草稿是否有未保存修改。
    pub fn is_dirty(&self) -> bool {
        self.draft != self.saved_draft
    }

    /// 草稿 -> Settings。失败时返回错误并保留旧设置。
    pub fn draft_to_settings(&self) -> anyhow::Result<Settings> {
        Ok(serde_json::from_value(self.draft.clone())?)
    }

    /// 保存成功后同步内存里的设置。
    pub fn accept_saved(&mut self, settings: Settings) {
        self.draft = serde_json::to_value(&settings).unwrap_or(Value::Null);
        self.saved_draft = self.draft.clone();
        self.theme_mode = settings.ui.theme;
        self.settings = settings;
        self.dirty = true;
    }

    // ---------- 聊天 ----------

    pub fn push_user(&mut self, text: &str) {
        self.items.push(ChatItem::user(text));
        self.items_version += 1;
        self.dirty = true;
    }

    /// 用户消息 + 图片缩略图（上传/截图）。
    pub fn push_user_with_thumbs(&mut self, text: &str, thumbs: Vec<(u32, u32, Vec<u8>)>) {
        self.items
            .push(ChatItem::User { text: text.to_string(), thumbs });
        self.items_version += 1;
        self.dirty = true;
    }

    /// 保证有一个正在流式输出的 assistant 气泡。
    pub fn ensure_streaming_assistant(&mut self) -> &mut ChatItem {
        let need_new = match self.items.last() {
            Some(ChatItem::Assistant { streaming, .. }) => !*streaming,
            _ => true,
        };
        if need_new {
            self.items.push(ChatItem::assistant());
        }
        self.items.last_mut().expect("just pushed")
    }

    pub fn append_assistant_text(&mut self, delta: &str) {
        self.dirty = true;
        if let ChatItem::Assistant { text, .. } = self.ensure_streaming_assistant() {
            text.push_str(delta);
        }
    }

    pub fn append_reasoning(&mut self, delta: &str) {
        self.dirty = true;
        if let ChatItem::Assistant { reasoning, .. } = self.ensure_streaming_assistant() {
            reasoning.push_str(delta);
        }
    }

    pub fn finish_assistant(&mut self, error: Option<String>) {
        for item in self.items.iter_mut().rev() {
            if let ChatItem::Assistant { streaming, error: e, .. } = item {
                if *streaming {
                    *streaming = false;
                    if e.is_none() {
                        *e = error;
                    }
                    break;
                }
            }
        }
        self.dirty = true;
    }

    pub fn tool_card_mut(&mut self, call_id: &str) -> Option<&mut ToolCard> {
        self.items.iter_mut().find_map(|it| match it {
            ChatItem::Tool(card) if card.call_id == call_id => Some(card),
            _ => None,
        })
    }

    pub fn subagent_mut(&mut self, id: &str) -> Option<&mut SubagentCard> {
        self.items.iter_mut().find_map(|it| match it {
            ChatItem::Subagent(card) if card.id == id => Some(card),
            _ => None,
        })
    }

    pub fn push_notice(&mut self, text: impl Into<String>, level: NoticeLevel) {
        self.items.push(ChatItem::Notice { text: text.into(), level });
        self.dirty = true;
    }

    /// 最近一段会话的纯文本（给"总结并记住"用）。
    pub fn transcript(&self, max_chars: usize) -> String {
        let mut out = String::new();
        for item in &self.items {
            match item {
                ChatItem::User { text, .. } => out.push_str(&format!("用户: {text}\n")),
                ChatItem::Assistant { text, .. } => out.push_str(&format!("助手: {text}\n")),
                ChatItem::Tool(c) => {
                    out.push_str(&format!("[工具 {} {}]\n", c.name, c.state.label()))
                }
                _ => {}
            }
            if out.chars().count() > max_chars {
                break;
            }
        }
        out
    }
}

/// JSON Pointer 写入（自动建中间对象）。返回是否写入成功。
fn set_pointer(root: &mut Value, path: &str, value: Value) -> bool {
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return false;
    }
    let mut cur = root;
    for p in &parts[..parts.len() - 1] {
        if !cur.is_object() {
            *cur = Value::Object(Default::default());
        }
        let obj = cur.as_object_mut().expect("made object");
        cur = obj.entry((*p).to_string()).or_insert(Value::Object(Default::default()));
    }
    if !cur.is_object() {
        *cur = Value::Object(Default::default());
    }
    cur.as_object_mut()
        .expect("object")
        .insert(parts[parts.len() - 1].to_string(), value);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_pointer_writes_nested() {
        let mut v = serde_json::json!({});
        assert!(set_pointer(&mut v, "/server/base_url", Value::String("x".into())));
        assert_eq!(v.pointer("/server/base_url").unwrap(), "x");
        assert!(set_pointer(&mut v, "/a/b/c", Value::Bool(true)));
        assert_eq!(v.pointer("/a/b/c").unwrap(), true);
    }

    #[test]
    fn state_streaming_appends() {
        let paths = Paths::from_root("C:/tmp/neosonia-test");
        let mut st = AppState::new(paths, Settings::default());
        st.append_assistant_text("hello");
        st.append_assistant_text(" world");
        st.finish_assistant(None);
        match st.items.last().unwrap() {
            ChatItem::Assistant { text, streaming, .. } => {
                assert_eq!(text, "hello world");
                assert!(!*streaming);
            }
            _ => panic!("expected assistant"),
        }
    }

    #[test]
    fn dirty_tracking() {
        let paths = Paths::from_root("C:/tmp/neosonia-test");
        let mut st = AppState::new(paths, Settings::default());
        assert!(!st.is_dirty());
        st.apply_setting("/ui/hotkey", SettingValue::Str("Ctrl+Alt+Space".into()));
        assert!(st.is_dirty());
        let s = st.draft_to_settings().unwrap();
        assert_eq!(s.ui.hotkey, "Ctrl+Alt+Space");
    }
}
