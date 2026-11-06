//! 配置与路径。
//!
//! 配置文件是一个 TOML（默认 `config/settings.toml`），可被 UI 热更新。
//! 所有字段都带 `#[serde(default)]`，所以旧配置文件缺字段也能加载。
//!
//! 密钥（API key / 邮箱密码）默认只存本地 TOML；也支持 `env:DEEPSEEK_API_KEY`
//! 这种引用写法，加载时展开。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ============================================================
// 路径
// ============================================================

/// 应用的目录布局。所有路径都从 `root` 推导。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Paths {
    /// 项目/安装根目录。
    pub root: PathBuf,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub workspace: PathBuf,
    pub personas: PathBuf,
    pub logs: PathBuf,
    pub screenshots: PathBuf,
    pub settings_file: PathBuf,
    pub memory_db: PathBuf,
    /// 技能目录（`skills/<id>/SKILL.md`）。
    pub skills: PathBuf,
    /// 聊天附件（上传的图片/文件）。
    pub uploads: PathBuf,
    /// 配对二维码 PNG。
    pub qrcodes: PathBuf,
    /// 本地文件索引库（类 Everything）。
    pub file_index_db: PathBuf,
    /// 已信任的远程设备。
    pub devices_file: PathBuf,
    /// 对话历史库（与 memory 共用 SQLite 文件也可以，这里单独一个便于迁移）。
    pub conversations_db: PathBuf,
    /// 即时存档备份区（`backups/`）。
    ///
    /// 只能由 `snapshot_save`（新版本覆盖）/ `snapshot_restore`（放回原位置）/
    /// `snapshot_delete`（删除备份）修改；普通文件工具与命令工具对这里只读。
    pub backups: PathBuf,
}

impl Paths {
    /// 解析根目录，优先级：
    /// 1. 环境变量 `NEOSONIA_HOME`
    /// 2. 从 exe 往上找带 `Cargo.toml` 的目录（开发时）
    /// 3. **exe 所在目录**（安装版：config/data/workspace/skills 都跟着安装位置走）
    /// 4. 系统数据目录 `ProjectDirs`（安装目录不可写时的兜底）
    pub fn resolve() -> Self {
        let root = std::env::var_os("NEOSONIA_HOME")
            .map(PathBuf::from)
            .or_else(Self::find_project_root)
            .or_else(Self::exe_dir)
            .unwrap_or_else(Self::system_data_dir);
        Self::from_root(root)
    }

    /// exe 所在目录（可写才返回）。
    ///
    /// 安装版（自解压安装包）把程序解到用户目录（如
    /// `%LOCALAPPDATA%\Programs\NeoSonia`），这里让 config/data/logs/workspace/
    /// skills 全部落在安装目录下，实现"绿色便携 + 数据跟随安装位置"。
    /// 若安装到 Program Files 等不可写位置，则回退到系统数据目录。
    fn exe_dir() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?.to_path_buf();
        let probe = dir.join(".neosonia-write-probe");
        match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&probe)
        {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                Some(dir)
            }
            Err(_) => None,
        }
    }

    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let config_dir = root.join("config");
        let data_dir = root.join("data");
        let logs = root.join("logs");
        let screenshots = data_dir.join("screenshots");
        Self {
            settings_file: config_dir.join("settings.toml"),
            memory_db: data_dir.join("memory.sqlite3"),
            conversations_db: data_dir.join("conversations.sqlite3"),
            file_index_db: data_dir.join("file_index.sqlite3"),
            devices_file: data_dir.join("devices.json"),
            skills: root.join("skills"),
            uploads: data_dir.join("uploads"),
            qrcodes: data_dir.join("qrcodes"),
            workspace: root.join("workspace"),
            personas: root.join("personas"),
            backups: root.join("backups"),
            config_dir,
            data_dir,
            logs,
            screenshots,
            root,
        }
    }

    fn find_project_root() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let mut dir = exe.parent()?.to_path_buf();
        for _ in 0..8 {
            if dir.join("Cargo.toml").is_file() && dir.join("crates").is_dir() {
                return Some(dir);
            }
            dir = dir.parent()?.to_path_buf();
        }
        None
    }

    fn system_data_dir() -> PathBuf {
        directories::ProjectDirs::from("dev", "NeoSonia", "NeoSonia")
            .map(|d| d.data_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// 建好所有目录（幂等）。
    pub fn ensure(&self) -> std::io::Result<()> {
        for d in [
            &self.config_dir,
            &self.data_dir,
            &self.workspace,
            &self.personas,
            &self.logs,
            &self.screenshots,
            &self.skills,
            &self.uploads,
            &self.qrcodes,
            &self.backups,
        ] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }

    pub fn screenshot_path(&self, stem: &str) -> PathBuf {
        self.screenshots.join(format!("{stem}.png"))
    }
}

// ============================================================
// 设置
// ============================================================

/// 一个可保存、一键切换的 API 配置（`[[server.profiles]]`）。
///
/// `server.*` 仍是"自定义/默认"配置；`active_profile` 命中某个 profile 时，
/// 用 profile 覆盖 base_url / model / api_key / reasoning_effort（空表示沿用默认）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiProfile {
    /// 唯一名字（UI 上显示，也是 active_profile 的键）。
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// 空 = 沿用 `server.reasoning_effort`。
    pub reasoning_effort: String,
}

/// 模型服务（任何 OpenAI 兼容端点：DeepSeek / OpenAI / Ollama / vLLM / OpenRouter...）
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerSettings {
    /// 例如 `https://api.deepseek.com/v1`（不带 `/chat/completions`）。
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// 温度；思考模式下部分模型忽略。
    pub temperature: f32,
    pub max_tokens: u32,
    /// 上下文窗口（token），用于裁剪历史。
    pub context_window: u32,
    /// `low` / `medium` / `high` / 空 = 不发该字段。
    pub reasoning_effort: String,
    /// 是否请求流式输出。
    pub stream: bool,
    /// 单次 HTTP 请求超时（秒）。思考模型可能很慢。
    pub request_timeout_secs: u64,
    /// 额外请求头。
    pub headers: BTreeMap<String, String>,
    /// 是否把 `thinking` 打开（DeepSeek 专有扩展，非该系列留空即可）。
    pub thinking: bool,
    /// 保存的 API 配置列表（可一键切换）。
    pub profiles: Vec<ApiProfile>,
    /// 当前使用的 profile 名字；空或不存在时回退到上面的默认字段。
    pub active_profile: String,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-flash".into(),
            api_key: String::new(),
            temperature: 0.6,
            max_tokens: 32768,
            context_window: 1_048_576,
            reasoning_effort: "high".into(),
            stream: true,
            request_timeout_secs: 600,
            headers: BTreeMap::new(),
            thinking: true,
            profiles: Vec::new(),
            active_profile: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockSide {
    Right,
    Left,
}

impl Default for DockSide {
    fn default() -> Self {
        DockSide::Right
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Dark,
    Light,
    System,
}

impl Default for ThemeMode {
    fn default() -> Self {
        ThemeMode::Dark
    }
}

/// Agent 工作模式（提示词隔离）。
///
/// - `Daily`：日常助手，注入人设 + 搜索/文件提示词；计算机使用提示词按需（用工具时）展开；
/// - `Coding`：专注编程，**不注入人设**，只保留安全等基础提示词 + 编程规范；
/// - `ComputerUse`：深度计算机使用，**不注入人设**，只保留基础/安全提示词，
///   计算机使用、文件、命令相关提示词全部展开。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    #[serde(alias = "day", alias = "normal")]
    Daily,
    #[serde(alias = "code", alias = "programming")]
    Coding,
    #[serde(alias = "computeruse", alias = "cu", alias = "computer-use")]
    ComputerUse,
}

impl Default for AgentMode {
    fn default() -> Self {
        AgentMode::Daily
    }
}

impl AgentMode {
    pub fn label(&self) -> &'static str {
        match self {
            AgentMode::Daily => "日常",
            AgentMode::Coding => "编程",
            AgentMode::ComputerUse => "计算机使用",
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            AgentMode::Daily => "daily",
            AgentMode::Coding => "coding",
            AgentMode::ComputerUse => "computer_use",
        }
    }
}

/// 命令审查（风险评分）设置。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewSettings {
    /// 总开关；关闭 = 完全权限（等价于旧行为）。
    pub enabled: bool,
    /// 低风险工具是否也走审查（默认只审"有副作用"的工具）。
    pub review_all_tools: bool,
    /// 2 分授权后，同类操作自动执行的有效期。
    pub allow_ttl_secs: u64,
    /// 4 分被拒绝后，同类操作自动拒绝的有效期。
    pub deny_ttl_secs: u64,
    /// 评分用的模型（空 = 跟随主模型）。
    pub model: String,
    /// 评分请求超时。
    pub timeout_secs: u64,
}

impl Default for ReviewSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            review_all_tools: false,
            allow_ttl_secs: 86_400,
            deny_ttl_secs: 86_400,
            model: String::new(),
            timeout_secs: 45,
        }
    }
}

/// 界面 / 浮窗行为。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    /// 全局快捷键，字符串形式例如 `Alt+Space`、`Ctrl+Alt+S`。
    pub hotkey: String,
    /// 浮窗宽度（逻辑像素）。
    pub width: f64,
    /// 浮窗高度占工作区比例（0.5 ~ 1.0）。
    pub height_ratio: f64,
    /// 距屏幕边缘的间距。
    pub margin: f64,
    pub side: DockSide,
    pub always_on_top: bool,
    /// 是否显示标题栏（false = 纯浮层，用顶部自绘拖拽区）。
    pub decorations: bool,
    /// 背景不透明度（0.6 ~ 1.0）；< 1 需要透明窗口。
    pub opacity: f64,
    pub theme: ThemeMode,
    /// 字体缩放。
    pub font_scale: f64,
    pub show_tray: bool,
    /// 启动时隐藏（只留托盘/快捷键唤出）。
    pub start_hidden: bool,
    /// 快捷键唤出时是否抢焦点。
    pub focus_on_show: bool,
    /// 打开窗口时的动画（位移/淡入）。
    pub animate: bool,
    /// 开机自启（写 HKCU Run 项，自启时只跑后端、不打开窗口）。
    pub autostart: bool,
    /// 「快捷截图发给模型」的全局快捷键。
    pub screenshot_hotkey: String,
    /// 调用「识别屏幕 / 操作鼠标键盘」类工具时，在屏幕上显示活动提示（HUD）。
    pub computer_activity_indicator: bool,
    /// 主快捷键之后的组合键：按下主快捷键后 `chord_timeout_ms` 内再按它 = 立即截图发模型。
    pub screenshot_chord: String,
    /// 组合键等待时间（毫秒）。
    pub chord_timeout_ms: u64,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            hotkey: "Alt+Space".into(),
            width: 424.0,
            height_ratio: 0.88,
            margin: 12.0,
            side: DockSide::Right,
            always_on_top: true,
            decorations: false,
            opacity: 1.0,
            theme: ThemeMode::Dark,
            font_scale: 1.0,
            show_tray: true,
            start_hidden: false,
            focus_on_show: true,
            animate: true,
            autostart: false,
            screenshot_hotkey: "Ctrl+Alt+A".into(),
            computer_activity_indicator: true,
            screenshot_chord: String::new(),
            chord_timeout_ms: 1500,
        }
    }
}

/// 危险操作的批准策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// 全部自动执行。
    Auto,
    /// 白名单外的写操作/命令要确认。
    Ask,
    /// 什么都不问，全部放行（用户已明确授权）。
    Never,
}

impl Default for ApprovalMode {
    fn default() -> Self {
        ApprovalMode::Auto
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// 当前人设 id（对应 personas/*.md 的文件名或内置 default）。
    pub persona: String,
    /// 工作目录（相对路径则相对 paths.workspace）。
    pub workspace_dir: String,
    /// 单个用户轮次最多几轮工具调用。
    pub max_iterations: u32,
    /// 喂回模型的单条工具输出上限（字符）。
    pub max_tool_output_chars: usize,
    /// 工具输出超出时保留头尾。
    pub approval_mode: ApprovalMode,
    pub subagents_enabled: bool,
    /// 同时运行的子代理上限。
    pub max_subagents: u32,
    /// 额外系统提示（追加在人设后面）。
    pub system_prompt_extra: String,
    /// 送给模型的历史消息条数上限。
    pub history_limit: usize,
    /// 每次请求前注入的长期记忆条数。
    pub memory_inject: usize,
    /// 是否让模型自动写长期记忆。
    pub auto_memory: bool,
    /// 是否在回复里显示思考过程。
    pub show_reasoning: bool,
    /// 当前工作模式（提示词隔离）：daily / coding / computer_use。
    pub mode: AgentMode,
    /// 视觉输入：把截图作为图片消息发给模型（需要模型支持 image 输入）。
    pub vision: bool,
    /// 单张图片送给模型前的最大边长（像素），过大自动缩小。
    pub vision_max_edge: u32,
    /// 上下文压缩：历史 token 超过阈值时自动把旧消息摘要成一段，保留最近若干条原样。
    pub context_compress: bool,
    /// 触发压缩的 token 估算阈值（默认约 70% of 1M）。
    pub context_threshold_tokens: u64,
    /// 压缩后保留最近多少条原始消息。
    pub keep_recent_messages: usize,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            persona: "default".into(),
            workspace_dir: String::new(),
            max_iterations: 40,
            max_tool_output_chars: 20_000,
            approval_mode: ApprovalMode::Auto,
            subagents_enabled: true,
            max_subagents: 3,
            system_prompt_extra: String::new(),
            history_limit: 80,
            memory_inject: 8,
            auto_memory: true,
            show_reasoning: true,
            mode: AgentMode::Daily,
            vision: true,
            vision_max_edge: 1536,
            context_compress: true,
            context_threshold_tokens: 700_000,
            keep_recent_messages: 40,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellKind {
    /// 按平台自动选：Windows -> powershell，其它 -> bash。
    Auto,
    Powershell,
    Cmd,
    Bash,
    Sh,
}

impl Default for ShellKind {
    fn default() -> Self {
        ShellKind::Auto
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsSettings {
    /// 命令默认超时（秒）。0 = 不超时（靠用户取消）。默认 30 分钟，
    /// 明确不接受 Sonia 那种"30 秒就脱离"的行为。
    pub command_timeout_secs: u64,
    /// 允许执行命令。
    pub allow_shell: bool,
    pub shell: ShellKind,
    /// 命令输出保留上限（字符）。
    pub max_output_chars: usize,
    /// 截图最长边（像素），超过则缩放。
    pub screenshot_max_edge: u32,
    /// 允许屏幕截图 / 鼠标键盘模拟。
    pub allow_input_simulation: bool,
    /// 允许 UI Automation 读取其它程序控件树。
    pub uia_enabled: bool,
    /// 允许通过 CDP 操作 Chromium 系应用。
    pub cdp_enabled: bool,
    /// Chrome 调试端口（用于连接已开 debug 端口的浏览器/Electron 应用）。
    pub cdp_port: u16,
}

impl Default for ToolsSettings {
    fn default() -> Self {
        Self {
            command_timeout_secs: 1800,
            allow_shell: true,
            shell: ShellKind::Auto,
            max_output_chars: 60_000,
            screenshot_max_edge: 1600,
            allow_input_simulation: true,
            uia_enabled: true,
            cdp_enabled: true,
            cdp_port: 9222,
        }
    }
}

/// IMAP 收信配置（不依赖 Outlook / 本机客户端）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ImapSettings {
    pub host: String,
    pub port: u16,
    /// 直连 TLS（993）为 true；明文 143 为 false。
    pub tls: bool,
    pub user: String,
    pub password: String,
    /// 收件箱目录名，默认 INBOX。
    pub mailbox: String,
    pub timeout_secs: u64,
}

impl Default for ImapSettings {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 993,
            tls: true,
            user: String::new(),
            password: String::new(),
            mailbox: "INBOX".into(),
            timeout_secs: 30,
        }
    }
}

/// SMTP 发信配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    /// 465 = 隐式 TLS；587 = STARTTLS。
    pub implicit_tls: bool,
    pub starttls: bool,
    pub user: String,
    pub password: String,
    /// 发件人地址（留空则用 user）。
    pub from: String,
    pub from_name: String,
    pub timeout_secs: u64,
}

impl Default for SmtpSettings {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 587,
            implicit_tls: false,
            starttls: true,
            user: String::new(),
            password: String::new(),
            from: String::new(),
            from_name: "NeoSonia".into(),
            timeout_secs: 30,
        }
    }
}

/// OAuth2 提供方（Thunderbird 内置了客户端 ID，我们也内置一份公开的）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthProvider {
    #[serde(alias = "microsoft", alias = "outlook", alias = "office365")]
    Microsoft,
    #[serde(alias = "gmail")]
    Google,
    /// 自定义提供方（自己填 client_id / endpoints）。
    Custom,
}

impl Default for OAuthProvider {
    fn default() -> Self {
        OAuthProvider::Microsoft
    }
}

/// 邮箱 OAuth2 登录（像 Thunderbird 一样点一下按钮走浏览器授权）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MailOAuthSettings {
    /// 是否用 OAuth2 替代密码登录。
    pub enabled: bool,
    pub provider: OAuthProvider,
    /// 覆盖内置客户端 ID（留空用内置的公开客户端 ID）。
    pub client_id: String,
    /// 微软租户（common / organizations / consumers / 租户 id）。
    pub tenant: String,
    /// 授权后拿到的邮箱地址。
    pub email: String,
    /// 长期刷新令牌（登录后由程序写入，不要手填）。
    pub refresh_token: String,
    /// 访问令牌缓存与过期时间。
    pub access_token: String,
    pub expires_at_ms: i64,
    /// 自定义授权端点（provider = custom 时必填）。
    pub auth_url: String,
    pub token_url: String,
    pub scope: String,
    /// OAuth 客户端密钥（可选）。Microsoft public client 留空；Google 自建"桌面应用"客户端换 token 时需要。
    #[serde(default)]
    pub client_secret: String,
    /// 仅在 HTTPS 证书校验失败时，是否允许降级为"不校验证书"重试一次。
    /// 默认 false；Kaspersky 等 HTTPS 拦截（MITM）环境可临时打开。
    #[serde(default)]
    pub allow_insecure_tls: bool,
}

impl Default for MailOAuthSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: OAuthProvider::Microsoft,
            client_id: String::new(),
            tenant: "common".into(),
            email: String::new(),
            refresh_token: String::new(),
            access_token: String::new(),
            expires_at_ms: 0,
            auth_url: String::new(),
            token_url: String::new(),
            scope: String::new(),
            client_secret: String::new(),
            allow_insecure_tls: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MailSettings {
    pub enabled: bool,
    /// 邮箱服务商：outlook / gmail / 163 / 126 / qq / custom。
    /// 用于 UI 预填 IMAP/SMTP 参数；实际连接仍用下面的 host/port。
    pub provider: String,
    /// OAuth2 登录（Outlook / Gmail）。
    pub oauth: MailOAuthSettings,
    pub imap: ImapSettings,
    pub smtp: SmtpSettings,
    /// 白名单：只有这些地址的来信会自动回复/触发代理。
    pub whitelist: Vec<String>,
    /// 白名单备注（地址 → 备注，纯展示用，不影响匹配）。
    #[serde(default)]
    pub whitelist_notes: std::collections::BTreeMap<String, String>,
    /// 轮询间隔（秒）。
    pub poll_interval_secs: u64,
    /// 收到白名单来信后是否自动用代理起草并回复。
    pub auto_reply: bool,
    /// 是否允许代理主动给白名单外的人发信（默认否）。
    pub allow_outbound_to_anyone: bool,
    /// 单封自动回复的最大字符数。
    pub max_reply_chars: usize,
    /// 自动回复签名。
    pub signature: String,
    /// 只处理未读，处理后标记已读。
    pub mark_read: bool,
    /// 每封邮件正文喂给模型的上限。
    pub max_body_chars: usize,
}

impl Default for MailSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "custom".into(),
            oauth: MailOAuthSettings::default(),
            imap: ImapSettings::default(),
            smtp: SmtpSettings::default(),
            whitelist: Vec::new(),
            whitelist_notes: std::collections::BTreeMap::new(),
            poll_interval_secs: 300,
            auto_reply: false,
            allow_outbound_to_anyone: false,
            max_reply_chars: 4000,
            signature: String::new(),
            mark_read: true,
            max_body_chars: 12_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchProvider {
    /// 聚合（默认，仿 SearXNG）：并行打百度/搜狗/必应中国/360，去重合并排序。
    #[serde(alias = "auto")]
    /// 国内可访问，不依赖任何自建服务。
    Aggregated,
    Baidu,
    Sogou,
    Bing,
    Duckduckgo,
    /// DeepSeek 官方搜索 API（`/v1/tools/search`，需要 DeepSeek 的 key）。
    DeepSeek,
    /// 自建 SearXNG 实例（可选，不依赖）。
    Searxng,
    /// 不用搜索引擎，仅抓 URL。
    None,
}

impl Default for SearchProvider {
    fn default() -> Self {
        SearchProvider::Aggregated
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSettings {
    pub search_provider: SearchProvider,
    /// SearXNG 实例地址（仅 provider = searxng 时用）。
    pub searxng_url: String,
    pub user_agent: String,
    /// 单页正文最多取多少字符。
    pub max_chars: usize,
    pub timeout_secs: u64,
    /// 允许 http 明文。
    pub allow_http: bool,
    pub proxy: String,
}

impl Default for WebSettings {
    fn default() -> Self {
        Self {
            search_provider: SearchProvider::Aggregated,
            searxng_url: String::new(),
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                         (KHTML, like Gecko) Chrome/131.0 Safari/537.36 NeoSonia/0.1"
                .into(),
            max_chars: 20_000,
            timeout_secs: 45,
            allow_http: true,
            proxy: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MemorySettings {
    pub enabled: bool,
    /// 事实条数上限，超过后按 重要度×时间 淘汰。
    pub max_items: i64,
    /// 每多少条对话做一次摘要入库。
    pub summarize_every: i64,
    /// 每次注入的记忆条数。
    pub recall_limit: usize,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_items: 5000,
            summarize_every: 40,
            recall_limit: 8,
        }
    }
}

// ============================================================
// 局域网访问（远端设备）
// ============================================================

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteSettings {
    /// 是否启用局域网 Web 服务（手机/平板/别的电脑连过来聊天）。
    pub enabled: bool,
    /// 监听地址（0.0.0.0 = 所有网卡）。
    pub bind: String,
    pub port: u16,
    /// 新设备必须扫码配对（强烈建议保持 true）。
    pub require_pairing: bool,
    /// 最多信任多少台设备。
    pub max_devices: u32,
    /// 只接受私有网段（RFC1918 / 链路本地）的连接，公网一律拒绝。
    pub lan_only: bool,
    /// 配对码有效期（秒）。
    pub pair_ttl_secs: u64,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "0.0.0.0".into(),
            port: 8787,
            require_pairing: true,
            max_devices: 8,
            lan_only: true,
            pair_ttl_secs: 600,
        }
    }
}

// ============================================================
// MCP（Model Context Protocol）服务器
// ============================================================

/// MCP 服务器传输方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    /// 本地子进程 + stdio 上的 JSON-RPC（最常用，`npx -y @modelcontextprotocol/server-xxx`）。
    Stdio,
    /// Streamable HTTP（MCP 2025 规范里的远程传输）。
    Http,
}

impl Default for McpTransport {
    fn default() -> Self {
        McpTransport::Stdio
    }
}

/// 一台 MCP 服务器的配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    /// 服务器标识（也用于工具名前缀，必须唯一、只含字母数字下划线）。
    pub name: String,
    pub enabled: bool,
    pub transport: McpTransport,
    /// stdio 传输的可执行文件（npx / uvx / node / python / 绝对路径）。
    pub command: String,
    pub args: Vec<String>,
    /// 追加的环境变量（`${VAR}` 会在加载时展开）。
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    /// http 传输的地址（例如 `http://127.0.0.1:3000/mcp`）。
    pub url: String,
    /// 启动握手超时。
    pub startup_timeout_secs: u64,
    /// 单次 tools/call 超时。
    pub call_timeout_secs: u64,
    /// 工具名前缀（默认 `mcp_<name>_`，太长时模型难认，可自定义）。
    pub tool_prefix: String,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: String::new(),
            url: String::new(),
            startup_timeout_secs: 30,
            call_timeout_secs: 120,
            tool_prefix: String::new(),
        }
    }
}

/// MCP 总配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct McpSettings {
    /// 总开关：关闭时不启动任何 MCP 服务器、也不注册 MCP 工具。
    pub enabled: bool,
    pub servers: Vec<McpServerConfig>,
    /// 所有服务器暴露的工具总数上限（防止工具列表爆炸）。
    pub max_total_tools: usize,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            servers: Vec::new(),
            max_total_tools: 100,
        }
    }
}

// ============================================================
// 本地文件索引（类 Everything）
// ============================================================

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexSettings {
    /// 是否建立本地文件索引（文件名极速搜索）。
    pub enabled: bool,
    /// 要索引的根目录（支持 `~`）。
    pub roots: Vec<String>,
    /// 跳过的目录名（小写比较）。
    pub exclude: Vec<String>,
    /// 索引条目上限（超过后停止加入新条目）。
    pub max_entries: u32,
    /// 启动时自动扫描；关闭则只能在工具里手动 `reindex_files`。
    pub autoscan: bool,
}

impl Default for IndexSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            roots: vec![
                "~/Desktop".into(),
                "~/Documents".into(),
                "~/Downloads".into(),
                "~/Pictures".into(),
            ],
            exclude: vec![
                "node_modules".into(),
                ".git".into(),
                "target".into(),
                ".cache".into(),
                "appdata".into(),
                "$recycle.bin".into(),
                "system volume information".into(),
            ],
            max_entries: 2_000_000,
            autoscan: true,
        }
    }
}

/// 全部设置。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server: ServerSettings,
    pub ui: UiSettings,
    pub agent: AgentSettings,
    pub tools: ToolsSettings,
    pub mail: MailSettings,
    pub web: WebSettings,
    pub memory: MemorySettings,
    pub remote: RemoteSettings,
    pub index: IndexSettings,
    /// MCP（Model Context Protocol）服务器。
    pub mcp: McpSettings,
    /// 命令审查（风险评分 + 授权）。
    pub review: ReviewSettings,
}

impl Settings {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        let mut s: Settings = toml::from_str(&text)?;
        s.expand_env();
        Ok(s)
    }

    /// 读取（不存在则默认），并保证配置文件存在。
    ///
    /// 同时做 v0.5 的数据迁移：旧版本的默认 `max_tokens = 8192` 太小
    /// （用户反馈"默认输出 8192 太小"），已保存的配置里如果还是 8192，
    /// 升级后自动改成 32768 并写回。
    pub fn load_or_create(paths: &Paths) -> anyhow::Result<Self> {
        paths.ensure()?;
        let existed = paths.settings_file.is_file();
        let old_text = if existed {
            std::fs::read_to_string(&paths.settings_file).ok()
        } else {
            None
        };
        let mut s = Self::load(&paths.settings_file)?;
        let mut changed = false;
        if let Some(text) = &old_text
            && s.server.max_tokens == 8192
            && text.contains("max_tokens")
        {
            s.server.max_tokens = 32_768;
            changed = true;
        }
        // 用户真实配置迁移：旧版默认 bind = 127.0.0.1，手机（局域网）连不上。
        // 绑定回环改成 0.0.0.0；显式配置其它地址不改。
        if s.remote.bind.trim() == "127.0.0.1" {
            s.remote.bind = "0.0.0.0".to_string();
            changed = true;
        }
        if !existed || changed {
            s.save(&paths.settings_file)?;
        }
        Ok(s)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        crate::util::atomic_write(path, text.as_bytes())
    }

    /// 把 `env:FOO` / `${FOO}` 形式的字段展开成环境变量值。
    fn expand_env(&mut self) {
        expand(&mut self.server.api_key);
        for profile in &mut self.server.profiles {
            expand(&mut profile.api_key);
        }
        expand(&mut self.mail.imap.password);
        expand(&mut self.mail.smtp.password);
        for v in self.server.headers.values_mut() {
            expand(v);
        }
        for server in self.mcp.servers.iter_mut() {
            for v in server.env.values_mut() {
                expand(v);
            }
            expand(&mut server.command);
            expand(&mut server.url);
        }
    }

    /// 当前生效的模型服务配置：`active_profile` 命中 profile 时覆盖
    /// base_url / model / api_key / reasoning_effort（空字段沿用默认）。
    pub fn effective_server(&self) -> ServerSettings {
        let mut server = self.server.clone();
        let active = self.server.active_profile.trim();
        if active.is_empty() {
            return server;
        }
        let Some(profile) = self
            .server
            .profiles
            .iter()
            .find(|profile| profile.name.trim() == active)
        else {
            return server;
        };
        if !profile.base_url.trim().is_empty() {
            server.base_url = profile.base_url.trim().to_string();
        }
        if !profile.model.trim().is_empty() {
            server.model = profile.model.trim().to_string();
        }
        if !profile.api_key.trim().is_empty() {
            server.api_key = profile.api_key.trim().to_string();
        }
        if !profile.reasoning_effort.trim().is_empty() {
            server.reasoning_effort = profile.reasoning_effort.trim().to_string();
        }
        server
    }

    /// 实际使用的 API key：当前生效配置优先，其次环境变量。
    pub fn effective_api_key(&self) -> String {
        let server = self.effective_server();
        if !server.api_key.trim().is_empty() {
            return server.api_key.trim().to_string();
        }
        for k in ["NEOSONIA_API_KEY", "DEEPSEEK_API_KEY", "OPENAI_API_KEY"] {
            if let Ok(v) = std::env::var(k) {
                if !v.trim().is_empty() {
                    return v.trim().to_string();
                }
            }
        }
        String::new()
    }

    /// 新增或覆盖一个 API profile（按 name 匹配）。返回是否覆盖了已有项。
    pub fn upsert_profile(&mut self, profile: ApiProfile) -> anyhow::Result<bool> {
        let name = profile.name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("API 配置名字不能为空，请先给这个配置起一个名字。");
        }
        let mut profile = profile;
        profile.name = name.clone();
        if let Some(existing) = self
            .server
            .profiles
            .iter_mut()
            .find(|existing| existing.name.trim() == name)
        {
            *existing = profile;
            return Ok(true);
        }
        self.server.profiles.push(profile);
        Ok(false)
    }

    /// 删除一个 API profile；如果删除的是当前 active，回退到默认配置。返回是否删除。
    pub fn remove_profile(&mut self, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let before = self.server.profiles.len();
        self.server
            .profiles
            .retain(|profile| profile.name.trim() != name);
        let removed = self.server.profiles.len() != before;
        if removed && self.server.active_profile.trim() == name {
            self.server.active_profile.clear();
        }
        removed
    }

    /// 切换当前 profile；名字不存在时返回 false 且保持原值。
    pub fn set_active_profile(&mut self, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            self.server.active_profile.clear();
            return true;
        }
        if self
            .server
            .profiles
            .iter()
            .any(|profile| profile.name.trim() == name)
        {
            self.server.active_profile = name.to_string();
            true
        } else {
            false
        }
    }

    pub fn effective_imap_password(&self) -> String {
        first_non_empty(&[
            self.mail.imap.password.clone(),
            std::env::var("NEOSONIA_IMAP_PASSWORD").unwrap_or_default(),
        ])
    }

    pub fn effective_smtp_password(&self) -> String {
        first_non_empty(&[
            self.mail.smtp.password.clone(),
            std::env::var("NEOSONIA_SMTP_PASSWORD").unwrap_or_default(),
        ])
    }

    /// 脱敏后的 JSON（给 UI 显示）。
    pub fn redacted_json(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(k) = v.pointer_mut("/server/api_key") {
            *k = serde_json::json!(mask(&self.server.api_key));
        }
        if let Some(k) = v.pointer_mut("/mail/imap/password") {
            *k = serde_json::json!(mask(&self.mail.imap.password));
        }
        if let Some(k) = v.pointer_mut("/mail/smtp/password") {
            *k = serde_json::json!(mask(&self.mail.smtp.password));
        }
        // profiles 里的 api_key 也要脱敏。
        if let Some(profiles) = v
            .pointer_mut("/server/profiles")
            .and_then(|value| value.as_array_mut())
        {
            for (index, profile) in self.server.profiles.iter().enumerate() {
                if let Some(entry) = profiles.get_mut(index)
                    && let Some(key) = entry.pointer_mut("/api_key")
                {
                    *key = serde_json::json!(mask(&profile.api_key));
                }
            }
        }
        v
    }

    /// 白名单相关：大小写不敏感匹配。
    pub fn is_whitelisted(&self, addr: &str) -> bool {
        let a = addr.trim().to_ascii_lowercase();
        if a.is_empty() {
            return false;
        }
        self.mail.whitelist.iter().any(|w| {
            let w = w.trim().to_ascii_lowercase();
            if w.starts_with('@') {
                a.ends_with(&w)
            } else {
                a == w
            }
        })
    }

    pub fn add_whitelist(&mut self, addr: &str) -> bool {
        let a = addr.trim().to_ascii_lowercase();
        if a.is_empty() || self.is_whitelisted(&a) {
            return false;
        }
        self.mail.whitelist.push(a);
        true
    }

    pub fn remove_whitelist(&mut self, addr: &str) -> bool {
        let a = addr.trim().to_ascii_lowercase();
        let before = self.mail.whitelist.len();
        self.mail
            .whitelist
            .retain(|w| w.trim().to_ascii_lowercase() != a);
        self.mail.whitelist.len() != before
    }
}

fn expand(v: &mut String) {
    let raw = v.trim().to_string();
    let name = if let Some(rest) = raw.strip_prefix("env:") {
        Some(rest.to_string())
    } else if raw.starts_with("${") && raw.ends_with('}') {
        Some(raw[2..raw.len() - 1].to_string())
    } else {
        None
    };
    if let Some(name) = name {
        if let Ok(val) = std::env::var(&name) {
            *v = val;
        }
    }
}

fn first_non_empty(cands: &[String]) -> String {
    cands
        .iter()
        .find(|s| !s.trim().is_empty())
        .cloned()
        .unwrap_or_default()
}

fn mask(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.len() <= 8 {
        return "****".into();
    }
    format!("{}****{}", &t[..4], &t[t.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_roundtrip() {
        let s = Settings::default();
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.server.model, s.server.model);
        assert_eq!(back.ui.hotkey, "Alt+Space");
    }

    #[test]
    fn whitelist_matching() {
        let mut s = Settings::default();
        assert!(s.add_whitelist("Boss@Corp.com"));
        assert!(s.is_whitelisted("boss@corp.com"));
        assert!(!s.is_whitelisted("other@corp.com"));
        s.mail.whitelist.push("@vip.com".into());
        assert!(s.is_whitelisted("anyone@vip.com"));
        assert!(s.remove_whitelist("boss@corp.com"));
    }

    #[test]
    fn mask_hides_middle() {
        assert_eq!(mask("sk-1234567890abc"), "sk-1****0abc");
        assert_eq!(mask(""), "");
    }

    #[test]
    fn v06_defaults() {
        let s = Settings::default();
        // 截图组合键不再默认 arm "S"（Alt+Space 不应该截图）。
        assert_eq!(s.ui.screenshot_chord, "");
        assert_eq!(s.mail.provider, "custom");
        assert!(s.server.profiles.is_empty());
        assert!(s.server.active_profile.is_empty());
    }

    #[test]
    fn api_profiles_effective_server_and_crud() {
        let mut s = Settings::default();
        let default_model = s.server.model.clone();
        s.upsert_profile(ApiProfile {
            name: "DeepSeek 官方".into(),
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-v4-flash".into(),
            api_key: "sk-profile".into(),
            reasoning_effort: String::new(),
        })
        .unwrap();
        // 未激活时用默认字段。
        assert_eq!(s.effective_server().model, default_model);
        assert_eq!(s.server.profiles.len(), 1);

        // 激活后覆盖 base_url/model/key。
        assert!(s.set_active_profile("DeepSeek 官方"));
        let effective = s.effective_server();
        assert_eq!(effective.model, "deepseek-v4-flash");
        assert_eq!(effective.api_key, "sk-profile");
        assert_eq!(s.effective_api_key(), "sk-profile");

        // active 名字不存在时回退默认字段。
        s.server.active_profile = "不存在".into();
        assert_eq!(s.effective_server().model, default_model);

        // 覆盖同名 profile。
        let replaced = s
            .upsert_profile(ApiProfile {
                name: "DeepSeek 官方".into(),
                base_url: "https://api.deepseek.com".into(),
                model: "m2".into(),
                api_key: String::new(),
                reasoning_effort: "high".into(),
            })
            .unwrap();
        assert!(replaced);
        assert_eq!(s.server.profiles.len(), 1);
        // 空 api_key 不覆盖默认 key（沿用默认配置里的 key）。
        assert_eq!(s.effective_server().api_key, s.server.api_key);
        assert_eq!(s.effective_server().reasoning_effort, "high");

        // 删除 active → 回退默认并清空 active。
        assert!(s.set_active_profile("DeepSeek 官方"));
        assert!(s.remove_profile("DeepSeek 官方"));
        assert!(s.server.profiles.is_empty());
        assert!(s.server.active_profile.is_empty());
        assert_eq!(s.effective_server().model, default_model);
        assert!(!s.remove_profile("不存在"));

        // 空名字不能保存。
        assert!(s.upsert_profile(ApiProfile::default()).is_err());
        // 设置不存在名字失败且不改状态。
        assert!(!s.set_active_profile("nope"));
    }

    #[test]
    fn settings_profiles_roundtrip() {
        let mut s = Settings::default();
        s.mail.provider = "qq".into();
        s.upsert_profile(ApiProfile {
            name: "A".into(),
            base_url: "https://a.example.com".into(),
            model: "m-a".into(),
            api_key: "ka".into(),
            reasoning_effort: "low".into(),
        })
        .unwrap();
        s.set_active_profile("A");
        let text = toml::to_string_pretty(&s).unwrap();
        assert!(
            text.contains("[[server.profiles]]"),
            "应序列化成数组表: {text}"
        );
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.mail.provider, "qq");
        assert_eq!(back.server.profiles.len(), 1);
        assert_eq!(back.server.active_profile, "A");
        assert_eq!(back.effective_server().model, "m-a");
    }

    #[test]
    fn load_or_create_migrates_loopback_bind() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(dir.path());
        paths.ensure().unwrap();
        let mut s = Settings::default();
        s.remote.bind = "127.0.0.1".into();
        s.save(&paths.settings_file).unwrap();

        let loaded = Settings::load_or_create(&paths).unwrap();
        assert_eq!(loaded.remote.bind, "0.0.0.0", "回环绑定应迁移成 0.0.0.0");
        let text = std::fs::read_to_string(&paths.settings_file).unwrap();
        assert!(text.contains("0.0.0.0"), "迁移结果要写回配置文件");

        // 显式配置其它地址保持不变。
        let mut other = Settings::default();
        other.remote.bind = "192.168.1.10".into();
        other.save(&paths.settings_file).unwrap();
        let loaded = Settings::load_or_create(&paths).unwrap();
        assert_eq!(loaded.remote.bind, "192.168.1.10");
    }
}
