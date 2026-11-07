//! Agent 循环引擎：接收用户消息，跑「模型 → 工具 → 模型」直到收敛。
//!
//! 结构：
//! - [`AgentEngine`]：app 持有的句柄，线程安全，提交/取消/配置；
//! - [`EngineCore`]：共享状态 + 命令循环 + 一轮对话的执行；
//! - [`run_loop`]：真正的主循环（流式输出、工具执行、取消、审批）；
//! - [`AgentHost`] 实现：把配置/记忆/子代理能力暴露给工具。
//!
//! 所有 await 点都能被取消（`tokio::select!` 监听 watch 通道），长工具输出由工具自己
//! 通过 `ctx.emit_output` 流式推给 UI。
//!
//! OWNER: core-llm 队友。公共签名已与 app crate 对齐（app 只依赖下面这些），
//! 改动公共签名必须先通知 Lead。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::bail;
use async_trait::async_trait;
use base64::Engine as _;
use futures_util::FutureExt;
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::{AgentMode, ApprovalMode, Paths, Settings};
use crate::conversation::{ConversationStore, title_from};
use crate::llm::{LlmClient, LlmDelta, LlmRequest, Usage};
use crate::memory::MemoryStore;
use crate::model::{
    AgentEvent, AgentHost, ChatMessage, DeliverFile, ImagePart, MemoryApi, PlanStep, Role,
    SubagentRequest, ToolCall, ToolCtx, ToolOutcome, ToolSpec,
};
use crate::persona::PersonaStore;
use crate::provider_status::{self, ProviderStatus};
use crate::registry::{Tool, ToolRegistry};
use crate::review::{LlmRiskJudge, ReviewStore, RiskClassifier, RiskJudge, RiskVerdict};
use crate::skill::SkillStore;
use crate::subagent::{SubagentManager, SubagentOutcome};
use crate::util::{estimate_tokens, human_duration_ms, truncate_chars};

/// 事件回调：UI 把它转成自己的 Msg。
pub type EventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// 用户在审批弹窗里的选择。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApproveDecision {
    /// 允许并执行（2 分风险会记住同类操作）。
    Approve,
    /// 拒绝本次执行。
    Deny,
    /// 拒绝，并且 1 天内拒绝同类操作（仅 4 分高风险时 UI 才提供）。
    DenyClass,
}

impl From<bool> for ApproveDecision {
    fn from(ok: bool) -> Self {
        if ok {
            ApproveDecision::Approve
        } else {
            ApproveDecision::Deny
        }
    }
}

impl ApproveDecision {
    pub fn is_approved(self) -> bool {
        matches!(self, ApproveDecision::Approve)
    }
}

/// 启动引擎需要的依赖。
#[derive(Clone)]
pub struct EngineConfig {
    pub settings: Arc<RwLock<Settings>>,
    pub paths: Paths,
    pub registry: ToolRegistry,
    pub memory: Arc<MemoryStore>,
    pub personas: PersonaStore,
    /// 对话历史库（第二阶段的会话持久化）。
    pub conversations: Arc<ConversationStore>,
    /// 技能仓库（`skills/<id>/SKILL.md`）。
    pub skills: SkillStore,
}

/// 等待危险操作确认的最长时间；超时按拒绝处理。
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);

/// 历史消息最多保留多少条（超出后从头部丢弃，避免内存无限增长）。
const HISTORY_KEEP_MAX: usize = 400;

/// 引擎内部的命令。
enum EngineCmd {
    Submit {
        turn_id: String,
        text: String,
        images: Vec<ImagePart>,
        /// 轮次序号：取消按序号生效，旧 cancel 不会吞掉之后提交的新任务。
        generation: u64,
    },
    RefreshProviderStatus,
    Shutdown,
}

/// UI 侧持有的引擎句柄。可 clone，线程安全。
pub struct AgentEngine {
    core: Arc<EngineCore>,
    cmds: mpsc::UnboundedSender<EngineCmd>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// app 没有 tokio runtime 时，引擎自建的运行时线程句柄（不 join，随进程退出）。
    own_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl AgentEngine {
    /// 启动后台循环任务。
    ///
    /// 优先复用调用方当前的 tokio runtime；如果当前线程没有 runtime（例如同步的
    /// fenestra GUI 主线程），会自动创建一个 2 worker 的多线程 runtime 线程。
    pub fn start(cfg: EngineConfig, sink: EventSink) -> anyhow::Result<Self> {
        let core = Arc::new(EngineCore::new(cfg, sink));

        // 内置工具由 core 自己注册（app 只注册 neosonia-tools / neosonia-mail 的工具）。
        let settings = core.settings_snapshot();
        crate::memory::register_builtin_tools(&core.cfg.registry);
        crate::subagent::register_builtin_tools(&core.cfg.registry, Arc::downgrade(&core));
        crate::skill::register_builtin_tools(&core.cfg.registry, core.cfg.skills.clone());
        register_builtin_tools(&core.cfg.registry, Arc::downgrade(&core));
        // 技能安装后自动做模式分类需要 settings。
        core.cfg.skills.attach_settings(core.cfg.settings.clone());
        core.cfg.memory.set_max_items(settings.memory.max_items);

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<EngineCmd>();
        let mut task = None;
        let mut own_thread = None;

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let loop_core = core.clone();
                task = Some(handle.spawn(async move {
                    loop_core.command_loop(cmd_rx).await;
                }));
            }
            Err(_) => {
                // 同步 GUI 主线程：自建 runtime 线程，避免要求 app 层改结构。
                let loop_core = core.clone();
                let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
                let join = std::thread::Builder::new()
                    .name("neosonia-agent".to_string())
                    .spawn(move || {
                        let runtime = tokio::runtime::Builder::new_multi_thread()
                            .worker_threads(2)
                            .thread_name("neosonia-agent-worker")
                            .enable_all()
                            .build();
                        match runtime {
                            Ok(runtime) => {
                                let _ = ready_tx.send(Ok(()));
                                runtime.block_on(loop_core.command_loop(cmd_rx));
                            }
                            Err(e) => {
                                let _ = ready_tx.send(Err(anyhow::anyhow!(
                                    "创建 agent 运行时失败：{e}。请检查系统资源/权限后重启应用。"
                                )));
                            }
                        }
                    })
                    .map_err(|e| anyhow::anyhow!("启动 agent 线程失败：{e}"))?;
                match ready_rx.recv() {
                    Ok(Ok(())) => own_thread = Some(join),
                    Ok(Err(e)) => {
                        return Err(e);
                    }
                    Err(_) => bail!("agent 线程启动后立即退出（可能是运行时创建失败或权限受限）"),
                }
            }
        }

        Ok(Self {
            core,
            cmds: cmd_tx,
            task: Mutex::new(task),
            own_thread: Mutex::new(own_thread),
        })
    }

    /// 提交一条用户消息；返回 turn_id。
    pub fn submit(&self, text: impl Into<String>) -> String {
        self.submit_with_images(text.into(), Vec::new())
    }

    /// 提交一条带图片的用户消息（视觉输入）。语义同 [`submit`]，只是首条 user 消息
    /// 用 `ChatMessage::user_with_images` 构造；`images` 为空时等价 `submit`。
    pub fn submit_with_images(&self, text: String, images: Vec<ImagePart>) -> String {
        let turn_id = crate::util::short_id();
        let text = text.trim().to_string();
        if text.is_empty() && images.is_empty() {
            self.core.emit(AgentEvent::Error(
                "消息内容为空，未提交。请输入要让我做的事。".to_string(),
            ));
            return turn_id;
        }
        self.core.busy.store(true, Ordering::SeqCst);
        let generation = self.core.turn_generation.fetch_add(1, Ordering::SeqCst) + 1;
        if self
            .cmds
            .send(EngineCmd::Submit {
                turn_id: turn_id.clone(),
                text,
                images,
                generation,
            })
            .is_err()
        {
            self.core.busy.store(false, Ordering::SeqCst);
            self.core.emit(AgentEvent::Error(
                "agent 引擎已关闭，无法提交新消息。请重启应用。".to_string(),
            ));
        }
        turn_id
    }

    pub fn cancel(&self) {
        if !self.core.busy.load(Ordering::SeqCst) {
            return;
        }
        // 记录"取消到当前已提交的最大序号"：正在跑/排队的轮次会收到取消，
        // 之后新提交的轮次序号更大，不会被这条取消误伤。
        let generation = self.core.turn_generation.load(Ordering::SeqCst);
        self.core
            .cancel_generation
            .fetch_max(generation, Ordering::SeqCst);
        self.core.cancel_flag.store(true, Ordering::SeqCst);
        self.core.cancel_tx.send_replace(true);
        self.core.emit(AgentEvent::Phase("正在取消...".to_string()));
    }

    /// 清空当前历史（等价于开始新对话），旧对话已持久化在历史库里。
    pub fn reset(&self) {
        self.new_conversation();
    }

    /// 开始一段新对话：只清空内存历史，下一轮自动建新会话。
    pub fn new_conversation(&self) {
        self.core.history.write().clear();
        *self.core.conversation_id.lock() = None;
        *self.core.context_cache.lock() = None;
        *self.core.computer_used_conv.lock() = None;
        *self.core.plan_cache.lock() = Vec::new();
        // 会话隔离：新会话从空计划开始；如果之前是 agent 隐藏了窗口，恢复显示。
        self.core
            .emit(AgentEvent::PlanUpdated { steps: Vec::new() });
        self.core.restore_window_if_hidden("开始新对话，恢复窗口");
        self.core.emit(AgentEvent::Notice(
            "已开始新对话（上一段对话已保存到历史记录）。".to_string(),
        ));
    }

    /// 当前会话 id（还没有落库的用户消息时为 None）。
    pub fn conversation_id(&self) -> Option<String> {
        self.core.conversation_id.lock().clone()
    }

    /// 载入一段历史会话：清空内存历史并从库里恢复消息。
    pub fn load_conversation(&self, id: String) {
        let id = id.trim().to_string();
        if id.is_empty() {
            self.core
                .emit(AgentEvent::Error("会话 id 不能为空。".to_string()));
            return;
        }
        match self.core.cfg.conversations.messages(&id, 0) {
            Ok(messages) => {
                let count = messages.len();
                *self.core.history.write() = messages;
                // 历史里可能有被中断的 tool 调用（assistant 带 tool_calls 但缺结果），
                // 补齐后再发请求，避免 API 400。
                repair_history(&self.core.history);
                *self.core.conversation_id.lock() = Some(id.clone());
                *self.core.context_cache.lock() = None;
                *self.core.computer_used_conv.lock() = None;
                // 切换会话时只显示该会话的计划。
                let plan = self.core.cfg.conversations.plan(&id).unwrap_or_default();
                *self.core.plan_cache.lock() = plan.clone();
                self.core.emit(AgentEvent::PlanUpdated { steps: plan });
                self.core.emit(AgentEvent::Notice(format!(
                    "已载入历史会话（{count} 条消息）。"
                )));
            }
            Err(e) => self
                .core
                .emit(AgentEvent::Error(format!("载入历史会话失败：{e}"))),
        }
    }

    pub fn conversations(&self) -> Arc<ConversationStore> {
        self.core.cfg.conversations.clone()
    }

    pub fn skills(&self) -> SkillStore {
        self.core.cfg.skills.clone()
    }

    /// 审查记忆库（设置页展示/清理授权与拒绝规则）。
    pub fn review_store(&self) -> Arc<ReviewStore> {
        self.core.review_store.clone()
    }

    /// 最近一次模型提供商状态（启动后 0-60s 内可能为 None）。
    pub fn current_provider_status(&self) -> Option<ProviderStatus> {
        self.core.current_provider_status()
    }

    /// 顶栏小球便捷接口：(provider, 中文状态, indicator 颜色)。
    pub fn provider_status_label(&self) -> Option<(String, String, String)> {
        self.current_provider_status().map(|status| status.short())
    }

    /// 立即后台刷新一次提供商状态（60s 缓存；变化时 emit 事件）。
    pub fn refresh_provider_status(&self) {
        let _ = self.cmds.send(EngineCmd::RefreshProviderStatus);
    }

    /// 切换提示词模式（等价于模型调用 `switch_mode` 工具，供设置页直接调用）。
    pub fn set_mode(&self, mode: AgentMode) {
        {
            self.core.cfg.settings.write().agent.mode = mode;
        }
        self.core.emit(AgentEvent::ModeChanged {
            mode: mode.id().to_string(),
            label: mode.label().to_string(),
        });
    }

    /// app 注入窗口显示/隐藏回调（`set_window_visibility` 工具与自动恢复用）。
    pub fn set_window_visibility_hook(&self, hook: Arc<dyn Fn(bool) + Send + Sync>) {
        *self.core.window_visibility_hook.lock() = Some(hook);
    }

    /// 当前会话计划（app 切会话/启动时展示用）。
    pub fn plan(&self) -> Vec<crate::model::PlanStep> {
        self.core.current_plan()
    }

    /// 新增/覆盖 API profile（写回 Settings，不自动切换）。
    pub fn upsert_server_profile(
        &self,
        profile: crate::config::ApiProfile,
    ) -> anyhow::Result<bool> {
        let replaced = self.core.cfg.settings.write().upsert_profile(profile)?;
        Ok(replaced)
    }

    /// 删除 API profile；如果删的是当前 active 会回退默认配置。
    pub fn remove_server_profile(&self, name: &str) -> bool {
        let removed = self.core.cfg.settings.write().remove_profile(name);
        if removed {
            self.core.refresh_llm();
        }
        removed
    }

    /// 一键切换 API profile；名字不存在返回 false（不改变当前配置）。
    pub fn set_active_server_profile(&self, name: &str) -> bool {
        let switched = self.core.cfg.settings.write().set_active_profile(name);
        if switched {
            self.core.refresh_llm();
            let label = if name.trim().is_empty() {
                "默认配置".to_string()
            } else {
                name.trim().to_string()
            };
            self.core
                .emit(AgentEvent::Notice(format!("已切换 API 配置：{label}")));
        }
        switched
    }

    /// 重新扫描技能目录（UI 里安装/卸载技能后调用）。
    pub fn reload_skills(&self) {
        self.core.cfg.skills.reload();
        let count = self.core.cfg.skills.list().len();
        self.core.emit(AgentEvent::Notice(format!(
            "技能已重新扫描：{count} 个可用。"
        )));
    }

    pub fn history(&self) -> Vec<ChatMessage> {
        self.core.history.read().clone()
    }

    pub fn is_busy(&self) -> bool {
        self.core.busy.load(Ordering::SeqCst)
    }

    pub fn set_persona(&self, id: &str) {
        let id = id.trim();
        // 先刷新磁盘上的人设文件，再切换。
        self.core.cfg.personas.reload();
        self.core.cfg.settings.write().agent.persona = id.to_string();
        let name = self.core.cfg.personas.get(id).name;
        self.core
            .emit(AgentEvent::Notice(format!("已切换人设：{name}")));
    }

    /// 配置（服务器/密钥/模型/工具开关）改完后调用，重建 LLM 客户端。
    pub fn reload_config(&self) {
        self.core.refresh_llm();
        let settings = self.core.settings_snapshot();
        self.core
            .cfg
            .memory
            .set_max_items(settings.memory.max_items);
        self.core
            .emit(AgentEvent::Notice("配置已重新加载。".to_string()));
    }

    /// 危险操作确认结果。
    ///
    /// 兼容旧调用（`approve(call_id, true/false)`，靠 `From<bool>`），
    /// 新 UI 用 `ApproveDecision::{Approve, Deny, DenyClass}`。
    pub fn approve(&self, call_id: &str, decision: impl Into<ApproveDecision>) {
        self.core.resolve_approval(call_id, decision.into());
    }

    pub fn shutdown(&self) {
        self.core.cancel_flag.store(true, Ordering::SeqCst);
        self.core.cancel_tx.send_replace(true);
        self.core.subagents.abort_all();
        let _ = self.cmds.send(EngineCmd::Shutdown);
        if let Some(task) = self.task.lock().take() {
            task.abort();
        }
        // 自建运行时线程：发完 Shutdown 后它会自己退出；这里 detach，避免阻塞 UI 退出。
        let _thread = self.own_thread.lock().take();
    }

    pub fn settings(&self) -> Arc<RwLock<Settings>> {
        self.core.cfg.settings.clone()
    }

    pub fn memory(&self) -> Arc<MemoryStore> {
        self.core.cfg.memory.clone()
    }

    pub fn personas(&self) -> PersonaStore {
        self.core.cfg.personas.clone()
    }

    pub fn registry(&self) -> ToolRegistry {
        self.core.cfg.registry.clone()
    }

    pub fn paths(&self) -> Paths {
        self.core.cfg.paths.clone()
    }

    /// 供工具/测试使用的 LLM 客户端（当前路径走 `EngineCore::llm`）。
    #[allow(dead_code)]
    pub(crate) fn llm(&self) -> LlmClient {
        self.core.llm()
    }

    /// 供工具使用的记忆接口（`AgentHost::memory` 已覆盖绝大多数场景）。
    #[allow(dead_code)]
    pub(crate) fn memory_api(&self) -> Arc<dyn MemoryApi> {
        let memory: Arc<dyn MemoryApi> = self.core.cfg.memory.clone();
        memory
    }
}

// ============================================================
// 引擎核心
// ============================================================

/// 引擎共享状态。`pub(crate)` 是因为 subagent 模块需要拿它来启动任务。
pub(crate) struct EngineCore {
    pub(crate) cfg: EngineConfig,
    sink: EventSink,
    busy: Arc<AtomicBool>,
    history: Arc<RwLock<Vec<ChatMessage>>>,
    /// 当前轮次是否已取消（工具轮询用，`AtomicBool` 读取无锁）。
    pub(crate) cancel_flag: Arc<AtomicBool>,
    /// 取消通知（异步等待用，`tokio::select!` 里监听）。
    pub(crate) cancel_tx: watch::Sender<bool>,
    pub(crate) subagents: Arc<SubagentManager>,
    /// 等待用户确认的危险操作：call_id -> oneshot 发送端。
    approvals: Arc<Mutex<HashMap<String, oneshot::Sender<ApproveDecision>>>>,
    /// LLM 客户端缓存（配置变更时由 `reload_config` 清空）。
    llm_cache: Mutex<Option<LlmClient>>,
    /// 当前会话 id（对话历史库里的 conversations.id）。
    conversation_id: Mutex<Option<String>>,
    /// 授权/拒绝记忆（data/review_state.json）。
    review_store: Arc<ReviewStore>,
    /// 风险评分器（默认走真实模型；单测注入假实现）。
    judge: Arc<dyn RiskJudge>,
    /// 上下文压缩缓存（同会话同前缀时复用摘要）。
    context_cache: Mutex<Option<ContextCache>>,
    /// 已调用过计算机类工具的会话 id（本会话后续每轮都展开完整计算机操作段）。
    computer_used_conv: Mutex<Option<String>>,
    /// 当前会话的 Work Plan 缓存（工具更新后同时落库）。
    plan_cache: Mutex<Vec<crate::model::PlanStep>>,
    /// app 注入的窗口显示/隐藏回调。
    window_visibility_hook: Mutex<Option<Arc<dyn Fn(bool) + Send + Sync>>>,
    /// 是否是 agent 主动隐藏了窗口（回合结束要自动恢复）。
    window_hidden_by_agent: AtomicBool,
    /// 本会话累计启动过的子代理数量。
    subagent_total: AtomicU32,
    /// 已提交轮次的序号（取消按 generation 生效，避免旧 cancel 吞掉新任务）。
    turn_generation: AtomicU64,
    /// 已请求取消到的轮次序号。
    cancel_generation: AtomicU64,
    /// 最近一次模型提供商状态（给 UI/远程查询）。
    provider_status: Mutex<Option<ProviderStatus>>,
}

impl EngineCore {
    fn new(cfg: EngineConfig, sink: EventSink) -> Self {
        Self::new_with_judge(cfg, sink, Arc::new(LlmRiskJudge))
    }

    fn new_with_judge(cfg: EngineConfig, sink: EventSink, judge: Arc<dyn RiskJudge>) -> Self {
        let (cancel_tx, _) = watch::channel(false);
        let review_path = cfg.paths.data_dir.join("review_state.json");
        let review_store = Arc::new(ReviewStore::open(&review_path));
        Self {
            cfg,
            sink,
            busy: Arc::new(AtomicBool::new(false)),
            history: Arc::new(RwLock::new(Vec::new())),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            cancel_tx,
            subagents: Arc::new(SubagentManager::new()),
            approvals: Arc::new(Mutex::new(HashMap::new())),
            llm_cache: Mutex::new(None),
            conversation_id: Mutex::new(None),
            review_store,
            judge,
            context_cache: Mutex::new(None),
            computer_used_conv: Mutex::new(None),
            plan_cache: Mutex::new(Vec::new()),
            window_visibility_hook: Mutex::new(None),
            window_hidden_by_agent: AtomicBool::new(false),
            subagent_total: AtomicU32::new(0),
            turn_generation: AtomicU64::new(0),
            cancel_generation: AtomicU64::new(0),
            provider_status: Mutex::new(None),
        }
    }

    pub(crate) fn emit(&self, event: AgentEvent) {
        // 事件回调属于 UI 层；这里兜底隔离 panic，避免 UI 的 bug 把 agent 线程带崩。
        let sink = self.sink.clone();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink(event)));
        if outcome.is_err() {
            tracing::error!("事件回调（EventSink）发生 panic（已隔离，本轮继续）");
        }
    }

    /// 解析一次等待中的审批（UI/测试都走这里）。
    pub(crate) fn resolve_approval(&self, call_id: &str, decision: ApproveDecision) {
        if let Some(tx) = self.approvals.lock().remove(call_id) {
            let _ = tx.send(decision);
            tracing::debug!(call_id, ?decision, "approval resolved");
        }
    }

    /// 本会话是否已经调用过计算机类工具（会话内一旦用过就永久展开完整提示词）。
    pub(crate) fn computer_used_in_conversation(&self, conversation_id: Option<&str>) -> bool {
        let Some(conversation_id) = conversation_id.filter(|id| !id.trim().is_empty()) else {
            return false;
        };
        self.computer_used_conv.lock().as_deref() == Some(conversation_id)
    }

    /// 记录本会话用过计算机类工具。
    pub(crate) fn mark_computer_used(&self, conversation_id: Option<&str>) {
        if let Some(conversation_id) = conversation_id.filter(|id| !id.trim().is_empty()) {
            *self.computer_used_conv.lock() = Some(conversation_id.to_string());
        }
    }

    /// 请求 UI 隐藏/显示窗口；返回是否有 app 注入的回调。
    pub(crate) fn set_window_visibility(&self, hidden: bool, reason: &str) -> bool {
        let hook = self.window_visibility_hook.lock().clone();
        self.emit(AgentEvent::WindowVisibilityRequested {
            hidden,
            reason: reason.to_string(),
        });
        if hidden {
            self.window_hidden_by_agent.store(true, Ordering::SeqCst);
        } else {
            self.window_hidden_by_agent.store(false, Ordering::SeqCst);
        }
        match hook {
            Some(hook) => {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(hidden))).is_err()
                {
                    tracing::error!("窗口显示/隐藏回调发生 panic（已隔离）");
                }
                true
            }
            None => false,
        }
    }

    /// 回合结束/需要审批时，如果 agent 隐藏过窗口就恢复显示。
    pub(crate) fn restore_window_if_hidden(&self, reason: &str) {
        if self.window_hidden_by_agent.swap(false, Ordering::SeqCst) {
            self.set_window_visibility(false, reason);
        }
    }

    /// (运行中子代理数, 本会话累计子代理数)
    pub(crate) fn subagent_counts(&self) -> (u32, u32) {
        (
            self.subagents.running_count(),
            self.subagent_total.load(Ordering::SeqCst),
        )
    }

    /// 保存/广播当前 Work Plan；有会话 id 时同时落库。
    pub(crate) fn apply_plan(
        &self,
        conversation_id: Option<&str>,
        steps: Vec<crate::model::PlanStep>,
    ) {
        *self.plan_cache.lock() = steps.clone();
        if let Some(conversation_id) = conversation_id.filter(|id| !id.trim().is_empty()) {
            if let Err(error) = self.cfg.conversations.save_plan(conversation_id, &steps) {
                tracing::warn!(error = %error, "保存会话计划失败");
            }
        }
        self.emit(AgentEvent::PlanUpdated { steps });
    }

    /// 当前会话计划（内存缓存优先）。
    pub(crate) fn current_plan(&self) -> Vec<crate::model::PlanStep> {
        self.plan_cache.lock().clone()
    }

    /// 拉取/刷新模型提供商状态；变化时 emit `ProviderStatusChanged`。
    pub(crate) async fn check_provider_status(&self) {
        let settings = self.settings_snapshot();
        match provider_status::current_status(&settings).await {
            Some(status) => self.publish_provider_status(status),
            None => {
                // 未知厂商（自建/Ollama 等）：不显示小球。
            }
        }
    }

    fn publish_provider_status(&self, status: ProviderStatus) {
        let changed = {
            let mut guard = self.provider_status.lock();
            let changed = guard
                .as_ref()
                .map(|old| old.provider != status.provider || old.indicator != status.indicator)
                .unwrap_or(true);
            *guard = Some(status.clone());
            changed
        };
        if changed {
            self.emit(AgentEvent::ProviderStatusChanged {
                provider: status.provider.clone(),
                indicator: status.indicator.clone(),
                label: status.label.clone(),
                url: status.url.clone(),
                checked_at_ms: status.checked_at_ms,
            });
        }
    }

    /// 最近一次已知的提供商状态（启动后 0-60s 内可能为 None）。
    pub(crate) fn current_provider_status(&self) -> Option<ProviderStatus> {
        self.provider_status.lock().clone()
    }

    pub(crate) fn settings_snapshot(&self) -> Settings {
        self.cfg.settings.read().clone()
    }

    pub(crate) fn subagents(&self) -> Arc<SubagentManager> {
        self.subagents.clone()
    }

    pub(crate) fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel_flag.clone()
    }

    pub(crate) fn cancel_subscribe(&self) -> watch::Receiver<bool> {
        self.cancel_tx.subscribe()
    }

    fn refresh_llm(&self) {
        *self.llm_cache.lock() = None;
    }

    /// 取（或按当前配置重建）LLM 客户端。热改配置后靠 `refresh_llm` 失效。
    fn llm(&self) -> LlmClient {
        let mut cache = self.llm_cache.lock();
        if let Some(client) = cache.as_ref() {
            return client.clone();
        }
        let settings = self.settings_snapshot();
        // 多 API profiles：当前生效的可能是 active_profile 覆盖后的配置。
        let client = LlmClient::new(settings.effective_server(), settings.effective_api_key());
        *cache = Some(client.clone());
        client
    }

    fn workspace(&self) -> PathBuf {
        let settings = self.settings_snapshot();
        let configured = PathBuf::from(settings.agent.workspace_dir.trim());
        if configured.as_os_str().is_empty() {
            self.cfg.paths.workspace.clone()
        } else if configured.is_absolute() {
            configured
        } else {
            self.cfg.paths.workspace.join(configured)
        }
    }

    async fn command_loop(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<EngineCmd>) {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                EngineCmd::Submit {
                    turn_id,
                    text,
                    images,
                    generation,
                } => {
                    self.busy.store(true, Ordering::SeqCst);
                    // 最外层兜底：任何未预期的 panic 都不能弄死引擎循环。
                    let turn = std::panic::AssertUnwindSafe(self.run_turn(
                        turn_id.clone(),
                        text,
                        images,
                        generation,
                    ));
                    if turn.catch_unwind().await.is_err() {
                        tracing::error!(turn_id = %turn_id, "agent turn panicked");
                        self.emit(AgentEvent::Error(format!(
                            "内部错误：处理这一轮时发生了 panic（已隔离，引擎仍在运行）。turn={turn_id}，\
                             请把刚才的操作和日志反馈给开发者。"
                        )));
                        repair_history(&self.history);
                        self.cancel_flag.store(false, Ordering::SeqCst);
                        self.cancel_tx.send_replace(false);
                        self.emit(AgentEvent::TurnFinished {
                            turn_id,
                            cancelled: false,
                        });
                    }
                    if rx.is_empty() {
                        self.busy.store(false, Ordering::SeqCst);
                    }
                }
                EngineCmd::RefreshProviderStatus => {
                    self.check_provider_status().await;
                }
                EngineCmd::Shutdown => break,
            }
        }
        self.busy.store(false, Ordering::SeqCst);
    }

    /// 跑完一个用户轮次。错误通过事件汇报，不向上抛（循环不能因为一次失败就死掉）。
    async fn run_turn(
        self: &Arc<Self>,
        turn_id: String,
        user_text: String,
        user_images: Vec<ImagePart>,
        generation: u64,
    ) {
        // 只有"被取消的排队轮次"才丢弃；submit 会先分配更大的 generation，
        // 所以"取消上一个任务后立刻发新任务"不会被旧的 cancel 标志吞掉。
        let pre_cancelled =
            turn_pre_cancelled(self.cancel_generation.load(Ordering::SeqCst), generation);
        // 新一轮开始：清掉底层取消标志与通知。
        self.cancel_flag.store(false, Ordering::SeqCst);
        self.cancel_tx.send_replace(false);

        self.emit(AgentEvent::TurnStarted {
            turn_id: turn_id.clone(),
            user_text: user_text.clone(),
        });
        if pre_cancelled {
            self.emit(AgentEvent::TurnFinished {
                turn_id,
                cancelled: true,
            });
            return;
        }
        // 后台刷新一次提供商状态（60s 缓存，变化才 emit）。
        {
            let core = self.clone();
            tokio::spawn(async move { core.check_provider_status().await });
        }

        let settings = self.settings_snapshot();

        // 确保会话存在；首次（或新会话）时通知 UI。
        let persona_id = settings.agent.persona.clone();
        let previous_conversation = self.conversation_id.lock().clone();
        let conversation_id = match self.cfg.conversations.ensure(
            previous_conversation.as_deref(),
            &user_text,
            &persona_id,
        ) {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(error = %e, "对话历史入库失败（本轮继续，只是不持久化）");
                None
            }
        };
        if let Some(id) = &conversation_id {
            if previous_conversation.as_deref() != Some(id.as_str()) {
                self.emit(AgentEvent::ConversationStarted {
                    conversation_id: id.clone(),
                    title: title_from(&user_text),
                });
            }
            *self.conversation_id.lock() = Some(id.clone());
        }

        // 对话记录入库（与 UI 消息分开存，供压缩/检索）。
        if settings.memory.enabled && !user_text.trim().is_empty() {
            if let Err(e) = self
                .cfg
                .memory
                .log_conversation("user", &user_text, &turn_id)
            {
                tracing::warn!(error = %e, "记录用户对话失败");
            }
        }

        let tools = safe_tool_specs(&self.cfg.registry, None);
        let host: Arc<dyn AgentHost> = Arc::new(EngineHost {
            core: self.clone(),
            cancel: self.cancel_flag.clone(),
            parent_cancel: None,
            is_subagent: false,
            subagent_id: None,
        });

        let setup = TurnSetup {
            mode: LoopMode::Main {
                turn_id: turn_id.clone(),
            },
            persona_id: persona_id.clone(),
            user_text: user_text.clone(),
            user_images,
            tools,
            tool_allow: Vec::new(),
            forbid_tools: Vec::new(),
            max_iterations: settings.agent.max_iterations.max(1),
            host,
            cancel_rx: self.cancel_subscribe(),
            parent_rx: None,
            history: self.history.clone(),
            conversation_id: conversation_id.clone(),
        };

        let result = run_loop(self, setup).await;
        tracing::debug!(
            turn_id = %turn_id,
            iterations = result.iterations,
            tool_calls = result.tool_calls,
            cancelled = result.cancelled,
            "turn finished"
        );

        trim_history_tail(&self.history, HISTORY_KEEP_MAX);

        if !result.text.trim().is_empty() && settings.memory.enabled {
            if let Err(e) = self
                .cfg
                .memory
                .log_conversation("assistant", &result.text, &turn_id)
            {
                tracing::warn!(error = %e, "记录助手对话失败");
            }
        }
        if let Some(error) = &result.error {
            tracing::warn!(turn_id = %turn_id, error = %error, "轮次以错误结束");
        }
        self.auto_remember(&user_text);

        // 回合结束（完成/出错/取消）：如果 agent 隐藏过窗口，恢复显示。
        self.restore_window_if_hidden("任务结束，恢复窗口");

        // 取消时单独发 Cancelled，UI 可以立即显示"已停止"（TurnFinished 仍保留兼容）。
        if result.cancelled {
            self.emit(AgentEvent::Cancelled {
                turn_id: turn_id.clone(),
            });
        }
        self.emit(AgentEvent::TurnFinished {
            turn_id,
            cancelled: result.cancelled,
        });
    }

    /// 从用户原话里识别值得长期记住的偏好，自动入库（不额外调用模型，零成本）。
    fn auto_remember(&self, user_text: &str) {
        let settings = self.settings_snapshot();
        if !settings.agent.auto_memory || !settings.memory.enabled {
            return;
        }
        let text = user_text.trim();
        if text.chars().count() < 4 {
            return;
        }
        let lower = text.to_lowercase();
        const MARKERS: [&str; 12] = [
            "记住",
            "以后",
            "总是",
            "我喜欢",
            "我不喜欢",
            "我的名字",
            "我叫",
            "我希望",
            "我习惯",
            "remember",
            "always",
            "prefer",
        ];
        if !MARKERS.iter().any(|marker| lower.contains(marker)) {
            return;
        }
        let content = truncate_chars(text, 240);
        match self.cfg.memory.remember_kind(
            "preference",
            &content,
            &["auto".to_string()],
            0.7,
            "auto",
        ) {
            Ok(_) => self.emit(AgentEvent::MemoryChanged {
                action: "remember".into(),
                content,
            }),
            Err(e) => tracing::warn!(error = %e, "自动写入长期记忆失败"),
        }
    }

    /// 组装系统提示：按 [`AgentMode`] 做提示词隔离。
    ///
    /// - daily：人设正文 + 长期记忆 + 文件/搜索提示 + 基础安全；计算机操作段按需展开；
    /// - coding：不注入人设/记忆，保留基础安全 + 编程规范 + 文件/命令提示；计算机按需展开；
    /// - computer_use：不注入人设，计算机操作 + 文件/命令全部展开；
    /// - `computer_expanded`：本轮已经用过计算机类工具（daily/coding 时下一次请求追加
    ///   「## 计算机操作」，同一轮只展开一次）。
    fn build_system_prompt(
        &self,
        user_text: &str,
        persona_id: &str,
        subagent: bool,
        computer_expanded: bool,
    ) -> String {
        let settings = self.settings_snapshot();
        let mode = settings.agent.mode;
        let mut prompt = String::new();

        // 1) 身份/人设：只有 daily 注入人设正文；coding/computer_use 做提示词隔离。
        if mode == AgentMode::Daily {
            let persona = self.cfg.personas.get(persona_id);
            let description = persona.description.trim().trim_end_matches('。');
            if description.is_empty() {
                prompt.push_str(&format!("你是 {}。\n\n", persona.name));
            } else {
                prompt.push_str(&format!("你是 {}，{}。\n\n", persona.name, description));
            }
            let persona_prompt = persona.system_prompt.trim();
            if !persona_prompt.is_empty() {
                prompt.push_str(persona_prompt);
                prompt.push_str("\n\n");
            }
        } else {
            let focus = match mode {
                AgentMode::Coding => "专注写代码、调试与验证，改动尽量小且可回滚。",
                _ => "专注操作桌面应用：截图、鼠标键盘、UI 自动化与浏览器调试。",
            };
            prompt.push_str(&format!(
                "你是 NeoSonia 的{}模式，{}该模式不注入人设与长期记忆。\n\n",
                mode.label(),
                focus
            ));
        }

        if subagent {
            prompt.push_str(
                "【子代理模式】你是父代理派出的子代理，只负责完成下面这一件事。\n\
                 - 不要反问用户，不要寒暄，不要扩大任务范围；\n\
                 - 需要时正常调用工具；\n\
                 - 完成后用 3-5 行汇报：做了什么、关键发现、产出（文件路径/命令结果/结论）。\n\n",
            );
        }

        // 2) 运行环境（所有模式保留）。
        prompt.push_str("## 运行环境\n");
        prompt.push_str(&format!("- 当前模式：{}（{}）\n", mode.label(), mode.id()));
        prompt.push_str(&format!("- 当前时间：{}\n", now_local_string()));
        prompt.push_str(&format!(
            "- 平台：{} / {}\n",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        prompt.push_str(&format!("- 工作目录：{}\n", self.workspace().display()));
        let tool_names = self.cfg.registry.names();
        if tool_names.is_empty() {
            prompt.push_str("- 可用工具：无（只能纯文本回答）\n");
        } else {
            prompt.push_str(&format!(
                "- 可用工具（{} 个）：{}\n",
                tool_names.len(),
                tool_names.join(", ")
            ));
        }

        // 3) 基础安全提示（不随模式消失）。
        prompt.push_str(
            "\n## 基础安全\n\
             - 执行明显破坏性、越权或不可逆的操作前，先用一句话说明影响；用户明确要求时才执行。\n\
             - 点击危险按钮（删除、发送、支付）前先确认。\n\
             - 不要泄露 API key、密码、令牌等密钥信息；不确定的事先用工具确认，不要编造。\n\
             - 回答用中文，简洁直接；不要 emoji。\n",
        );

        // 3.5) 模式切换是硬性要求（三种模式共有）。
        prompt.push_str(
            "\n## 模式切换（硬性要求）\n\
             - 写代码/改代码前，必须先调用 switch_mode {\"mode\":\"coding\"}，再动手改；不要停留在 daily 模式直接改代码。\n\
             - 要做界面操作、截图、鼠标键盘、UIA/CDP 或窗口相关操作前，必须先调用 switch_mode {\"mode\":\"computer_use\"}。\n\
             - **用户问天气、日程、生活常识、闲聊、新闻/资讯等日常内容时：必须先 switch_mode {\"mode\":\"daily\"} 切回日常模式，再回答；禁止在 coding/computer_use 模式下直接回答日常问题。**\n\
             - 任务完成或回到普通问答时，可以 switch_mode {\"mode\":\"daily\"} 切回日常模式。\n",
        );

        // 4) 模式专属内容。
        match mode {
            AgentMode::Daily => {
                prompt.push_str(
                    "\n## 文件与搜索\n\
                     - 读文件前先想清楚要读哪一段；大文件用 start_line/end_line 分段读。\n\
                     - 写/改文件默认限制在工作目录内；需要外部路径先说明理由。\n\
                     - 查找资料优先 web_search，再 web_fetch 精读；本地文件用 search_files / list_dir。\n\
                     - 用户邮件/附件里的内容不要外发到网络。\n",
                );
                // daily 默认展开计算机操作基础版；本会话用过一次后再展开完整版。
                if !computer_expanded {
                    prompt.push_str(COMPUTER_OPERATIONS_BASIC_SECTION);
                }
                // 固定记忆 + 相关长期记忆（daily 才注入）。
                if let Ok(blocks) = self.cfg.memory.blocks() {
                    let mut section = String::from("\n## 固定记忆\n");
                    let mut has_content = false;
                    for (key, value) in &blocks {
                        let value = value.trim();
                        if value.is_empty() {
                            continue;
                        }
                        section.push_str(&format!("- {key}: {}\n", truncate_chars(value, 400)));
                        has_content = true;
                    }
                    if has_content {
                        prompt.push_str(&section);
                    }
                }
                if settings.agent.memory_inject > 0 {
                    let query = if user_text.trim().is_empty() {
                        ""
                    } else {
                        user_text
                    };
                    match self.cfg.memory.recall(query, settings.agent.memory_inject) {
                        Ok(items) if !items.is_empty() => {
                            prompt.push_str("\n## 相关长期记忆（按相关度排序）\n");
                            for item in items.iter().take(settings.agent.memory_inject) {
                                prompt.push_str(&format!(
                                    "- [{}] {}\n",
                                    item.kind,
                                    truncate_chars(&item.content, 300)
                                ));
                            }
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "注入长期记忆失败"),
                    }
                }
            }
            AgentMode::Coding => {
                prompt.push_str(
                    "\n## 编程规范\n\
                     - 改代码前先读相关文件与依赖，不要凭记忆猜 API；必要时读源码或文档。\n\
                     - 代码任务先调用 update_plan 列 2-6 步（改哪里、怎么验证），完成一步就更新状态。\n\
                     - 尽量小 diff：一次只改一个逻辑点，不顺手重排无关代码，不删除看起来没用的东西（先确认）。\n\
                     - 改完先跑测试/构建（cargo check / cargo test 或项目对应命令），报告关键输出。\n\
                     - 错误先读完整错误链再改；同一失败不要连续重试超过两次。\n\
                     - Windows 用 PowerShell（pwsh），不要假设 bash / grep / sed。\n\
                     - 提交前看 diff；不用 reset --hard / force push 等破坏性 git 命令，除非用户明确要求。\n\
                     - 改完后可以 switch_mode {\"mode\":\"computer_use\"}，用真实界面跑一遍验证效果，再切回 coding/daily。\n",
                );
                prompt.push_str(FILES_AND_COMMANDS_SECTION);
                prompt.push_str(
                    "\n## 日常问题\n\
                     - 遇到天气、日程、生活常识、闲聊、新闻等日常内容：先 switch_mode {\"mode\":\"daily\"}，再回答。\n",
                );
            }
            AgentMode::ComputerUse => {
                prompt.push_str(FILES_AND_COMMANDS_SECTION);
                prompt.push_str(COMPUTER_OPERATIONS_SECTION);
                prompt.push_str(
                    "\n## 日常问题\n\
                     - 遇到天气、日程、生活常识、闲聊、新闻等日常内容：先 switch_mode {\"mode\":\"daily\"}，再回答。\n",
                );
            }
        }

        // 5) daily/coding 的按需展开：本轮用过计算机类工具后，下一次请求追加一次。
        if mode != AgentMode::ComputerUse && computer_expanded {
            prompt.push_str(COMPUTER_OPERATIONS_SECTION);
        }

        // 6) 技能：只列当前模式匹配的（modes 为空视为 daily）。
        let skills = self.cfg.skills.list_for_mode(mode.id());
        if !skills.is_empty() {
            prompt.push_str("\n## 可用技能\n");
            for summary in skills.iter().take(20) {
                prompt.push_str(&format!(
                    "- {} — {}\n",
                    summary.name,
                    truncate_chars(&summary.when_to_use, 120)
                ));
            }
            prompt.push_str(
                "任务与某个技能匹配时，先用 skill_load 加载技能正文，再严格按正文步骤执行。\n",
            );
        }

        let extra = settings.agent.system_prompt_extra.trim();
        if !extra.is_empty() {
            prompt.push_str("\n## 额外指示\n");
            prompt.push_str(extra);
            prompt.push('\n');
        }

        // 7) 工具/MCP 通用规范。
        prompt.push_str(
            "\n## 工具与 MCP\n\
             - MCP 服务器提供的工具会以 mcp_<server>_<tool> 的名字出现在可用工具列表里，直接像普通工具一样调用，参数按该工具的 JSON schema。\n\
             - 不确定有哪些 MCP 服务器或它们是否在线时，用 mcp_list_servers 查看状态。\n",
        );
        prompt.push_str(
            "\n## 工具使用规范\n\
             - 调用工具前先用一句话说明要做什么；调用后简要解读结果。\n\
             - 不要用工具重复做同一件事；读文件前先想清楚要读哪一段。\n\
             - 多步任务（尤其写代码/排查问题）先调用 update_plan 列出步骤，开始一步标 in_progress、完成标 done，让用户看到进度。\n\
             - 产出了图片/视频/文件或有用链接时，用 deliver 交付（只交本任务产生/引用的文件，绝对路径）。\n\
             - 工具返回 [err] 表示失败：先读错误信息并换一种方法；同一失败不要连续重试超过两次。\n\
             - 有副作用或不可逆的工具调用前，先用一句话说明影响。\n",
        );
        prompt
    }
}

/// 文件 + 命令（coding / computer_use 展开；daily 不展开命令细节）。
const FILES_AND_COMMANDS_SECTION: &str = "\n## 文件与命令\n\
     - 读文件前先想清楚要读哪一段；大文件分段读；写/改文件默认限制在工作目录内。\n\
     - 搜索本地文件用 search_files / list_dir；查资料用 web_search + web_fetch。\n\
     - 可能超过 60 秒的命令（构建、测试、启动服务、下载）用 run_command {background:true}，用 job_status 看全部后台任务状态与输出尾部。\n\
     - 单个任务完整输出用 read_job_output {job_id, wait_ms}；需要交互时用 send_job_input {job_id, input}；终止用 kill_job {job_id}。\n\
     - 前台命令只用于 60 秒内能结束的操作；被超时截断时改用后台任务，不要反复重试。\n";

/// 计算机操作基础版（daily 默认展开；列出可直接用的内置工具）。
const COMPUTER_OPERATIONS_BASIC_SECTION: &str = "\n## 计算机操作（基础版）\n\
     - 截图：直接用内置 screenshot（截整个屏幕）或 ui_screenshot（截指定窗口），画面会自动作为图片发给模型看。\n\
     - **不要用 python/PIL 截图**：内置截图工具会让模型直接看到画面，走 python 反而看不到结果。\n\
     - 键鼠：mouse_click / mouse_scroll / keyboard_type / keyboard_combo；坐标是屏幕物理像素。\n\
     - 界面优先用 UIA：ui_act / ui_click / ui_drag 按控件名操作；ui_tree / ui_window_list / foreground_window 看界面；wait_for_window 等窗口出现。\n\
     - 首次做界面操作前先 switch_mode {\"mode\":\"computer_use\"}。\n\
     - 需要隐藏主窗口时用 set_window_visibility {hidden:true}；任务结束、需要用户确认或出错时必须 set_window_visibility {hidden:false} 恢复。\n\
     - 动作后用 screenshot / ui_tree 验证结果；桌面活动提示（HUD）属正常，不需要向用户解释。\n";

/// 计算机操作完整段落（computer_use 常驻；daily/coding 用过一次后每轮展开）。
const COMPUTER_OPERATIONS_SECTION: &str = "\n## 计算机操作\n\
     - 看屏幕：先调用 screenshot（画面会自动作为图片给你看），再决定下一步；看不清就传 region 局部放大。\n\
     - **不要用 python/PIL 截图**：直接调用内置 screenshot / ui_screenshot，模型能直接看图。\n\
     - 需要精确位置时用 screenshot {grid:true}：输出图带 100px 坐标网格与刻度数字，坐标就是屏幕物理像素，看刻度读出坐标后再点。\n\
     - 能用 UI 自动化就优先用：ui_act {action:\"click\", name:\"保存\"} / ui_click / ui_drag 按控件名操作；拿不到控件时才用坐标。\n\
     - 鼠标：mouse_click {button:\"left\"|\"right\"|\"middle\", double:true}（双击）；滚轮 mouse_scroll {direction:\"up\"|\"down\", amount:N}。\n\
     - 键盘：keyboard_type 输入文字；keyboard_combo {keys:[\"ctrl\",\"s\"]} 发快捷键。\n\
     - 坐标一律是屏幕物理像素：非 grid 截图可能被等比缩小，屏幕坐标 = 图片坐标 × (截图原始宽 / 图片宽)；工具输出里有原始尺寸/屏幕尺寸/scale，先换算再点。\n\
     - 动作后必须验证：再截图或用 ui_tree / ui_window_list 确认结果，不要连续盲点。\n\
     - 需要等窗口出现或页面加载时用 wait_for_window / wait，不要反复点击或重复输入。\n\
     - 后台任务/长时间操作前可以 set_window_visibility {hidden:true} 隐藏主窗口；任务完成前、需要用户确认或出错时必须 set_window_visibility {hidden:false} 恢复窗口。\n\
     - 调用截图、鼠标、键盘、UIA 类工具时桌面会显示活动提示（HUD），这是正常现象，不需要向用户解释。\n\
     - 有副作用的按钮（删除、发送、支付）先说明影响再点，除非用户明确要求。\n";

/// 判断一个工具名是否属于"计算机使用类"（用于 daily/coding 的按需展开）。
fn is_computer_use_tool(name: &str) -> bool {
    let name = name.trim();
    name == "screenshot"
        || name == "ui_screenshot"
        || name == "screen_metrics"
        || name == "screen_size"
        || name == "foreground_window"
        || name == "wait_for_window"
        || name == "wait"
        || name.starts_with("mouse_")
        || name.starts_with("keyboard_")
        || name.starts_with("clipboard_")
        || name.starts_with("ui_")
        || name.starts_with("cdp_")
}

fn now_local_string() -> String {
    chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S %:z")
        .to_string()
}

// ============================================================
// 主循环
// ============================================================

#[derive(Clone, Debug)]
enum LoopMode {
    Main { turn_id: String },
    Subagent { id: String },
}

impl LoopMode {
    fn is_main(&self) -> bool {
        matches!(self, LoopMode::Main { .. })
    }

    fn id(&self) -> &str {
        match self {
            LoopMode::Main { turn_id } => turn_id,
            LoopMode::Subagent { id } => id,
        }
    }
}

/// 一轮循环的输入。
struct TurnSetup {
    mode: LoopMode,
    /// 人设 id（daily 模式注入；coding/computer_use 忽略）。
    persona_id: String,
    user_text: String,
    /// 用户这一轮随消息上传的图片（视觉输入）。
    user_images: Vec<ImagePart>,
    tools: Vec<ToolSpec>,
    /// 非空时只允许这些工具（子代理的受限工具集）。
    tool_allow: Vec<String>,
    /// 明确禁止的工具（例如子代理不能再开子代理）。
    forbid_tools: Vec<String>,
    max_iterations: u32,
    host: Arc<dyn AgentHost>,
    cancel_rx: watch::Receiver<bool>,
    parent_rx: Option<watch::Receiver<bool>>,
    history: Arc<RwLock<Vec<ChatMessage>>>,
    /// 当前会话 id（None = 不落对话历史库，例如子代理）。
    conversation_id: Option<String>,
}

#[derive(Default)]
struct TurnResult {
    text: String,
    reasoning: String,
    cancelled: bool,
    error: Option<String>,
    /// 已执行的工具调用次数。
    tool_calls: u32,
    /// 已进行的模型请求轮数。
    iterations: u32,
}

struct ToolExec {
    cancelled: bool,
    /// 这个工具的截图/图片标记（等整批工具执行完再注入，避免打断 tool_calls → tool 序列）。
    vision: Option<(String, String)>,
}

/// 模型请求最多尝试次数（连续未响应就终止本轮）。
const MAX_MODEL_ATTEMPTS: u32 = 5;

/// 第 `attempt` 次失败后的退避时间：1s / 2s / 4s / 8s。
fn retry_backoff(attempt: u32) -> Duration {
    let seconds = match attempt {
        0 | 1 => 1,
        2 => 2,
        3 => 4,
        _ => 8,
    };
    Duration::from_secs(seconds)
}

/// 一次模型请求尝试为什么结束（用于重试循环）。
enum ModelAttemptStop {
    Cancelled {
        text: String,
        reasoning: String,
    },
    Fatal {
        message: String,
        text: String,
    },
}

/// 排队轮次是否在开始前就被取消了（按 generation 判断，旧 cancel 不影响新轮次）。
fn turn_pre_cancelled(cancel_generation: u64, generation: u64) -> bool {
    cancel_generation >= generation
}

/// 主循环：模型 -> 工具 -> 模型，直到没有 tool_calls 或达到上限。
async fn run_loop(core: &Arc<EngineCore>, setup: TurnSetup) -> TurnResult {
    let mut result = TurnResult::default();

    if !setup.user_text.trim().is_empty() || !setup.user_images.is_empty() {
        let message = user_message_for(&setup);
        push_message(core, &setup, message);
    }

    let max_iterations = setup.max_iterations.max(1);
    // daily/coding 模式下，本轮用过计算机类工具后，下一次请求按需展开「计算机操作」。
    let mut computer_expanded = false;

    loop {
        if *setup.cancel_rx.borrow() {
            result.cancelled = true;
            break;
        }
        if result.iterations >= max_iterations {
            let message = format!(
                "已达到本轮工具调用上限（{max_iterations} 轮），任务可能还没完成。\
                 可以调大设置里的 agent.max_iterations，或把任务拆小后重试。"
            );
            emit_error(core, &setup.mode, &message);
            result.error = Some(message);
            break;
        }

        // 每轮重新组装 system prompt：模式可能被 switch_mode 切换，计算机操作段可能按需展开。
        let settings = core.settings_snapshot();
        let session_computer = core.computer_used_in_conversation(setup.conversation_id.as_deref());
        let system_prompt = core.build_system_prompt(
            &setup.user_text,
            &setup.persona_id,
            !setup.mode.is_main(),
            computer_expanded || session_computer,
        );
        // 上下文压缩：超过阈值时把早前历史折叠成摘要（只影响本次请求，不改会话库）。
        let prepared = prepare_history(core, &setup, &settings).await;
        let messages = build_request_messages(&settings, &system_prompt, &prepared.history);
        if setup.mode.is_main() {
            core.emit(AgentEvent::Phase("思考中".to_string()));
        }

        let client = core.llm();
        let request = LlmRequest {
            messages,
            tools: setup.tools.clone(),
            max_tokens: None,
            temperature: None,
        };

        // ---- 模型请求：超时/未响应按退避重试，最多 5 次；厂商故障则直接终止 ----
        let mut attempt: u32 = 0;
        let reply = loop {
            attempt += 1;
            let mut streamed_text = String::new();
            let mut streamed_reasoning = String::new();
            let mut saw_delta = false;
            // 闭包借用上面两个 String；作用域结束后把借用还回来，取消时才能读部分输出。
            let outcome = {
                let mode_for_cb = setup.mode.clone();
                let core_for_cb = core.clone();
                let mut on_delta = |delta: LlmDelta| match delta {
                    LlmDelta::Text(text) => {
                        saw_delta = true;
                        streamed_text.push_str(&text);
                        if mode_for_cb.is_main() {
                            core_for_cb.emit(AgentEvent::TextDelta(text));
                        }
                    }
                    LlmDelta::Reasoning(reasoning) => {
                        saw_delta = true;
                        streamed_reasoning.push_str(&reasoning);
                        if mode_for_cb.is_main() {
                            core_for_cb.emit(AgentEvent::ReasoningDelta(reasoning));
                        }
                    }
                };
                // 可取消流式：每收到一个 delta 检查取消标志，尽量立即停止。
                let call = client.chat_stream_cancellable(
                    request.clone(),
                    &mut on_delta,
                    &core.cancel_flag,
                );
                let cancel = wait_any_cancel(setup.cancel_rx.clone(), setup.parent_rx.clone());
                tokio::select! {
                    response = call => Some(response),
                    _ = cancel => None,
                }
            };

            match outcome {
                None => {
                    break Err(ModelAttemptStop::Cancelled {
                        text: streamed_text,
                        reasoning: streamed_reasoning,
                    });
                }
                Some(Err(error))
                    if crate::llm::is_cancelled_error(&error) || *setup.cancel_rx.borrow() =>
                {
                    break Err(ModelAttemptStop::Cancelled {
                        text: streamed_text,
                        reasoning: streamed_reasoning,
                    });
                }
                Some(Err(error)) => {
                    if crate::llm::is_transient_error(&error) && !saw_delta {
                        // 每次重试前查一次缓存的厂商状态；故障就不浪费时间重试。
                        let status_settings = core.settings_snapshot();
                        if let Some(status) =
                            provider_status::current_status(&status_settings).await
                        {
                            let attention = status.requires_attention();
                            let detail_label = status.label.clone();
                            let provider = status.provider.clone();
                            let url = status.url.clone();
                            core.publish_provider_status(status);
                            if attention {
                                break Err(ModelAttemptStop::Fatal {
                                    message: format!(
                                        "检测到 {provider} API 出现故障（{detail_label}），\
                                         请等待修复：{url}"
                                    ),
                                    text: String::new(),
                                });
                            }
                        }
                        if attempt < MAX_MODEL_ATTEMPTS {
                            let delay = retry_backoff(attempt);
                            if setup.mode.is_main() {
                                core.emit(AgentEvent::Phase(format!(
                                    "模型未响应，{:.0} 秒后重试（{attempt}/{MAX_MODEL_ATTEMPTS}）",
                                    delay.as_secs_f64()
                                )));
                            }
                            let cancel =
                                wait_any_cancel(setup.cancel_rx.clone(), setup.parent_rx.clone());
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => continue,
                                _ = cancel => {
                                    break Err(ModelAttemptStop::Cancelled {
                                        text: String::new(),
                                        reasoning: String::new(),
                                    });
                                }
                            }
                        }
                        break Err(ModelAttemptStop::Fatal {
                            message: format!("模型连续 {MAX_MODEL_ATTEMPTS} 次未响应，本轮已终止"),
                            text: String::new(),
                        });
                    }
                    break Err(ModelAttemptStop::Fatal {
                        message: format!(
                            "调用模型失败：{error}。请检查设置里的 base_url、模型名、API key 与网络；\
                             流式端点需要支持 /chat/completions 的 SSE 返回。"
                        ),
                        text: streamed_text,
                    });
                }
                Some(Ok(reply)) => break Ok(reply),
            }
        };

        let reply = match reply {
            Ok(reply) => reply,
            Err(ModelAttemptStop::Cancelled { text, reasoning }) => {
                // 被取消：把已经流出的部分文本落库，保持历史可读。
                if !text.trim().is_empty() {
                    push_message(core, &setup, ChatMessage::assistant(text.clone()));
                    if setup.mode.is_main() {
                        core.emit(AgentEvent::AssistantMessage {
                            text: text.clone(),
                            reasoning: if reasoning.trim().is_empty() {
                                None
                            } else {
                                Some(reasoning.clone())
                            },
                        });
                    }
                }
                result.text = text;
                result.reasoning = reasoning;
                result.cancelled = true;
                break;
            }
            Err(ModelAttemptStop::Fatal { message, text }) => {
                if !text.trim().is_empty() {
                    push_message(core, &setup, ChatMessage::assistant(text.clone()));
                }
                emit_error(core, &setup.mode, &message);
                result.error = Some(message);
                break;
            }
        };

        if reply.usage != Usage::default() && setup.mode.is_main() {
            core.emit(AgentEvent::Usage {
                prompt_tokens: reply.usage.prompt_tokens,
                completion_tokens: reply.usage.completion_tokens,
                cached_tokens: reply.usage.cached_tokens,
            });
        }

        let assistant_text = reply.text.clone();
        let assistant_reasoning = reply.reasoning.clone();
        // 补齐空的 tool_call id：assistant 消息与 tool 结果必须用同一组 id，
        // 否则下一轮请求会因消息序列非法被 API 拒绝。
        let mut calls = reply.tool_calls.clone();
        for call in &mut calls {
            if call.id.trim().is_empty() {
                call.id = format!("call_{}", crate::util::short_id());
            }
        }
        push_message(
            core,
            &setup,
            ChatMessage::assistant_with_calls(
                Some(assistant_text.clone()).filter(|s| !s.is_empty()),
                Some(assistant_reasoning.clone()).filter(|s| !s.is_empty()),
                calls.clone(),
            ),
        );

        if setup.mode.is_main() {
            if !assistant_text.is_empty() || !assistant_reasoning.is_empty() {
                core.emit(AgentEvent::AssistantMessage {
                    text: assistant_text.clone(),
                    reasoning: if assistant_reasoning.is_empty() {
                        None
                    } else {
                        Some(assistant_reasoning.clone())
                    },
                });
            }
        } else if !assistant_text.is_empty() {
            emit_activity(
                core,
                &setup.mode,
                format!("生成汇报（{} 字）", assistant_text.chars().count()),
            );
        }

        result.text = assistant_text;
        result.reasoning = assistant_reasoning;

        if calls.is_empty() {
            break;
        }

        result.iterations += 1;
        let mut cancelled = false;
        // 这一批是否有计算机使用类工具（决定下一次请求是否按需展开计算机操作段）。
        let mut batch_computer_use = false;
        // 截图/图片要等这一批 tool 消息全部进历史后再注入为 user 图片消息，
        // 否则会出现 assistant(tool_calls) → tool → user → tool 的非法序列（API 400）。
        let mut pending_vision: Vec<(String, String)> = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            result.tool_calls += 1;
            if is_computer_use_tool(&call.function.name) {
                batch_computer_use = true;
            }
            let exec = execute_tool(core, &setup, call.clone()).await;
            if let Some(vision) = exec.vision {
                pending_vision.push(vision);
            }
            if exec.cancelled || *setup.cancel_rx.borrow() {
                cancelled = true;
                // 同一批里剩余的工具调用不能再执行，但必须补上 tool 结果，
                // 否则历史里 assistant(tool_calls) 与 tool 消息数量对不上，下一轮会 400。
                for rest in calls.iter().skip(index + 1) {
                    push_tool_message(
                        core,
                        &setup,
                        &rest.id,
                        &rest.function.name,
                        "本轮已取消，这条工具调用没有执行。",
                        false,
                    );
                }
                break;
            }
        }
        for (tool_name, content) in pending_vision {
            attach_images_from_tool_output(core, &setup, &tool_name, &content);
        }
        if batch_computer_use {
            computer_expanded = true;
            // 会话级记忆：本会话后续每轮都展开完整计算机操作段（切新会话才重置）。
            core.mark_computer_used(setup.conversation_id.as_deref());
        }
        if cancelled {
            result.cancelled = true;
            break;
        }
    }

    result
}

/// 组请求消息：system + 按 `history_limit` 与 token 预算裁剪后的历史。
fn build_request_messages(
    settings: &Settings,
    system_prompt: &str,
    history: &[ChatMessage],
) -> Vec<ChatMessage> {
    let system = ChatMessage::system(system_prompt);
    let system_tokens = estimate_tokens(system_prompt);
    let context_window = settings.server.context_window.max(2048) as usize;
    let reserved_output = settings.server.max_tokens as usize;
    let budget = context_window
        .saturating_sub(reserved_output)
        .saturating_sub(system_tokens)
        .saturating_sub(1024)
        .max(1024);
    let history_limit = settings.agent.history_limit;
    let trimmed = trim_history(history, history_limit, budget);
    let mut messages = Vec::with_capacity(trimmed.len() + 1);
    messages.push(system);
    messages.extend(trimmed);
    messages
}

// ============================================================
// 上下文压缩
// ============================================================

/// 压缩后准备发给模型的上下文。
struct PreparedHistory {
    history: Vec<ChatMessage>,
}

/// 压缩缓存：同一会话、同一前缀长度时复用摘要，避免每轮重复调模型。
struct ContextCache {
    conversation_id: String,
    covered_len: usize,
    summary: String,
}

const COMPRESS_SYSTEM_PROMPT: &str = "你是一个对话压缩器。把给出的早前对话压缩成一段中文摘要，供助手继续任务使用。\n\
要求：\n\
1. 保留：用户的目标与偏好、已确认的事实/数字/路径/文件名、已完成的操作与结果、未完成的事项与下一步。\n\
2. 去掉：寒暄、重复解释、工具原始输出细节（只留结论）、图片内容（只留“看了某张图”）。\n\
3. 只输出摘要正文，不要标题、不要 JSON、不要代码块，长度不超过 1500 字。";

/// 每轮请求前的历史准备：超过阈值时压缩早前消息为摘要（只影响本次请求）。
async fn prepare_history(
    core: &Arc<EngineCore>,
    setup: &TurnSetup,
    settings: &Settings,
) -> PreparedHistory {
    let history = setup.history.read().clone();
    let enabled = settings.agent.context_compress;
    let before_tokens = history_token_estimate(&history);
    if !enabled || before_tokens <= settings.agent.context_threshold_tokens {
        return PreparedHistory { history };
    }
    let keep = settings.agent.keep_recent_messages.max(4);
    if history.len() <= keep {
        return PreparedHistory { history };
    }
    let split = compression_split(&history, keep);
    if split == 0 {
        return PreparedHistory { history };
    }
    let kept = &history[split..];
    let conv_id = setup.conversation_id.clone().unwrap_or_default();
    let cache_key = conv_id.clone();

    // 复用缓存：同一会话、覆盖前缀没变时不再重新摘要。
    let cached = {
        let mut guard = core.context_cache.lock();
        match guard.as_ref() {
            Some(cache)
                if cache.conversation_id == cache_key
                    && cache.covered_len == split
                    && !cache.summary.trim().is_empty() =>
            {
                Some(cache.summary.clone())
            }
            _ => {
                // 前缀变了（新消息把边界推后/历史被裁剪），旧缓存作废。
                if guard.is_some() {
                    *guard = None;
                }
                None
            }
        }
    };

    let (summary, failed) = match cached {
        Some(summary) => (summary, false),
        None => {
            let transcript = compression_transcript(&history[..split]);
            match summarize_history(core, setup, &transcript).await {
                Some(summary) => {
                    *core.context_cache.lock() = Some(ContextCache {
                        conversation_id: cache_key.clone(),
                        covered_len: split,
                        summary: summary.clone(),
                    });
                    (summary, false)
                }
                None => {
                    // 降级：只保留最近 N 条 + 一条“已省略”提示，不中断本轮。
                    let fallback = "更早的内容已省略（上下文压缩未能生成摘要），仅保留最近的消息。"
                        .to_string();
                    *core.context_cache.lock() = Some(ContextCache {
                        conversation_id: cache_key.clone(),
                        covered_len: split,
                        summary: fallback.clone(),
                    });
                    (fallback, true)
                }
            }
        }
    };

    let effective = build_compressed_history(&summary, kept, failed);
    let after_tokens = history_token_estimate(&effective) as u64;
    let before_tokens = before_tokens as u64;

    if setup.mode.is_main() {
        core.emit(AgentEvent::ContextCompressed {
            before_tokens,
            after_tokens,
            kept: kept.len(),
        });
    }
    if !conv_id.is_empty() {
        if let Err(error) = core.cfg.conversations.save_summary(
            &conv_id,
            &summary,
            split,
            kept.len(),
            before_tokens,
            after_tokens,
        ) {
            tracing::warn!(error = %error, "保存上下文压缩摘要失败");
        }
    }
    PreparedHistory { history: effective }
}

/// 组装压缩后的历史：一条 system 摘要消息 + 最近 N 条原文。
fn build_compressed_history(summary: &str, kept: &[ChatMessage], failed: bool) -> Vec<ChatMessage> {
    let mut effective: Vec<ChatMessage> = Vec::with_capacity(kept.len() + 1);
    effective.push(ChatMessage::system(format!(
        "（这是早前对话的压缩摘要{}，供你延续上下文；最近 {} 条消息保留原文。）\n{summary}",
        if failed {
            "，压缩调用失败已降级"
        } else {
            ""
        },
        kept.len()
    )));
    effective.extend_from_slice(kept);
    effective
}

/// 计算压缩切分点：最近 `keep` 条之外的部分被摘要；并保证保留部分不以 tool 消息开头。
fn compression_split(history: &[ChatMessage], keep: usize) -> usize {
    let mut split = history.len().saturating_sub(keep.max(1));
    while split > 0 && history[split].role == Role::Tool {
        split -= 1;
    }
    split
}

/// 历史估算 token（不含 system prompt）。
fn history_token_estimate(history: &[ChatMessage]) -> u64 {
    total_tokens(history) as u64
}

/// 把早前历史转成给压缩模型看的纯文本（不包含图片 base64）。
fn compression_transcript(history: &[ChatMessage]) -> String {
    const MAX_CHARS: usize = 120_000;
    const TOOL_CHARS: usize = 600;
    let mut out = String::new();
    for message in history {
        if out.chars().count() >= MAX_CHARS {
            out.push_str("（更早内容已截断）\n");
            break;
        }
        match message.role {
            Role::User => {
                out.push_str(&format!(
                    "用户：{}\n",
                    truncate_chars(message.text().trim(), 4000)
                ));
            }
            Role::Assistant => {
                let text = message.text();
                let text = text.trim();
                if !text.is_empty() {
                    out.push_str(&format!("助手：{}\n", truncate_chars(text, 4000)));
                }
                if !message.tool_calls.is_empty() {
                    let names = message
                        .tool_calls
                        .iter()
                        .map(|call| {
                            let args = call.function.parse_args();
                            format!(
                                "{}({})",
                                call.function.name,
                                RiskClassifier::summary("", &args)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("；");
                    out.push_str(&format!(
                        "（助手调用了工具：{}）\n",
                        truncate_chars(&names, 800)
                    ));
                }
            }
            Role::Tool => {
                let name = message.name.as_deref().unwrap_or("工具");
                out.push_str(&format!(
                    "工具[{name}]：{}\n",
                    truncate_chars(message.text().trim(), TOOL_CHARS)
                ));
            }
            Role::System => {
                // system 消息是每轮重新组装的，不参与摘要。
            }
        }
    }
    out
}

/// 调主模型生成摘要；失败/超时/取消返回 None（由调用方降级）。
async fn summarize_history(
    core: &Arc<EngineCore>,
    setup: &TurnSetup,
    transcript: &str,
) -> Option<String> {
    if transcript.trim().is_empty() {
        return None;
    }
    let client = core.llm();
    let request = LlmRequest {
        messages: vec![
            ChatMessage::system(COMPRESS_SYSTEM_PROMPT),
            ChatMessage::user(format!("请压缩下面的早前对话：\n\n{transcript}")),
        ],
        tools: Vec::new(),
        max_tokens: Some(2048),
        temperature: Some(0.2),
    };
    let call = async {
        match tokio::time::timeout(Duration::from_secs(60), client.chat_once(request)).await {
            Ok(Ok(reply)) => {
                let text = truncate_chars(reply.text.trim(), 1500);
                if text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "上下文压缩调用失败，降级处理");
                None
            }
            Err(_) => {
                tracing::warn!("上下文压缩调用超时（60s），降级处理");
                None
            }
        }
    };
    let cancel = wait_any_cancel(setup.cancel_rx.clone(), setup.parent_rx.clone());
    tokio::select! {
        summary = call => summary,
        _ = cancel => None,
    }
}

/// 裁剪历史：只保留最近 `limit` 条（0 = 不限），再按 token 预算从旧到新丢弃。
///
/// 约束：不能以 `tool` 消息开头（否则缺少对应的 assistant tool_calls，API 会 400）。
fn trim_history(history: &[ChatMessage], limit: usize, budget_tokens: usize) -> Vec<ChatMessage> {
    if history.is_empty() {
        return Vec::new();
    }
    let start = if limit == 0 {
        0
    } else {
        history.len().saturating_sub(limit)
    };
    let mut keep: Vec<ChatMessage> = history[start..].to_vec();
    while keep.first().map(|m| m.role == Role::Tool).unwrap_or(false) {
        keep.remove(0);
    }
    while keep.len() > 1 && total_tokens(&keep) > budget_tokens {
        keep.remove(0);
        while keep.first().map(|m| m.role == Role::Tool).unwrap_or(false) {
            keep.remove(0);
        }
    }
    // 极端预算下只剩一条 tool 消息时宁可不发历史，也不要发非法序列。
    if keep.len() == 1 && keep[0].role == Role::Tool {
        keep.clear();
    }
    keep
}

fn total_tokens(messages: &[ChatMessage]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

fn estimate_message_tokens(message: &ChatMessage) -> usize {
    // 角色 / 分隔符的固定开销。
    let mut tokens = 6usize;
    if let Some(content) = &message.content {
        tokens += estimate_tokens(content);
    }
    if let Some(reasoning) = &message.reasoning_content {
        tokens += estimate_tokens(reasoning);
    }
    for call in &message.tool_calls {
        tokens += estimate_tokens(&call.function.name);
        tokens += estimate_tokens(&call.function.arguments);
        tokens += 8;
    }
    if let Some(name) = &message.name {
        tokens += estimate_tokens(name);
    }
    // 图片按 OpenAI 的粗略口径估算（低细节约 85 token，高清按像素/750）。
    for image in &message.images {
        let pixels = (image.width as usize).saturating_mul(image.height as usize);
        tokens += (pixels / 750).max(85);
    }
    tokens
}

/// 历史只保留最近 `max_len` 条；同时不保留孤立的 tool 消息。
fn trim_history_tail(history: &RwLock<Vec<ChatMessage>>, max_len: usize) {
    let mut guard = history.write();
    if guard.len() <= max_len {
        return;
    }
    let drain = guard.len() - max_len;
    guard.drain(..drain);
    while guard.first().map(|m| m.role == Role::Tool).unwrap_or(false) {
        guard.remove(0);
    }
}

/// 修复被中断（panic / 异常）打断的历史：每个带 tool_calls 的 assistant 消息
/// 后面必须补齐对应的 tool 结果，否则下一轮请求会因消息序列非法被 API 拒绝。
fn repair_history(history: &RwLock<Vec<ChatMessage>>) {
    let mut guard = history.write();
    let mut index = 0usize;
    while index < guard.len() {
        let calls = if guard[index].role == Role::Assistant {
            guard[index].tool_calls.clone()
        } else {
            Vec::new()
        };
        if calls.is_empty() {
            index += 1;
            continue;
        }
        let mut cursor = index + 1;
        let mut answered: Vec<String> = Vec::new();
        while cursor < guard.len() && guard[cursor].role == Role::Tool {
            if let Some(call_id) = guard[cursor].tool_call_id.clone() {
                answered.push(call_id);
            }
            cursor += 1;
        }
        let missing: Vec<ToolCall> = calls
            .into_iter()
            .filter(|call| !answered.iter().any(|id| id == &call.id))
            .collect();
        if missing.is_empty() {
            index = cursor;
            continue;
        }
        let inserts: Vec<ChatMessage> = missing
            .into_iter()
            .map(|call| {
                ChatMessage::tool_result(
                    call.id,
                    call.function.name,
                    "内部错误：这条工具调用没有返回结果（本轮被中断），请重新发起或换一种方法。",
                    false,
                )
            })
            .collect();
        let insert_at = cursor;
        guard.splice(insert_at..insert_at, inserts);
        index = insert_at + 1;
    }
}

// ============================================================
// 消息入库 + 视觉输入
// ============================================================

/// 把一条消息同时写入内存历史和对话历史库（没有会话 id 时只写内存，例如子代理）。
fn push_message(core: &EngineCore, setup: &TurnSetup, message: ChatMessage) {
    setup.history.write().push(message.clone());
    if let Some(conv_id) = &setup.conversation_id {
        if let Err(e) = core.cfg.conversations.append(conv_id, &message) {
            tracing::warn!(error = %e, conv_id = %conv_id, "消息写入对话历史失败");
        }
    }
}

/// 本轮开头的用户消息：带上传图片时用 `user_with_images`。
fn user_message_for(setup: &TurnSetup) -> ChatMessage {
    if setup.user_images.is_empty() {
        ChatMessage::user(&setup.user_text)
    } else {
        ChatMessage::user_with_images(&setup.user_text, setup.user_images.clone())
    }
}

/// 视觉插入消息的固定前缀（用来识别“自动注入的截图”，不占用户上传图片的额度）。
const VISION_PROMPT_PREFIX: &str = "（这是刚才截图的画面，可能已被等比缩小。";
/// 视觉插入的用户消息文案：明确说明缩放与坐标换算，避免按图片坐标点偏。
const VISION_PROMPT_TEXT: &str = "（这是刚才截图的画面，可能已被等比缩小。\
点击/拖拽请用屏幕物理坐标：先看工具输出里的截图原始尺寸/屏幕尺寸/scale，\
屏幕坐标 = 图片坐标 × (截图原始宽 / 图片宽)。\
能用 ui_act/ui_click 按控件名操作就优先用它，不要用坐标。）";
/// 一轮里最多把几张工具图片喂给模型（控制 token）。
const VISION_MAX_IMAGES_PER_TURN: usize = 3;

/// 拼接视觉注入文案，并把附图的尺寸写进去（模型换算坐标时要用）。
fn vision_prompt_text(images: &[ImagePart]) -> String {
    let sizes = images
        .iter()
        .map(|image| format!("{}x{}", image.width, image.height))
        .collect::<Vec<_>>()
        .join("、");
    format!("{VISION_PROMPT_TEXT}（附图尺寸：{sizes}）")
}

/// 工具输出里出现 `[screenshot:<路径>]` / `[image:<路径>]` 时，把图片作为下一条
/// user 消息插入历史（不进 UI 消息流，只 emit `ImagesAttached`）。
fn attach_images_from_tool_output(
    core: &Arc<EngineCore>,
    setup: &TurnSetup,
    tool_name: &str,
    content: &str,
) {
    if !setup.mode.is_main() {
        return;
    }
    let settings = core.settings_snapshot();
    if !settings.agent.vision {
        return;
    }
    let markers = extract_image_markers(content);
    if markers.is_empty() {
        return;
    }
    let max_edge = settings.agent.vision_max_edge.max(64);
    let (existing_images, existing_sources) = {
        let history = setup.history.read();
        let mut sources = Vec::new();
        let mut count = 0usize;
        for message in history.iter() {
            // 只统计"由截图自动注入"的图片消息，用户自己上传的图片不占这 3 张额度。
            let injected = message
                .content
                .as_deref()
                .map(|text| text.starts_with(VISION_PROMPT_PREFIX))
                .unwrap_or(false);
            if !injected {
                continue;
            }
            for image in &message.images {
                count += 1;
                if let Some(source) = &image.source {
                    sources.push(source.clone());
                }
            }
        }
        (count, sources)
    };

    let mut images: Vec<ImagePart> = Vec::new();
    for marker in markers {
        if existing_images + images.len() >= VISION_MAX_IMAGES_PER_TURN {
            break;
        }
        if existing_sources.iter().any(|source| source == &marker) {
            continue;
        }
        let path = Path::new(&marker);
        if !path.is_absolute() {
            tracing::warn!(path = %marker, "截图标记不是绝对路径，跳过 vision");
            continue;
        }
        // 图片解码/编码在 image crate 里可能 panic（畸形文件），这里兜底隔离。
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            encode_image_for_vision(path, max_edge)
        })) {
            Ok(Ok(image)) => images.push(image),
            Ok(Err(e)) => tracing::warn!(path = %marker, error = %e, "读取截图失败，跳过 vision"),
            Err(_) => tracing::error!(path = %marker, "图片解码/编码发生 panic（已隔离）"),
        }
    }
    if images.is_empty() {
        return;
    }
    let count = images.len();
    let source = tool_name.to_string();
    let prompt = vision_prompt_text(&images);
    push_message(core, setup, ChatMessage::user_with_images(prompt, images));
    core.emit(AgentEvent::ImagesAttached { count, source });
}

/// 从工具输出文本里提取 `[screenshot:...]` / `[image:...]` 标记后的路径。
pub(crate) fn extract_image_markers(text: &str) -> Vec<String> {
    const PREFIXES: [&str; 2] = ["[screenshot:", "[image:"];
    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    while cursor < text.len() {
        let mut found: Option<(usize, usize)> = None;
        for (offset, _) in text[cursor..].char_indices() {
            let start = cursor + offset;
            if let Some(prefix) = PREFIXES
                .iter()
                .find(|prefix| prefix_matches(text, start, prefix))
            {
                found = Some((start, prefix.len()));
                break;
            }
        }
        let Some((start, prefix_len)) = found else {
            break;
        };
        let rest_start = start + prefix_len;
        let Some(relative_end) = text[rest_start..].find(']') else {
            break;
        };
        let end = rest_start + relative_end;
        let raw = text[rest_start..end]
            .trim()
            .trim_matches(|ch: char| ch == '"' || ch == '\'' || ch == '`')
            .trim();
        if !raw.is_empty() && raw.chars().count() <= 240 && !out.iter().any(|item| item == raw) {
            out.push(raw.to_string());
        }
        cursor = end + 1;
    }
    out
}

fn prefix_matches(text: &str, start: usize, prefix: &str) -> bool {
    let mut chars = text[start..].chars();
    for want in prefix.chars() {
        match chars.next() {
            Some(actual) if actual.eq_ignore_ascii_case(&want) => {}
            _ => return false,
        }
    }
    true
}

/// 读图、按最大边等比缩放、PNG 编码、base64，生成给模型的 [`ImagePart`]。
pub(crate) fn encode_image_for_vision(path: &Path, max_edge: u32) -> anyhow::Result<ImagePart> {
    let max_edge = max_edge.max(1);
    let decoded =
        image::open(path).map_err(|e| anyhow::anyhow!("无法解码图片 {}：{e}", path.display()))?;
    let (width, height) = (decoded.width().max(1), decoded.height().max(1));
    let decoded = if width.max(height) > max_edge {
        decoded.resize(max_edge, max_edge, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };
    let (out_width, out_height) = (decoded.width().max(1), decoded.height().max(1));
    let mut bytes: Vec<u8> = Vec::new();
    decoded
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .map_err(|e| anyhow::anyhow!("PNG 编码失败：{e}"))?;
    let base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(ImagePart::png(
        base64,
        out_width,
        out_height,
        Some(path.display().to_string()),
    ))
}

// ============================================================
// 工具执行
// ============================================================

async fn execute_tool(core: &Arc<EngineCore>, setup: &TurnSetup, call: ToolCall) -> ToolExec {
    let call_id = if call.id.trim().is_empty() {
        format!("call_{}", crate::util::short_id())
    } else {
        call.id.clone()
    };
    let name = call.function.name.trim().to_string();
    let args_raw = call.function.arguments.clone();
    let args = call.function.parse_args();
    let main = setup.mode.is_main();
    let preview = preview_for(&name, &args);

    if main {
        core.emit(AgentEvent::ToolStarted {
            call_id: call_id.clone(),
            name: name.clone(),
            args: args_raw.clone(),
            preview: preview.clone(),
        });
    } else {
        emit_activity(core, &setup.mode, format!("调用工具 {name}"));
    }

    let blocked = if name.is_empty() {
        Some("模型返回了空的工具名，无法执行。请检查模型是否支持 function calling。".to_string())
    } else if setup
        .forbid_tools
        .iter()
        .any(|forbidden| forbidden == &name)
    {
        Some(format!(
            "当前上下文不允许调用工具 {name}（子代理不能再启动子代理）。\
             请把需要完成的工作合并到当前任务里，直接调用普通工具完成。"
        ))
    } else if !setup.tool_allow.is_empty()
        && !setup.tool_allow.iter().any(|allowed| allowed == &name)
    {
        Some(format!(
            "工具 {name} 不在本次允许的列表里。允许的工具：{}。请改用其中的工具。",
            setup.tool_allow.join(", ")
        ))
    } else {
        None
    };
    if let Some(reason) = blocked {
        return finish_tool_error(core, setup, call_id, &name, reason);
    }

    let Some(tool) = core.cfg.registry.get(&name) else {
        return finish_tool_error(
            core,
            setup,
            call_id,
            &name,
            format!(
                "未知工具：{name}。请只使用工具列表里存在的工具；若需要类似能力，\
                 请用 run_command 或 read_file 等现有工具组合实现。"
            ),
        );
    };

    // 风险审查（提示词隔离的小模型评分 + 授权/拒绝记忆）。
    let settings = core.settings_snapshot();
    let mut review_proceeded = false;
    match review_gate(
        core, setup, &settings, &call_id, &name, &args_raw, &args, main,
    )
    .await
    {
        ReviewGateResult::NotApplicable => {}
        ReviewGateResult::Proceed => review_proceeded = true,
        ReviewGateResult::Blocked(exec) => return exec,
    }

    // 旧的危险操作审批（approval_mode=ask）；风险审查已处理的不再重复询问。
    let needs_approval = !review_proceeded
        && settings.agent.approval_mode == ApprovalMode::Ask
        && tool.requires_approval(&args);
    if needs_approval {
        if !main {
            return finish_tool_error(
                core,
                setup,
                call_id,
                &name,
                format!(
                    "子代理在 approval_mode = ask 时不能执行需要确认的危险工具 {name}。\
                     请在主对话里执行，或把 approval_mode 改成 auto。"
                ),
            );
        }
        let decision = request_approval(core, setup, &call_id, &name, &args_raw, None, false).await;
        if !decision.is_approved() {
            return finish_tool_error(
                core,
                setup,
                call_id,
                &name,
                "用户拒绝了这次操作，或等待确认超时（10 分钟）。\
                 请换一种不需要危险操作的方法，或先说明原因再试。"
                    .to_string(),
            );
        }
    }

    let started = Instant::now();
    let ctx = ToolCtx::new(
        setup.host.clone(),
        setup.mode.id().to_string(),
        call_id.clone(),
    );
    let cancel = wait_any_cancel(setup.cancel_rx.clone(), setup.parent_rx.clone());
    // 工具内部的 panic 不能让整个轮次崩掉：catch_unwind 后转成 [err] 结果。
    let tool_call = std::panic::AssertUnwindSafe(core.cfg.registry.call(&ctx, &name, &args));
    let outcome = tokio::select! {
        result = tool_call.catch_unwind() => Some(match result {
            Ok(outcome) => outcome,
            Err(_) => {
                tracing::error!(tool = %name, call_id = %call_id, "工具执行时发生 panic（已隔离）");
                ToolOutcome::err(format!(
                    "工具执行时发生内部错误（已捕获，不影响程序）：{name}。\
                     请换一种方法完成任务；若反复出现，请把工具名和参数反馈给开发者。"
                ))
            }
        }),
        _ = cancel => None,
    };

    match outcome {
        None => {
            let content = "工具执行被用户取消。".to_string();
            push_tool_message(core, setup, &call_id, &name, &content, false);
            let elapsed_ms = started.elapsed().as_millis() as u64;
            if main {
                core.emit(AgentEvent::ToolFinished {
                    call_id,
                    name,
                    ok: false,
                    output: content.clone(),
                    summary: Some("已取消".to_string()),
                    elapsed_ms,
                });
            } else {
                emit_activity(
                    core,
                    &setup.mode,
                    format!("工具 {name} 已取消（{}）", human_duration_ms(elapsed_ms)),
                );
            }
            ToolExec {
                cancelled: true,
                vision: None,
            }
        }
        Some(outcome) => {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let max_chars = settings.agent.max_tool_output_chars;
            // 工具的 truncate_output 来自外部 crate，同样兜底 panic。
            let truncated = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tool.truncate_output(&outcome.content, max_chars)
            })) {
                Ok(text) => text,
                Err(_) => {
                    tracing::error!(tool = %name, "工具 truncate_output 发生 panic（已隔离）");
                    crate::util::truncate_chars(&outcome.content, max_chars)
                }
            };
            let content = if truncated.trim().is_empty() {
                if outcome.ok {
                    "(工具没有输出)".to_string()
                } else {
                    "(工具没有返回错误详情)".to_string()
                }
            } else {
                truncated
            };
            push_tool_message(core, setup, &call_id, &name, &content, outcome.ok);
            if main {
                core.emit(AgentEvent::ToolFinished {
                    call_id,
                    name: name.clone(),
                    ok: outcome.ok,
                    output: content.clone(),
                    summary: outcome.display,
                    elapsed_ms,
                });
            } else {
                emit_activity(
                    core,
                    &setup.mode,
                    format!(
                        "工具 {name} {}（{}）{}",
                        if outcome.ok { "完成" } else { "失败" },
                        human_duration_ms(elapsed_ms),
                        outcome.display.unwrap_or_default()
                    ),
                );
            }
            // 工具输出里有截图/图片标记时先记下来，等整批工具结束后再注入
            // 一条带图片的 user 消息（让下一次模型请求能「看到」画面）。
            let vision =
                if main && settings.agent.vision && !extract_image_markers(&content).is_empty() {
                    Some((name.clone(), content.clone()))
                } else {
                    None
                };
            ToolExec {
                cancelled: false,
                vision,
            }
        }
    }
}

/// 工具没有真正执行（未知/被禁止/审批拒绝）时的收尾。
fn finish_tool_error(
    core: &Arc<EngineCore>,
    setup: &TurnSetup,
    call_id: String,
    name: &str,
    reason: String,
) -> ToolExec {
    push_tool_message(core, setup, &call_id, name, &reason, false);
    if setup.mode.is_main() {
        core.emit(AgentEvent::ToolFinished {
            call_id,
            name: name.to_string(),
            ok: false,
            output: reason.clone(),
            summary: Some("未执行".to_string()),
            elapsed_ms: 0,
        });
    } else {
        emit_activity(
            core,
            &setup.mode,
            format!("工具 {name} 未执行：{}", truncate_chars(&reason, 200)),
        );
    }
    ToolExec {
        cancelled: false,
        vision: None,
    }
}

fn push_tool_message(
    core: &EngineCore,
    setup: &TurnSetup,
    call_id: &str,
    name: &str,
    content: &str,
    ok: bool,
) {
    push_message(
        core,
        setup,
        ChatMessage::tool_result(
            call_id.to_string(),
            name.to_string(),
            content.to_string(),
            ok,
        ),
    );
}

/// 安全地收集工具 spec：单个工具的 `spec()` panic 只跳过该工具，不影响整轮。
///
/// `allow` 为 None 表示全部工具；Some(空) 也按全部处理（与旧 `specs_filtered` 一致）。
fn safe_tool_specs(registry: &ToolRegistry, allow: Option<&[String]>) -> Vec<ToolSpec> {
    let all_names = registry.names();
    let mut out = Vec::with_capacity(all_names.len());
    for name in all_names {
        if let Some(allow) = allow {
            if !allow.is_empty() && !allow.iter().any(|allowed| allowed == &name) {
                continue;
            }
        }
        let Some(tool) = registry.get(&name) else {
            continue;
        };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tool.spec())) {
            Ok(spec) => out.push(spec),
            Err(_) => {
                tracing::error!(tool = %name, "工具 spec() 发生 panic（已跳过该工具）");
            }
        }
    }
    out
}

fn preview_for(name: &str, args: &Value) -> Option<String> {
    let picked: Option<String> = match name {
        "run_command" => args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "read_file" | "write_file" | "edit_file" | "append_file" | "list_dir" | "file_info"
        | "make_dir" | "delete_path" => args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "screenshot" => args
            .get("save_path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "spawn_subagent" => args
            .get("task")
            .and_then(|v| v.as_str())
            .map(|s| truncate_chars(s, 160)),
        _ => {
            let raw = args.to_string();
            if raw == "{}" { None } else { Some(raw) }
        }
    };
    picked.map(|s| truncate_chars(&s, 200))
}

/// 风险审查对一次工具调用的结论。
enum ReviewGateResult {
    /// 不需要审查（review 关闭或该工具不触发审查）。
    NotApplicable,
    /// 审查通过（1 分自动执行 / 已授权记忆命中），直接执行。
    Proceed,
    /// 被拒绝或取消，返回工具收尾。
    Blocked(ToolExec),
}

/// 风险审查：查授权/拒绝记忆 → 需要时调独立小模型评分 → 1 分放行、
/// 2 分弹窗且允许后记住同类、3 分每次确认、4 分强提示且拒绝后记住拒绝。
async fn review_gate(
    core: &Arc<EngineCore>,
    setup: &TurnSetup,
    settings: &Settings,
    call_id: &str,
    name: &str,
    args_raw: &str,
    args: &Value,
    main: bool,
) -> ReviewGateResult {
    let review = settings.review.clone();
    if !review.enabled {
        return ReviewGateResult::NotApplicable;
    }
    if !review.review_all_tools && !RiskClassifier::needs_review(name) {
        return ReviewGateResult::NotApplicable;
    }
    let class = RiskClassifier::class(name, args);
    let store = core.review_store.clone();

    if store.denies(&class) {
        return ReviewGateResult::Blocked(finish_tool_error(
            core,
            setup,
            call_id.to_string(),
            name,
            format!("该类操作（{class}）在拒绝期内，已自动拒绝。可在设置页清除审查记忆后再试。"),
        ));
    }
    if store.allows(&class) {
        return ReviewGateResult::Proceed;
    }

    // 计算机操作类工具：固定 2 分、跳过 LLM 评分（省 token/延迟），只走 allow 缓存/一次性确认。
    let fixed_computer = RiskClassifier::is_computer_use(name);
    let verdict: RiskVerdict = if fixed_computer {
        RiskClassifier::fixed_computer_verdict()
    } else {
        let llm = core.llm();
        let judge_fut = core.judge.judge(&llm, settings, name, args);
        let cancel = wait_any_cancel(setup.cancel_rx.clone(), setup.parent_rx.clone());
        tokio::select! {
            verdict = judge_fut => verdict,
            _ = cancel => {
                push_tool_message(
                    core,
                    setup,
                    call_id,
                    name,
                    "工具执行被用户取消（风险审查阶段）。",
                    false,
                );
                if main {
                    core.emit(AgentEvent::ToolFinished {
                        call_id: call_id.to_string(),
                        name: name.to_string(),
                        ok: false,
                        output: "工具执行被用户取消（风险审查阶段）。".to_string(),
                        summary: Some("已取消".to_string()),
                        elapsed_ms: 0,
                    });
                }
                return ReviewGateResult::Blocked(ToolExec {
                    cancelled: true,
                    vision: None,
                });
            }
        }
    };

    // 评分结论（1 分也发，UI 记日志用）。
    core.emit(AgentEvent::RiskReviewed {
        tool: name.to_string(),
        level: verdict.level,
        class: verdict.class.clone(),
        reason: verdict.reason.clone(),
    });

    if verdict.level <= 1 {
        return ReviewGateResult::Proceed;
    }

    if !main {
        return ReviewGateResult::Blocked(finish_tool_error(
            core,
            setup,
            call_id.to_string(),
            name,
            format!(
                "子代理不能执行需要用户确认的风险操作（等级 {}：{}）。\
                 请让主对话先获得授权，或在设置里调整审查策略。",
                verdict.level, verdict.reason
            ),
        ));
    }

    let emphasized = verdict.level >= 4;
    let decision = request_approval(
        core,
        setup,
        call_id,
        name,
        args_raw,
        Some(&verdict),
        emphasized,
    )
    .await;

    if decision.is_approved() {
        // 2 分：允许后记住同类操作（TTL）。
        if verdict.level == 2 {
            store.grant(&verdict.class, review.allow_ttl_secs);
        }
        ReviewGateResult::Proceed
    } else {
        // 4 分被拒绝：记录"拒绝同类"（TTL）；3 分以下只拒绝本次。
        if verdict.level >= 4 && decision == ApproveDecision::DenyClass {
            store.deny(&verdict.class, review.deny_ttl_secs);
        }
        ReviewGateResult::Blocked(finish_tool_error(
            core,
            setup,
            call_id.to_string(),
            name,
            format!(
                "用户拒绝了这次操作（风险等级 {}：{}）。请换一种更安全的方法，或先向用户说明原因。",
                verdict.level, verdict.reason
            ),
        ))
    }
}

/// 弹窗请求用户审批；返回用户选择（超时/取消按拒绝）。
///
/// `risk = None` 表示旧的 approval_mode=ask 审批（风险字段为 0/空）。
async fn request_approval(
    core: &Arc<EngineCore>,
    setup: &TurnSetup,
    call_id: &str,
    name: &str,
    args_raw: &str,
    risk: Option<&RiskVerdict>,
    emphasized: bool,
) -> ApproveDecision {
    let (tx, rx) = oneshot::channel();
    core.approvals.lock().insert(call_id.to_string(), tx);
    // 需要用户确认时必须把窗口显示出来（避免用户在隐藏窗口时看不到弹窗）。
    core.restore_window_if_hidden("需要用户确认，恢复窗口");
    let (reason, risk_level, risk_reason, class) = match risk {
        Some(verdict) => (
            verdict.reason.clone(),
            verdict.level,
            verdict.reason.clone(),
            verdict.class.clone(),
        ),
        None => (
            format!("工具 {name} 属于危险操作（approval_mode = ask），确认参数后选择允许或拒绝。"),
            0,
            String::new(),
            String::new(),
        ),
    };
    core.emit(AgentEvent::ApprovalRequested {
        call_id: call_id.to_string(),
        name: name.to_string(),
        args: args_raw.to_string(),
        reason,
        risk_level,
        risk_reason,
        class,
        emphasized,
    });

    let decision = tokio::select! {
        result = rx => result.unwrap_or(ApproveDecision::Deny),
        _ = wait_any_cancel(setup.cancel_rx.clone(), setup.parent_rx.clone()) => ApproveDecision::Deny,
        _ = tokio::time::sleep(APPROVAL_TIMEOUT) => ApproveDecision::Deny,
    };
    core.approvals.lock().remove(call_id);
    decision
}

// ============================================================
// 内置工具：模式切换
// ============================================================

/// 把 `switch_mode` 等内置工具注册进共享 registry（由 `AgentEngine::start` 调用）。
pub(crate) fn register_builtin_tools(reg: &ToolRegistry, core: Weak<EngineCore>) {
    reg.register(Arc::new(SwitchModeTool { core: core.clone() }));
    reg.register(Arc::new(UpdatePlanTool { core: core.clone() }));
    reg.register(Arc::new(DeliverTool { core: core.clone() }));
    reg.register(Arc::new(WindowVisibilityTool { core }));
}

struct SwitchModeTool {
    core: Weak<EngineCore>,
}

/// 解析模式字符串（兼容中文与常见别名）。
pub(crate) fn parse_agent_mode(raw: &str) -> Option<AgentMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "daily" | "day" | "normal" | "日常" => Some(AgentMode::Daily),
        "coding" | "code" | "programming" | "编程" => Some(AgentMode::Coding),
        "computer_use" | "computeruse" | "computer-use" | "cu" | "计算机使用" => {
            Some(AgentMode::ComputerUse)
        }
        _ => None,
    }
}

#[async_trait]
impl Tool for SwitchModeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "switch_mode",
            "切换提示词模式（会立即影响后续请求的 system prompt）。\n\
             daily = 日常助手：注入人设 + 长期记忆 + 搜索/文件提示，计算机操作按需展开；\n\
             coding = 编程：不注入人设，保留编程规范 + 文件/命令提示，计算机操作按需展开；\n\
             computer_use = 计算机使用：不注入人设，计算机操作 + 文件/命令全部展开。\n\
             只在任务类型明显变化时切换；图片/截图任务优先 computer_use，写代码优先 coding。",
            json!({
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["daily", "coding", "computer_use"],
                        "description": "目标模式：daily / coding / computer_use。"
                    }
                },
                "required": ["mode"]
            }),
        )
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let raw = args
            .get("mode")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim();
        let Some(mode) = parse_agent_mode(raw) else {
            return ToolOutcome::err(format!(
                "未知模式「{raw}」。可选：daily（日常）、coding（编程）、computer_use（计算机使用）。"
            ));
        };
        let Some(core) = self.core.upgrade() else {
            return ToolOutcome::err("agent 引擎已关闭，无法切换模式。请重启应用。");
        };
        {
            core.cfg.settings.write().agent.mode = mode;
        }
        core.emit(AgentEvent::ModeChanged {
            mode: mode.id().to_string(),
            label: mode.label().to_string(),
        });
        let hint = match mode {
            AgentMode::Daily => "会注入人设与长期记忆，计算机操作按需展开。",
            AgentMode::Coding => "不注入人设，保留编程规范与文件/命令提示。",
            AgentMode::ComputerUse => "不注入人设，计算机操作与文件/命令提示全部展开。",
        };
        ToolOutcome::ok(format!(
            "已切换到{}模式（{}）。{hint}后续请求立即生效。",
            mode.label(),
            mode.id()
        ))
        .with_display(format!("切换到{}模式", mode.label()))
    }
}

// ============================================================
// 内置工具：Work Plan / 交付物 / 窗口可见性
// ============================================================

const PLAN_MAX_STEPS: usize = 12;
const PLAN_MAX_TITLE_CHARS: usize = 40;
const DELIVER_MAX_FILES: usize = 12;
const DELIVER_MAX_LINKS: usize = 10;
const DELIVER_MAX_FILE_BYTES: u64 = 200 * 1024 * 1024;
const DELIVER_MAX_TOTAL_BYTES: u64 = 500 * 1024 * 1024;

/// 计划标题截断到最多 40 字（含省略号）。
fn clip_plan_title(title: &str) -> String {
    let title = title.trim();
    if title.chars().count() <= PLAN_MAX_TITLE_CHARS {
        title.to_string()
    } else {
        // truncate_chars 会在末尾补一个 "…"，所以先只取 39 个字符。
        truncate_chars(title, PLAN_MAX_TITLE_CHARS.saturating_sub(1))
    }
}

/// 校验并规范化 `update_plan` 的步骤列表。
pub(crate) fn parse_plan_steps(args: &Value) -> anyhow::Result<Vec<PlanStep>> {
    let Some(raw_steps) = args.get("steps").and_then(|value| value.as_array()) else {
        anyhow::bail!(
            "缺少 steps 数组，例如 {{\"steps\":[{{\"title\":\"读代码\",\"status\":\"in_progress\"}}]}}。"
        );
    };
    if raw_steps.len() > PLAN_MAX_STEPS {
        anyhow::bail!(
            "计划最多 {PLAN_MAX_STEPS} 步，收到 {} 步。请合并步骤。",
            raw_steps.len()
        );
    }
    let mut steps = Vec::with_capacity(raw_steps.len());
    for (index, item) in raw_steps.iter().enumerate() {
        let title = item
            .get("title")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim();
        if title.is_empty() {
            anyhow::bail!("第 {} 步缺少 title。", index + 1);
        }
        let raw_status = item
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("pending")
            .trim();
        let status = match raw_status.to_ascii_lowercase().as_str() {
            "pending" | "todo" | "待办" | "未开始" => "pending",
            "in_progress" | "in-progress" | "doing" | "进行中" | "正在做" => "in_progress",
            "done" | "completed" | "完成" | "已完成" => "done",
            other => anyhow::bail!(
                "第 {} 步的 status 非法：「{other}」。只允许 pending / in_progress / done。",
                index + 1
            ),
        };
        steps.push(PlanStep {
            title: clip_plan_title(title),
            status: status.to_string(),
        });
    }
    Ok(steps)
}

/// 交付物文件类型（按扩展名）。
fn deliver_kind(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "svg" => "image",
        "mp4" | "mov" | "avi" | "mkv" | "webm" | "m4v" => "video",
        _ => "file",
    }
}

/// 禁止交付的敏感文件（系统目录/密钥/凭据）。返回中文原因。
fn forbidden_deliver_reason(path: &Path) -> Option<String> {
    let normalized = path.display().to_string().replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "key" | "pem" | "pfx" | "p12" | "keystore") {
        return Some("密钥/证书类文件".to_string());
    }
    if file_name == ".env"
        || file_name.starts_with(".env.")
        || file_name.contains("credentials")
        || file_name.starts_with("id_rsa")
        || file_name.starts_with("id_ed25519")
        || file_name.ends_with(".kdbx")
    {
        return Some("密钥/凭据类文件".to_string());
    }
    if lower.contains("/.ssh/")
        || lower.contains("/.gnupg/")
        || lower.contains("/windows/system32")
        || lower.contains("/windows/syswow64")
        || lower.contains("/system volume information")
        || lower.starts_with("c:/windows/")
    {
        return Some("系统目录或私密目录".to_string());
    }
    None
}

/// 校验 `deliver` 参数并生成事件载荷。
pub(crate) fn collect_deliver_payload(
    args: &Value,
) -> anyhow::Result<(Vec<DeliverFile>, Vec<String>, String)> {
    let raw_files = args
        .get("files")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let raw_links = args
        .get("links")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if raw_files.is_empty() && raw_links.is_empty() {
        anyhow::bail!("deliver 至少需要 files 或 links 其中一项。");
    }
    if raw_files.len() > DELIVER_MAX_FILES {
        anyhow::bail!(
            "一次最多交付 {DELIVER_MAX_FILES} 个文件，收到 {} 个。",
            raw_files.len()
        );
    }
    let mut files = Vec::with_capacity(raw_files.len());
    let mut total: u64 = 0;
    for item in &raw_files {
        let raw = item
            .as_str()
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .ok_or_else(|| anyhow::anyhow!("files 里有一项不是非空字符串路径。"))?;
        let path = Path::new(raw);
        if !path.is_absolute() {
            anyhow::bail!("交付文件必须是绝对路径：{raw}");
        }
        if let Some(reason) = forbidden_deliver_reason(path) {
            anyhow::bail!(
                "出于安全考虑，不能交付这个文件（{reason}）：{}",
                path.display()
            );
        }
        let meta = std::fs::metadata(path).map_err(|error| {
            anyhow::anyhow!("文件不存在或无法读取：{}（{error}）", path.display())
        })?;
        if !meta.is_file() {
            anyhow::bail!("只能交付文件，不能交付目录：{}", path.display());
        }
        if meta.len() > DELIVER_MAX_FILE_BYTES {
            anyhow::bail!(
                "单个交付文件不能超过 {} MB：{}",
                DELIVER_MAX_FILE_BYTES / (1024 * 1024),
                path.display()
            );
        }
        total = total.saturating_add(meta.len());
        if total > DELIVER_MAX_TOTAL_BYTES {
            anyhow::bail!(
                "交付文件合计不能超过 {} MB。请分批交付。",
                DELIVER_MAX_TOTAL_BYTES / (1024 * 1024)
            );
        }
        files.push(DeliverFile {
            path: path.display().to_string(),
            name: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(raw)
                .to_string(),
            kind: deliver_kind(path).to_string(),
            size: meta.len(),
        });
    }

    if raw_links.len() > DELIVER_MAX_LINKS {
        anyhow::bail!(
            "一次最多交付 {DELIVER_MAX_LINKS} 个链接，收到 {} 个。",
            raw_links.len()
        );
    }
    let mut links = Vec::with_capacity(raw_links.len());
    for item in &raw_links {
        let raw = item
            .as_str()
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .ok_or_else(|| anyhow::anyhow!("links 里有一项不是非空字符串。"))?;
        let parsed = url::Url::parse(raw)
            .map_err(|error| anyhow::anyhow!("链接不合法：{raw}（{error}）"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            anyhow::bail!("只支持 http/https 链接，收到：{raw}");
        }
        links.push(parsed.to_string());
    }

    let caption = args
        .get("caption")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim();
    let caption = truncate_chars(caption, 500);
    Ok((files, links, caption))
}

struct UpdatePlanTool {
    core: Weak<EngineCore>,
}

#[async_trait]
impl Tool for UpdatePlanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "update_plan",
            "更新当前任务的 Work Plan（给用户看进度）。多步任务先列计划，完成一步更新一步。\n\
             每步 title 最多 40 字、最多 12 步；status 只能是 pending / in_progress / done；\
             steps 传空数组表示清空计划。",
            json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "description": "计划步骤（最多 12 步）",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string", "description": "步骤标题（≤40 字）"},
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "done"],
                                    "description": "步骤状态"
                                }
                            },
                            "required": ["title", "status"]
                        }
                    }
                },
                "required": ["steps"]
            }),
        )
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let steps = match parse_plan_steps(&args) {
            Ok(steps) => steps,
            Err(error) => return ToolOutcome::err(format!("{error}")),
        };
        let Some(core) = self.core.upgrade() else {
            return ToolOutcome::err("agent 引擎已关闭，无法更新计划。");
        };
        let conversation_id = core.conversation_id.lock().clone();
        core.apply_plan(conversation_id.as_deref(), steps.clone());
        if steps.is_empty() {
            return ToolOutcome::ok("已清空当前任务计划。").with_display("清空计划");
        }
        let done = steps.iter().filter(|step| step.status == "done").count();
        let summary = steps
            .iter()
            .map(|step| {
                let mark = match step.status.as_str() {
                    "done" => "[x]",
                    "in_progress" => "[>]",
                    _ => "[ ]",
                };
                format!("{mark} {}", step.title)
            })
            .collect::<Vec<_>>()
            .join("\n");
        ToolOutcome::ok(format!(
            "计划已更新（{done}/{} 完成）：\n{summary}",
            steps.len()
        ))
        .with_display(format!("计划更新（{done}/{}）", steps.len()))
    }
}

struct DeliverTool {
    core: Weak<EngineCore>,
}

#[async_trait]
impl Tool for DeliverTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "deliver",
            "把本任务产生的成果交付给用户：图片/视频/文件（绝对路径）或链接。UI 会渲染预览并提供打开按钮。\n\
             只交本任务真正产生或引用的文件；禁止交付系统目录、密钥、.env、.ssh 等敏感文件。\n\
             单文件 ≤200MB，合计 ≤500MB，最多 12 个文件 + 10 个链接。",
            json!({
                "type": "object",
                "properties": {
                    "files": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "要交付的文件的绝对路径"
                    },
                    "links": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "要交付的 http/https 链接"
                    },
                    "caption": {
                        "type": "string",
                        "description": "可选说明（≤500 字）"
                    }
                }
            }),
        )
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let (files, links, caption) = match collect_deliver_payload(&args) {
            Ok(payload) => payload,
            Err(error) => return ToolOutcome::err(format!("{error}")),
        };
        let Some(core) = self.core.upgrade() else {
            return ToolOutcome::err("agent 引擎已关闭，无法交付。");
        };
        let mut parts = Vec::new();
        if !files.is_empty() {
            parts.push(format!("{} 个文件", files.len()));
        }
        if !links.is_empty() {
            parts.push(format!("{} 个链接", links.len()));
        }
        core.emit(AgentEvent::Delivered {
            files: files.clone(),
            links: links.clone(),
            caption: caption.clone(),
        });
        let names = files
            .iter()
            .map(|file| file.name.as_str())
            .collect::<Vec<_>>()
            .join("、");
        ToolOutcome::ok(format!(
            "已交付{}。{}{}",
            parts.join("、"),
            if names.is_empty() {
                String::new()
            } else {
                format!("文件：{names}。")
            },
            if caption.is_empty() {
                String::new()
            } else {
                format!("说明：{caption}")
            }
        ))
        .with_display(format!("交付{}", parts.join("、")))
    }
}

struct WindowVisibilityTool {
    core: Weak<EngineCore>,
}

#[async_trait]
impl Tool for WindowVisibilityTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "set_window_visibility",
            "隐藏或恢复主窗口。后台任务/长时间自动化操作前可隐藏窗口减少干扰；\
             **任务完成、出错或需要用户确认时必须恢复窗口**。",
            json!({
                "type": "object",
                "properties": {
                    "hidden": {
                        "type": "boolean",
                        "description": "true=隐藏主窗口；false=恢复显示"
                    }
                },
                "required": ["hidden"]
            }),
        )
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let Some(hidden) = args.get("hidden").and_then(|value| value.as_bool()) else {
            return ToolOutcome::err("缺少 hidden 参数（true/false）。");
        };
        let Some(core) = self.core.upgrade() else {
            return ToolOutcome::err("agent 引擎已关闭，无法控制窗口。");
        };
        let reason = if hidden {
            "模型请求后台执行，隐藏窗口"
        } else {
            "模型请求恢复窗口"
        };
        let hooked = core.set_window_visibility(hidden, reason);
        if hooked {
            ToolOutcome::ok(if hidden {
                "已隐藏主窗口；完成后请调用 set_window_visibility {hidden:false} 恢复。"
            } else {
                "已恢复主窗口。"
            })
            .with_display(if hidden {
                "隐藏窗口"
            } else {
                "显示窗口"
            })
        } else {
            ToolOutcome::err("当前界面没有提供窗口控制回调，无法隐藏/恢复窗口；请直接继续任务。")
        }
    }
}

// ============================================================
// 取消辅助
// ============================================================

/// 等待任意一个取消通道变成 true。
async fn wait_any_cancel(mut own: watch::Receiver<bool>, parent: Option<watch::Receiver<bool>>) {
    if *own.borrow() {
        return;
    }
    if let Some(parent) = &parent {
        if *parent.borrow() {
            return;
        }
    }
    match parent {
        Some(mut parent) => {
            tokio::select! {
                _ = wait_true(&mut own) => {}
                _ = wait_true(&mut parent) => {}
            }
        }
        None => wait_true(&mut own).await,
    }
}

async fn wait_true(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            // 发送端被释放（应用退出）：按取消处理，让等待点尽快返回。
            return;
        }
    }
}

fn emit_error(core: &EngineCore, mode: &LoopMode, message: &str) {
    match mode {
        LoopMode::Main { .. } => core.emit(AgentEvent::Error(message.to_string())),
        LoopMode::Subagent { id } => core.emit(AgentEvent::SubagentActivity {
            id: id.clone(),
            line: format!("[err] {}", truncate_chars(message, 300)),
        }),
    }
}

fn emit_activity(core: &EngineCore, mode: &LoopMode, line: impl Into<String>) {
    if let LoopMode::Subagent { id } = mode {
        core.emit(AgentEvent::SubagentActivity {
            id: id.clone(),
            line: line.into(),
        });
    }
}

// ============================================================
// AgentHost：给工具用的宿主能力
// ============================================================

struct EngineHost {
    core: Arc<EngineCore>,
    /// 当前轮次（或子代理）自己的取消标记。
    cancel: Arc<AtomicBool>,
    /// 子代理要额外继承父轮次的取消标记。
    parent_cancel: Option<Arc<AtomicBool>>,
    is_subagent: bool,
    subagent_id: Option<String>,
}

impl AgentHost for EngineHost {
    fn settings(&self) -> Arc<RwLock<Settings>> {
        self.core.cfg.settings.clone()
    }

    fn paths(&self) -> Paths {
        self.core.cfg.paths.clone()
    }

    fn memory(&self) -> Arc<dyn MemoryApi> {
        let memory: Arc<dyn MemoryApi> = self.core.cfg.memory.clone();
        memory
    }

    fn emit(&self, event: AgentEvent) {
        if !self.is_subagent {
            self.core.emit(event);
            return;
        }
        // 子代理：把事件压缩成 SubagentActivity 行，避免把整段文本灌进主聊天流。
        let Some(id) = &self.subagent_id else {
            return;
        };
        let line = match event {
            AgentEvent::Phase(text) => text,
            AgentEvent::TextDelta(_) | AgentEvent::ReasoningDelta(_) => return,
            AgentEvent::ToolOutput { chunk, .. } => {
                let first = chunk
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                if first.is_empty() {
                    return;
                }
                format!("输出：{}", truncate_chars(first, 160))
            }
            AgentEvent::ToolStarted { name, .. } => format!("调用工具 {name}"),
            AgentEvent::ToolFinished {
                name,
                ok,
                summary,
                elapsed_ms,
                ..
            } => format!(
                "工具 {name} {}（{}）{}",
                if ok { "完成" } else { "失败" },
                human_duration_ms(elapsed_ms),
                summary.unwrap_or_default()
            ),
            AgentEvent::MemoryChanged { action, content } => {
                format!("记忆 {action}：{}", truncate_chars(&content, 80))
            }
            AgentEvent::Error(text) => format!("[err] {}", truncate_chars(&text, 300)),
            AgentEvent::Notice(text) => text,
            AgentEvent::SubagentStarted { label, .. } => format!("启动子子代理 {label}"),
            AgentEvent::SubagentActivity { line, .. } => line,
            AgentEvent::SubagentFinished {
                label, ok, summary, ..
            } => format!(
                "子子代理 {label} {}：{}",
                if ok { "完成" } else { "失败" },
                truncate_chars(&summary, 120)
            ),
            _ => return,
        };
        self.core.emit(AgentEvent::SubagentActivity {
            id: id.clone(),
            line,
        });
    }

    fn spawn_subagent(&self, req: SubagentRequest) -> anyhow::Result<String> {
        if self.is_subagent {
            bail!("子代理不能再启动子代理（避免无限递归）。请把任务合并到当前子代理里完成。");
        }
        let result = crate::subagent::spawn(self.core.clone(), req, self.cancel.clone())?;
        Ok(result.id)
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
            || self
                .parent_cancel
                .as_ref()
                .map(|flag| flag.load(Ordering::SeqCst))
                .unwrap_or(false)
    }
}

// ============================================================
// 子代理执行（由 subagent::spawn 调用）
// ============================================================

pub(crate) async fn run_subagent(
    core: Arc<EngineCore>,
    id: String,
    req: SubagentRequest,
    parent_cancel: Arc<AtomicBool>,
    parent_rx: watch::Receiver<bool>,
) -> SubagentOutcome {
    let started = Instant::now();
    let label = if req.label.trim().is_empty() {
        "子代理".to_string()
    } else {
        req.label.clone()
    };
    core.emit(AgentEvent::SubagentStarted {
        id: id.clone(),
        label: label.clone(),
        task: req.task.clone(),
    });
    core.subagent_total.fetch_add(1, Ordering::SeqCst);
    let (active, total) = core.subagent_counts();
    core.emit(AgentEvent::SubagentCount { active, total });

    let settings = core.settings_snapshot();
    let persona_id = req
        .persona
        .clone()
        .filter(|persona| !persona.trim().is_empty())
        .unwrap_or_else(|| settings.agent.persona.clone());

    let mut tools = safe_tool_specs(&core.cfg.registry, Some(&req.tools));
    // 递归保护：子代理的工具集里不出现 spawn_subagent / subagent_list。
    tools.retain(|spec| spec.name != "spawn_subagent" && spec.name != "subagent_list");
    let known = core.cfg.registry.names();
    let missing: Vec<&String> = req
        .tools
        .iter()
        .filter(|name| !known.contains(*name))
        .collect();
    if !missing.is_empty() {
        let names = missing
            .iter()
            .map(|name| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        core.emit(AgentEvent::SubagentActivity {
            id: id.clone(),
            line: format!("忽略不存在的工具：{names}"),
        });
    }

    // 子代理自己的取消通道（目前没有主动取消入口，保留给未来的超时/中止）。
    let (own_tx, own_rx) = watch::channel(false);
    let own_cancel = Arc::new(AtomicBool::new(false));
    let host: Arc<dyn AgentHost> = Arc::new(EngineHost {
        core: core.clone(),
        cancel: own_cancel,
        parent_cancel: Some(parent_cancel),
        is_subagent: true,
        subagent_id: Some(id.clone()),
    });

    let setup = TurnSetup {
        mode: LoopMode::Subagent { id: id.clone() },
        persona_id,
        user_text: req.task.clone(),
        user_images: Vec::new(),
        tools,
        tool_allow: req.tools.clone(),
        forbid_tools: vec!["spawn_subagent".to_string(), "subagent_list".to_string()],
        max_iterations: req.max_iterations.clamp(1, 50),
        host,
        cancel_rx: own_rx,
        parent_rx: Some(parent_rx),
        history: Arc::new(RwLock::new(Vec::new())),
        // 子代理的运行不写进用户会话历史（避免污染对话记录）。
        conversation_id: None,
    };

    let result = run_loop(&core, setup).await;
    drop(own_tx);

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let ok = result.error.is_none() && !result.cancelled;
    let summary = if let Some(error) = &result.error {
        format!("子代理执行失败：{error}")
    } else if result.cancelled {
        "子代理已取消（父轮次被取消或应用正在退出）。".to_string()
    } else if result.text.trim().is_empty() {
        "子代理没有产生文本汇报（可能只执行了工具）。".to_string()
    } else {
        truncate_chars(result.text.trim(), 1500)
    };

    core.emit(AgentEvent::SubagentFinished {
        id: id.clone(),
        label: label.clone(),
        ok,
        summary: summary.clone(),
        elapsed_ms,
    });
    let (active, total) = core.subagent_counts();
    core.emit(AgentEvent::SubagentCount { active, total });

    SubagentOutcome {
        id,
        label,
        ok,
        summary,
        elapsed_ms,
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FunctionCall;

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    fn message(role: Role, content: impl Into<String>) -> ChatMessage {
        let mut message = ChatMessage::default();
        message.role = role;
        message.content = Some(content.into());
        message
    }

    #[test]
    fn trim_history_respects_limit_and_tool_ordering() {
        let history = vec![
            message(Role::User, "问题一"),
            ChatMessage::assistant_with_calls(
                Some("调用工具".into()),
                None,
                vec![tool_call("c1", "read_file")],
            ),
            ChatMessage::tool_result("c1", "read_file", "文件内容", true),
            message(Role::Assistant, "回答一"),
        ];
        // limit = 2 -> 最后两条是 tool + assistant，开头的 tool 必须被丢掉。
        let kept = trim_history(&history, 2, 100_000);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].role, Role::Assistant);
        // limit = 0 表示不限条数。
        assert_eq!(trim_history(&history, 0, 100_000).len(), 4);
    }

    #[test]
    fn trim_history_drops_old_messages_under_budget() {
        let history = vec![
            message(Role::User, "很长很长的历史内容".repeat(50)),
            message(Role::Assistant, "同样很长的回答".repeat(50)),
            message(Role::User, "最新问题"),
        ];
        let kept = trim_history(&history, 0, 20);
        assert_eq!(kept.len(), 1, "预算很小时只保留最后一条");
        assert_eq!(kept[0].content.as_deref(), Some("最新问题"));
    }

    #[test]
    fn trim_history_never_starts_with_tool() {
        let history = vec![
            ChatMessage::assistant_with_calls(None, None, vec![tool_call("c1", "x")]),
            ChatMessage::tool_result("c1", "x", "结果", true),
            ChatMessage::tool_result("c2", "y", "结果2", true),
        ];
        let kept = trim_history(&history, 0, 100_000);
        assert_ne!(kept.first().map(|m| m.role), Some(Role::Tool));
    }

    #[test]
    fn message_token_estimate_is_positive() {
        let msg = ChatMessage::assistant_with_calls(
            Some("你好".into()),
            Some("思考".into()),
            vec![tool_call("c1", "read_file")],
        );
        assert!(estimate_message_tokens(&msg) > 10);
    }

    #[test]
    fn preview_picks_command_and_path() {
        let args = serde_json::json!({"command": "cargo test -p neosonia-core"});
        assert_eq!(
            preview_for("run_command", &args).as_deref(),
            Some("cargo test -p neosonia-core")
        );
        let args = serde_json::json!({"path": "E:/NeoSonia/README.md"});
        assert_eq!(
            preview_for("read_file", &args).as_deref(),
            Some("E:/NeoSonia/README.md")
        );
        assert!(preview_for("read_file", &serde_json::json!({})).is_none());
    }

    #[test]
    fn loop_mode_helpers() {
        let main = LoopMode::Main {
            turn_id: "t1".into(),
        };
        assert!(main.is_main());
        assert_eq!(main.id(), "t1");
        let sub = LoopMode::Subagent { id: "sa-1".into() };
        assert!(!sub.is_main());
        assert_eq!(sub.id(), "sa-1");
    }

    #[test]
    fn repair_history_backfills_missing_tool_results() {
        let history = RwLock::new(vec![
            message(Role::User, "问题"),
            ChatMessage::assistant_with_calls(
                None,
                None,
                vec![tool_call("c1", "read_file"), tool_call("c2", "run_command")],
            ),
            ChatMessage::tool_result("c1", "read_file", "ok", true),
            message(Role::Assistant, "完成"),
        ]);
        repair_history(&history);
        let guard = history.read();
        assert_eq!(guard.len(), 5, "应补一条缺失的 tool 结果");
        assert_eq!(guard[3].role, Role::Tool);
        assert_eq!(guard[3].tool_call_id.as_deref(), Some("c2"));
        assert_eq!(guard[3].tool_ok, Some(false));
        assert_eq!(guard[4].role, Role::Assistant);
    }

    #[test]
    fn repair_history_keeps_valid_history_untouched() {
        let history = RwLock::new(vec![
            message(Role::User, "问题"),
            ChatMessage::assistant_with_calls(None, None, vec![tool_call("c1", "read_file")]),
            ChatMessage::tool_result("c1", "read_file", "ok", true),
            message(Role::Assistant, "完成"),
        ]);
        repair_history(&history);
        assert_eq!(history.read().len(), 4);
    }

    #[test]
    fn tap_tokens_are_roughly_sane() {
        assert!(estimate_tokens("hello world") > 0);
        assert!(crate::util::now_ms() > 0);
    }

    // ---------------- 第二阶段：视觉 / 会话 ---------------- //

    /// 构造一个用于测试的 EngineCore（不启动运行时、不联网）。
    fn test_core(
        root: &std::path::Path,
    ) -> (
        Arc<EngineCore>,
        Arc<crate::conversation::ConversationStore>,
        Arc<Mutex<Vec<AgentEvent>>>,
    ) {
        test_core_with_judge(root, Arc::new(FakeJudge { level: 1 }))
    }

    /// 用指定评分器构造 EngineCore（假 judge，不联网）。
    fn test_core_with_judge(
        root: &std::path::Path,
        judge: Arc<dyn RiskJudge>,
    ) -> (
        Arc<EngineCore>,
        Arc<crate::conversation::ConversationStore>,
        Arc<Mutex<Vec<AgentEvent>>>,
    ) {
        let paths = Paths::from_root(root);
        let _ = paths.ensure();
        let settings = Arc::new(RwLock::new(Settings::default()));
        let memory = Arc::new(MemoryStore::open(&paths.memory_db).unwrap());
        let conversations = Arc::new(
            crate::conversation::ConversationStore::open(&paths.conversations_db).unwrap(),
        );
        let cfg = EngineConfig {
            settings,
            paths: paths.clone(),
            registry: ToolRegistry::new(),
            memory,
            personas: PersonaStore::load(&paths.personas),
            conversations: conversations.clone(),
            skills: SkillStore::load(&paths.skills),
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink: EventSink = {
            let events = events.clone();
            Arc::new(move |event: AgentEvent| events.lock().push(event))
        };
        let core = Arc::new(EngineCore::new_with_judge(cfg, sink, judge));
        (core, conversations, events)
    }

    /// 假评分器：固定返回指定风险等级，不联网。
    struct FakeJudge {
        level: u8,
    }

    #[async_trait::async_trait]
    impl RiskJudge for FakeJudge {
        async fn judge(
            &self,
            _llm: &LlmClient,
            _settings: &Settings,
            tool: &str,
            args: &Value,
        ) -> RiskVerdict {
            RiskVerdict::new(
                self.level,
                RiskClassifier::class(tool, args),
                "测试用风险评分",
            )
        }
    }

    fn test_setup(
        core: &Arc<EngineCore>,
        conversation_id: Option<String>,
    ) -> (TurnSetup, watch::Sender<bool>) {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let host: Arc<dyn AgentHost> = Arc::new(EngineHost {
            core: core.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
            parent_cancel: None,
            is_subagent: false,
            subagent_id: None,
        });
        let setup = TurnSetup {
            mode: LoopMode::Main {
                turn_id: "t-test".into(),
            },
            persona_id: "default".into(),
            user_text: "测试".into(),
            user_images: Vec::new(),
            tools: Vec::new(),
            tool_allow: Vec::new(),
            forbid_tools: Vec::new(),
            max_iterations: 2,
            host,
            cancel_rx,
            parent_rx: None,
            history: core.history.clone(),
            conversation_id,
        };
        (setup, cancel_tx)
    }

    #[test]
    fn extract_image_markers_from_tool_output() {
        let text = "已保存: [screenshot:E:\\NeoSonia\\logs\\a.png]\n\
                    再看 [IMAGE:C:/tmp/b.jpg] 和重复 [image:C:/tmp/b.jpg]\n\
                    无关内容 [image:]";
        let markers = extract_image_markers(text);
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0], "E:\\NeoSonia\\logs\\a.png");
        assert_eq!(markers[1], "C:/tmp/b.jpg");
        assert!(extract_image_markers("没有标记").is_empty());
        // 未闭合的标记不能 panic，也不能产出半截路径。
        assert!(extract_image_markers("[screenshot:C:/x.png").is_empty());
    }

    #[test]
    fn vision_prompt_mentions_size_and_physical_coordinates() {
        let images = vec![ImagePart::png(
            "AAAA".into(),
            1280,
            720,
            Some("a.png".into()),
        )];
        let text = vision_prompt_text(&images);
        assert!(text.starts_with(VISION_PROMPT_PREFIX));
        assert!(text.contains("1280x720"), "必须把附图尺寸写进提示");
        assert!(text.contains("屏幕物理坐标"));
        assert!(text.contains("截图原始宽"));
        assert!(text.contains("ui_act"), "应提示优先用控件名操作");
    }

    /// 构造指定模式/展开状态的 system prompt（不启动运行时、不联网）。
    fn test_prompt_with(mode: AgentMode, computer_expanded: bool) -> String {
        let dir = tempfile::tempdir().unwrap();
        let (core, _conversations, _events) = test_core(dir.path());
        {
            core.cfg.settings.write().agent.mode = mode;
        }
        core.build_system_prompt("打开记事本", "default", false, computer_expanded)
    }

    fn count_section(prompt: &str, header: &str) -> usize {
        prompt.matches(header).count()
    }

    #[test]
    fn non_daily_modes_require_switching_back_for_daily_content() {
        // 用户要求：天气/日程/生活常识等日常内容必须先切回 daily 再回答。
        for mode in [AgentMode::Coding, AgentMode::ComputerUse] {
            let prompt = test_prompt_with(mode, false);
            assert!(
                prompt.contains("必须先 switch_mode") && prompt.contains("日常内容"),
                "{} 模式应要求日常内容先切回 daily",
                mode.id()
            );
        }
        let daily = test_prompt_with(AgentMode::Daily, false);
        assert!(!daily.contains("## 日常问题"), "daily 模式不需要切回提示");
    }

    #[test]
    fn computer_use_prompt_has_computer_section() {
        let prompt = test_prompt_with(AgentMode::ComputerUse, false);
        assert!(prompt.contains("## 计算机操作"));
        assert!(prompt.contains("ui_act"));
        assert!(prompt.contains("keyboard_type"));
        assert!(prompt.contains("keyboard_combo"));
        assert!(prompt.contains("屏幕坐标 = 图片坐标"));
        assert!(prompt.contains("## 文件与命令"));
        assert_eq!(count_section(&prompt, "## 计算机操作"), 1);
    }

    #[test]
    fn computer_use_prompt_mentions_grid_screenshot_and_mouse_options() {
        let prompt = test_prompt_with(AgentMode::ComputerUse, false);
        assert!(
            prompt.contains("screenshot {grid:true}"),
            "应提示用 grid 截图读精确坐标"
        );
        assert!(prompt.contains("100px"), "应说明网格是 100px 刻度");
        assert!(prompt.contains("mouse_click {button"));
        assert!(prompt.contains("double:true"), "应说明双击参数");
        assert!(prompt.contains("mouse_scroll {direction"));
        assert!(prompt.contains("direction:\"up\"|\"down\""));
        assert!(
            prompt.contains("坐标就是屏幕物理像素"),
            "grid 截图下坐标即物理像素"
        );
    }

    #[test]
    fn coding_prompt_expands_commands_but_not_computer_use() {
        let prompt = test_prompt_with(AgentMode::Coding, false);
        assert!(prompt.contains("## 编程规范"));
        assert!(prompt.contains("## 文件与命令"));
        assert!(prompt.contains("run_command {background:true}"));
        assert!(prompt.contains("job_status"));
        assert!(prompt.contains("read_job_output"));
        assert!(prompt.contains("send_job_input"));
        assert!(prompt.contains("kill_job"));
        assert!(prompt.contains("60 秒"), "应提示前台命令 60 秒上限");
        // coding 模式不注入人设正文，也不常驻计算机操作段。
        assert!(
            !prompt.contains("先行动，后解释"),
            "coding 不应注入人设正文"
        );
        assert!(!prompt.contains("## 计算机操作"));
        assert!(!prompt.contains("## 文件与搜索"));
    }

    #[test]
    fn daily_prompt_injects_persona_and_shows_basic_computer_section() {
        let prompt = test_prompt_with(AgentMode::Daily, false);
        assert!(prompt.contains("先行动，后解释"), "daily 应注入人设正文");
        assert!(prompt.contains("## 文件与搜索"));
        // daily 默认展开基础版（列内置工具、禁用 python/PIL 截图）。
        assert!(prompt.contains("## 计算机操作（基础版）"));
        assert!(prompt.contains("不要用 python/PIL 截图"));
        assert!(prompt.contains("ui_screenshot"));
        // 但完整版（grid 截图等）不常驻。
        assert!(!prompt.contains("screenshot {grid:true}"));
        assert!(!prompt.contains("## 编程规范"));
        assert!(!prompt.contains("## 文件与命令"));
    }

    #[test]
    fn computer_section_expands_once_on_demand() {
        // daily 默认：基础版，完整版不出现。
        let daily_basic = test_prompt_with(AgentMode::Daily, false);
        assert!(daily_basic.contains("## 计算机操作（基础版）"));
        assert!(!daily_basic.contains("screenshot {grid:true}"));

        // daily 用过计算机工具后：完整版（grid 坐标等）。
        let daily_full = test_prompt_with(AgentMode::Daily, true);
        assert!(
            daily_full.contains("screenshot {grid:true}"),
            "daily 用过计算机工具后应展开完整版"
        );
        assert!(!daily_full.contains("## 计算机操作（基础版）"));

        // coding 默认没有计算机段；用过之后展开完整版。
        let coding_basic = test_prompt_with(AgentMode::Coding, false);
        assert!(!coding_basic.contains("## 计算机操作"));
        let coding_full = test_prompt_with(AgentMode::Coding, true);
        assert!(coding_full.contains("screenshot {grid:true}"));

        // computer_use 常驻完整版且不重复。
        let cu = test_prompt_with(AgentMode::ComputerUse, true);
        assert_eq!(count_section(&cu, "## 计算机操作"), 1);
        assert!(!cu.contains("## 计算机操作（基础版）"));
    }

    #[test]
    fn all_modes_require_switch_mode_before_coding_or_computer_use() {
        for mode in [AgentMode::Daily, AgentMode::Coding, AgentMode::ComputerUse] {
            let prompt = test_prompt_with(mode, false);
            assert!(
                prompt.contains("## 模式切换（硬性要求）"),
                "{} 模式缺少模式切换硬性要求",
                mode.id()
            );
            assert!(prompt.contains("switch_mode {\"mode\":\"coding\"}"));
            assert!(prompt.contains("switch_mode {\"mode\":\"computer_use\"}"));
        }
        // coding 提示测完可切 computer_use 验证。
        let coding = test_prompt_with(AgentMode::Coding, false);
        assert!(coding.contains("用真实界面跑一遍验证效果"));
        // coding 提示先 update_plan。
        assert!(coding.contains("update_plan"));
    }

    #[test]
    fn computer_use_seen_is_remembered_per_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let (core, _conversations, _events) = test_core(dir.path());
        assert!(!core.computer_used_in_conversation(Some("conv-a")));
        core.mark_computer_used(Some("conv-a"));
        assert!(core.computer_used_in_conversation(Some("conv-a")));
        // 其它会话不受影响（会话隔离）。
        assert!(!core.computer_used_in_conversation(Some("conv-b")));
        // 空会话 id 不记忆（子代理）。
        core.mark_computer_used(None);
        assert!(!core.computer_used_in_conversation(None));
    }

    #[test]
    fn skills_are_filtered_by_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (core, _conversations, _events) = test_core(dir.path());
        let source = dir.path().join("coder-skill.md");
        std::fs::write(
            &source,
            "---\nid: coder-skill\nname: 编程专用技能\ndescription: 写代码\n---\n\n正文\n",
        )
        .unwrap();
        core.cfg.skills.install_from_path(&source).unwrap();
        core.cfg
            .skills
            .rewrite_modes("coder-skill", &["coding".to_string()])
            .unwrap();
        core.cfg.skills.reload();

        let daily = core.build_system_prompt("写代码", "default", false, false);
        assert!(!daily.contains("编程专用技能"), "daily 不列 coding 技能");
        {
            core.cfg.settings.write().agent.mode = AgentMode::Coding;
        }
        let coding = core.build_system_prompt("写代码", "default", false, false);
        assert!(coding.contains("编程专用技能"), "coding 应列出 coding 技能");
    }

    /// 带调用计数的假评分器（验证某些路径不触发 LLM 评分）。
    struct CountingJudge {
        level: u8,
        calls: Arc<AtomicU32>,
    }

    #[async_trait::async_trait]
    impl RiskJudge for CountingJudge {
        async fn judge(
            &self,
            _llm: &LlmClient,
            _settings: &Settings,
            tool: &str,
            args: &Value,
        ) -> RiskVerdict {
            self.calls.fetch_add(1, Ordering::SeqCst);
            RiskVerdict::new(
                self.level,
                RiskClassifier::class(tool, args),
                "测试用风险评分",
            )
        }
    }

    #[test]
    fn computer_use_tools_skip_llm_and_reuse_allow_cache() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        let (core, _conversations, events) = test_core_with_judge(
            dir.path(),
            Arc::new(CountingJudge {
                level: 2,
                calls: calls.clone(),
            }),
        );
        let (setup, _cancel_tx) = test_setup(&core, None);
        let args = json!({"x": 120, "y": 240});

        // 第一次点击：固定 2 分，弹一次确认（不调 LLM）。
        let gate = gate_run_tool(
            &core,
            &setup,
            "mouse_click",
            &args,
            "call-cu-1",
            Some(ApproveDecision::Approve),
        );
        assert!(matches!(gate, ReviewGateResult::Proceed));
        assert_eq!(approvals_count(&events), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "计算机工具不应走 LLM 评分");
        assert!(core.review_store.allows("computer_use"));

        // 第二次点击：allow 命中，不再弹窗、不再评分。
        let gate = gate_run_tool(&core, &setup, "mouse_click", &args, "call-cu-2", None);
        assert!(matches!(gate, ReviewGateResult::Proceed));
        assert_eq!(approvals_count(&events), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn python_script_allow_is_cached_by_semantic_class() {
        let dir = tempfile::tempdir().unwrap();
        let (core, _conversations, events) =
            test_core_with_judge(dir.path(), Arc::new(FakeJudge { level: 2 }));
        let (setup, _cancel_tx) = test_setup(&core, None);

        let first = json!({"command": "python tools/build.py --release"});
        let gate = gate_run_tool(
            &core,
            &setup,
            "run_command",
            &first,
            "call-py-1",
            Some(ApproveDecision::Approve),
        );
        assert!(matches!(gate, ReviewGateResult::Proceed));
        assert!(core.review_store.allows("shell:python"));
        let approvals = approvals_count(&events);

        // 换一个 python 脚本（同类语义）：不应再请求审批。
        let second = json!({"command": "python other/script.py arg1"});
        let gate = gate_run_tool(&core, &setup, "run_command", &second, "call-py-2", None);
        assert!(matches!(gate, ReviewGateResult::Proceed));
        assert_eq!(
            approvals_count(&events),
            approvals,
            "同一 shell:python 类第二次不应再弹审批"
        );
    }

    #[test]
    fn update_plan_validates_and_persists() {
        let steps = parse_plan_steps(&json!({
            "steps": [
                {"title": "读代码", "status": "done"},
                {"title": "改实现", "status": "in_progress"},
                {"title": "跑测试", "status": "pending"}
            ]
        }))
        .unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[1].status, "in_progress");
        // 非法状态、空标题、超过 12 步都要报错。
        assert!(
            parse_plan_steps(&json!({"steps": [{"title": "x", "status": "unknown"}]})).is_err()
        );
        assert!(
            parse_plan_steps(&json!({"steps": [{"title": " ", "status": "pending"}]})).is_err()
        );
        let too_many: Vec<Value> = (0..13)
            .map(|index| json!({"title": format!("步骤{index}"), "status": "pending"}))
            .collect();
        assert!(parse_plan_steps(&json!({"steps": too_many})).is_err());
        // 超长标题被截断到 40 字。
        let long = "很长的步骤标题".repeat(20);
        let step = parse_plan_steps(&json!({"steps": [{"title": long, "status": "pending"}]}))
            .unwrap()
            .remove(0);
        assert!(step.title.chars().count() <= 40);

        let dir = tempfile::tempdir().unwrap();
        let (core, conversations, events) = test_core(dir.path());
        core.apply_plan(Some("conv-plan"), steps.clone());
        assert_eq!(conversations.plan("conv-plan").unwrap(), steps);
        assert!(events.lock().iter().any(|event| matches!(
            event,
            AgentEvent::PlanUpdated { steps: event_steps } if event_steps.len() == 3
        )));
        // 空计划 = 清除。
        core.apply_plan(Some("conv-plan"), Vec::new());
        assert!(conversations.plan("conv-plan").unwrap().is_empty());
    }

    #[test]
    fn deliver_payload_validates_and_classifies() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        std::fs::write(&image, b"fake png").unwrap();
        let doc = dir.path().join("report.txt");
        std::fs::write(&doc, b"hello").unwrap();

        let (files, links, caption) = collect_deliver_payload(&json!({
            "files": [image.display().to_string(), doc.display().to_string()],
            "links": ["https://example.com/a?b=1"],
            "caption": "本次结果"
        }))
        .unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].kind, "image");
        assert_eq!(files[1].kind, "file");
        assert_eq!(files[0].size, 8);
        assert_eq!(links, vec!["https://example.com/a?b=1".to_string()]);
        assert_eq!(caption, "本次结果");

        // 敏感/非法输入都要拒绝。
        let secret = dir.path().join(".env");
        std::fs::write(&secret, b"KEY=1").unwrap();
        assert!(
            collect_deliver_payload(&json!({"files": [secret.display().to_string()]})).is_err()
        );
        assert!(collect_deliver_payload(&json!({"files": ["relative/path.txt"]})).is_err());
        assert!(
            collect_deliver_payload(&json!({"files": ["C:/Windows/System32/drivers/etc/hosts"]}))
                .is_err()
        );
        assert!(collect_deliver_payload(&json!({"links": ["ftp://x.com/a"]})).is_err());
        assert!(collect_deliver_payload(&json!({})).is_err());
    }

    #[test]
    fn window_visibility_hook_and_auto_restore() {
        let dir = tempfile::tempdir().unwrap();
        let (core, _conversations, events) = test_core(dir.path());
        let seen: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_hook = seen.clone();
        // AgentEngine::set_window_visibility_hook 是 app 用的入口；单测直接注入 EngineCore。
        *core.window_visibility_hook.lock() = Some(Arc::new(move |hidden: bool| {
            seen_for_hook.lock().push(hidden);
        }));
        assert!(core.set_window_visibility(true, "测试隐藏"));
        assert_eq!(seen.lock().as_slice(), [true]);
        assert!(events.lock().iter().any(|event| matches!(
            event,
            AgentEvent::WindowVisibilityRequested { hidden: true, .. }
        )));

        // 回合结束自动恢复。
        core.restore_window_if_hidden("任务结束");
        assert_eq!(seen.lock().as_slice(), [true, false]);
        // 没隐藏过就不重复调用 hook。
        core.restore_window_if_hidden("任务结束");
        assert_eq!(seen.lock().as_slice(), [true, false]);

        // 没有注入 hook 时返回 false（工具会提示界面不支持）。
        let dir2 = tempfile::tempdir().unwrap();
        let (core2, _c, _e) = test_core(dir2.path());
        assert!(!core2.set_window_visibility(true, "x"));
    }

    #[test]
    fn retry_backoff_and_generation_cancel_logic() {
        assert_eq!(retry_backoff(1), Duration::from_secs(1));
        assert_eq!(retry_backoff(2), Duration::from_secs(2));
        assert_eq!(retry_backoff(3), Duration::from_secs(4));
        assert_eq!(retry_backoff(4), Duration::from_secs(8));
        assert_eq!(retry_backoff(5), Duration::from_secs(8));

        // 取消发生在第 1 轮之后、第 2 轮开始之前：第 1 轮丢弃，第 2 轮正常运行。
        assert!(turn_pre_cancelled(2, 1));
        assert!(turn_pre_cancelled(2, 2));
        assert!(
            !turn_pre_cancelled(2, 3),
            "新提交的轮次不能被旧 cancel 吞掉"
        );
        assert!(!turn_pre_cancelled(0, 1));
    }

    #[test]
    fn provider_status_is_cached_and_emits_only_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let (core, _conversations, events) = test_core(dir.path());
        assert!(core.current_provider_status().is_none());

        let green = crate::provider_status::ProviderStatus {
            provider: "DeepSeek".into(),
            indicator: "green".into(),
            label: "正常".into(),
            url: "https://status.deepseek.com".into(),
            checked_at_ms: 1,
            detail: String::new(),
        };
        core.publish_provider_status(green.clone());
        assert_eq!(core.current_provider_status().unwrap().indicator, "green");
        assert_eq!(
            events
                .lock()
                .iter()
                .filter(|event| matches!(event, AgentEvent::ProviderStatusChanged { .. }))
                .count(),
            1
        );

        // 相同状态不重复 emit。
        core.publish_provider_status(green.clone());
        assert_eq!(
            events
                .lock()
                .iter()
                .filter(|event| matches!(event, AgentEvent::ProviderStatusChanged { .. }))
                .count(),
            1
        );

        // 状态变化 emit。
        let mut red = green;
        red.indicator = "red".into();
        red.label = "严重中断".into();
        core.publish_provider_status(red);
        assert!(events.lock().iter().any(|event| matches!(
            event,
            AgentEvent::ProviderStatusChanged { indicator, .. } if indicator == "red"
        )));
    }

    #[test]
    fn switch_mode_tool_updates_settings_and_emits() {
        let dir = tempfile::tempdir().unwrap();
        let (core, _conversations, events) = test_core(dir.path());
        register_builtin_tools(&core.cfg.registry, Arc::downgrade(&core));
        for name in [
            "switch_mode",
            "update_plan",
            "deliver",
            "set_window_visibility",
        ] {
            assert!(
                core.cfg.registry.names().iter().any(|item| item == name),
                "{name} 应注册进共享 registry"
            );
        }
        let tool = SwitchModeTool {
            core: Arc::downgrade(&core),
        };
        let (setup, _cancel_tx) = test_setup(&core, None);
        let ctx = ToolCtx::new(setup.host.clone(), "turn-test", "call-test");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let outcome = rt.block_on(tool.call(&ctx, json!({"mode": "coding"})));
        assert!(outcome.ok, "切换失败：{}", outcome.content);
        assert_eq!(core.settings_snapshot().agent.mode, AgentMode::Coding);
        assert!(events.lock().iter().any(|event| matches!(
            event,
            AgentEvent::ModeChanged { mode, label } if mode == "coding" && label == "编程"
        )));

        // 非法模式：返回错误且不改变当前模式。
        let outcome = rt.block_on(tool.call(&ctx, json!({"mode": "不存在的模式"})));
        assert!(!outcome.ok);
        assert_eq!(core.settings_snapshot().agent.mode, AgentMode::Coding);
        // 中文别名也能解析。
        let outcome = rt.block_on(tool.call(&ctx, json!({"mode": "计算机使用"})));
        assert!(outcome.ok, "中文别名应可解析：{}", outcome.content);
        assert_eq!(core.settings_snapshot().agent.mode, AgentMode::ComputerUse);
    }

    fn approvals_count(events: &Arc<Mutex<Vec<AgentEvent>>>) -> usize {
        events
            .lock()
            .iter()
            .filter(|event| matches!(event, AgentEvent::ApprovalRequested { .. }))
            .count()
    }

    fn risk_review_count(events: &Arc<Mutex<Vec<AgentEvent>>>) -> usize {
        events
            .lock()
            .iter()
            .filter(|event| matches!(event, AgentEvent::RiskReviewed { .. }))
            .count()
    }

    /// 在独立 runtime 里跑一次 review_gate；`decision=Some` 时异步代用户点按钮。
    fn gate_run(
        core: &Arc<EngineCore>,
        setup: &TurnSetup,
        args: &Value,
        call_id: &str,
        decision: Option<ApproveDecision>,
    ) -> ReviewGateResult {
        gate_run_tool(core, setup, "write_file", args, call_id, decision)
    }

    /// 指定工具名的 review_gate 测试驱动。
    fn gate_run_tool(
        core: &Arc<EngineCore>,
        setup: &TurnSetup,
        tool: &str,
        args: &Value,
        call_id: &str,
        decision: Option<ApproveDecision>,
    ) -> ReviewGateResult {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let settings = core.settings_snapshot();
        rt.block_on(async {
            if let Some(decision) = decision {
                let core_for_approve = core.clone();
                let id = call_id.to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    core_for_approve.resolve_approval(&id, decision);
                });
            }
            review_gate(core, setup, &settings, call_id, tool, "{}", args, true).await
        })
    }

    #[test]
    fn review_gate_handles_four_levels_and_memory() {
        let args = json!({"path": "C:/Users/x/a.txt"});
        let class = "fs:write:c:/users";

        // 1 分：自动执行，无审批。
        {
            let dir = tempfile::tempdir().unwrap();
            let (core, _conversations, events) =
                test_core_with_judge(dir.path(), Arc::new(FakeJudge { level: 1 }));
            let (setup, _cancel_tx) = test_setup(&core, None);
            let gate = gate_run(&core, &setup, &args, "call-l1", None);
            assert!(matches!(gate, ReviewGateResult::Proceed));
            assert_eq!(approvals_count(&events), 0);
            assert!(
                events
                    .lock()
                    .iter()
                    .any(|event| matches!(event, AgentEvent::RiskReviewed { level: 1, .. }))
            );
        }

        // 2 分：允许 → 记住同类（allow TTL）；授权后不再弹窗。
        {
            let dir = tempfile::tempdir().unwrap();
            let (core, _conversations, events) =
                test_core_with_judge(dir.path(), Arc::new(FakeJudge { level: 2 }));
            let (setup, _cancel_tx) = test_setup(&core, None);
            let gate = gate_run(
                &core,
                &setup,
                &args,
                "call-l2",
                Some(ApproveDecision::Approve),
            );
            assert!(matches!(gate, ReviewGateResult::Proceed));
            assert_eq!(approvals_count(&events), 1);
            assert!(
                core.review_store.allows(class),
                "2 分允许后应写入 allow 记忆"
            );
            let reviews_before = risk_review_count(&events);
            let gate = gate_run(&core, &setup, &args, "call-l2b", None);
            assert!(matches!(gate, ReviewGateResult::Proceed));
            assert_eq!(approvals_count(&events), 1, "已授权同类操作不应再弹窗");
            assert_eq!(
                risk_review_count(&events),
                reviews_before,
                "已授权不应再评分"
            );
        }

        // 2 分：拒绝 → 不执行、不记忆。
        {
            let dir = tempfile::tempdir().unwrap();
            let (core, _conversations, events) =
                test_core_with_judge(dir.path(), Arc::new(FakeJudge { level: 2 }));
            let (setup, _cancel_tx) = test_setup(&core, None);
            let gate = gate_run(
                &core,
                &setup,
                &args,
                "call-l2d",
                Some(ApproveDecision::Deny),
            );
            assert!(matches!(gate, ReviewGateResult::Blocked(_)));
            assert_eq!(approvals_count(&events), 1);
            assert!(!core.review_store.allows(class));
        }

        // 3 分：每次都要批准；批准/拒绝都不写记忆。
        {
            let dir = tempfile::tempdir().unwrap();
            let (core, _conversations, events) =
                test_core_with_judge(dir.path(), Arc::new(FakeJudge { level: 3 }));
            let (setup, _cancel_tx) = test_setup(&core, None);
            let gate = gate_run(&core, &setup, &args, "call-l3", Some(ApproveDecision::Deny));
            assert!(matches!(gate, ReviewGateResult::Blocked(_)));
            assert_eq!(approvals_count(&events), 1);
            assert!(!core.review_store.allows(class));
            assert!(!core.review_store.denies(class));
        }

        // 4 分：emphasized；拒绝并选 DenyClass → 写入 deny 记忆；之后自动拒绝。
        {
            let dir = tempfile::tempdir().unwrap();
            let (core, _conversations, events) =
                test_core_with_judge(dir.path(), Arc::new(FakeJudge { level: 4 }));
            let (setup, _cancel_tx) = test_setup(&core, None);
            let gate = gate_run(
                &core,
                &setup,
                &args,
                "call-l4",
                Some(ApproveDecision::DenyClass),
            );
            assert!(matches!(gate, ReviewGateResult::Blocked(_)));
            assert_eq!(approvals_count(&events), 1);
            assert!(
                core.review_store.denies(class),
                "DenyClass 应写入 deny 记忆"
            );
            assert!(events.lock().iter().any(|event| matches!(
                event,
                AgentEvent::ApprovalRequested { risk_level: 4, emphasized: true, class: c, .. } if c == class
            )));
            let reviews_before = risk_review_count(&events);
            let gate = gate_run(&core, &setup, &args, "call-l4b", None);
            assert!(matches!(gate, ReviewGateResult::Blocked(_)));
            assert_eq!(approvals_count(&events), 1, "拒绝期内不应再弹窗");
            assert_eq!(
                risk_review_count(&events),
                reviews_before,
                "拒绝期内不应再评分"
            );
        }
    }

    #[test]
    fn context_compression_helpers_and_sequence_shape() {
        let mut history = Vec::new();
        history.push(ChatMessage::user("用户最初的需求".repeat(300)));
        history.push(ChatMessage::assistant("助手的第一段回复".repeat(300)));
        let assistant = ChatMessage::assistant_with_calls(
            Some("调用工具".to_string()),
            None,
            vec![ToolCall {
                id: "c1".into(),
                call_type: "function".into(),
                function: crate::model::FunctionCall {
                    name: "read_file".into(),
                    arguments: "{\"path\":\"a.txt\"}".into(),
                },
            }],
        );
        history.push(assistant);
        history.push(ChatMessage::tool_result(
            "c1",
            "read_file",
            "[ok] 文件内容",
            true,
        ));
        history.push(ChatMessage::user("最近一条消息"));

        let before = history_token_estimate(&history);
        assert!(before > 0);

        // 切分点不能落在 tool 消息上（否则保留部分会缺少 assistant tool_calls）。
        let split = compression_split(&history, 2);
        assert!(split > 0 && split < history.len());
        assert_ne!(history[split].role, Role::Tool);
        let kept = &history[split..];

        // 摘要文本不包含图片 base64，包含工具结果摘要。
        let transcript = compression_transcript(&history);
        assert!(transcript.contains("用户："));
        assert!(transcript.contains("工具[read_file]"));
        assert!(transcript.contains("调用工具"));
        assert!(!transcript.contains("base64"));

        let effective = build_compressed_history("这是摘要正文", kept, false);
        assert_eq!(effective[0].role, Role::System);
        assert!(effective[0].text().contains("压缩摘要"));
        assert_eq!(effective.len(), kept.len() + 1);
        assert_eq!(effective.last().unwrap().text(), "最近一条消息");
        assert!(
            history_token_estimate(&effective) < before,
            "压缩后 token 应下降"
        );

        // 失败降级：摘要消息里标明降级。
        let fallback = build_compressed_history("更早的内容已省略", kept, true);
        assert!(fallback[0].text().contains("压缩调用失败已降级"));
    }

    #[test]
    fn tool_panic_is_isolated_and_returns_chinese_error() {
        struct PanicTool;

        #[async_trait::async_trait]
        impl Tool for PanicTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec::new(
                    "panic_tool",
                    "测试用：调用即 panic",
                    json!({"type": "object", "properties": {}}),
                )
            }

            async fn call(&self, _ctx: &ToolCtx, _args: Value) -> ToolOutcome {
                panic!("测试工具故意 panic");
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let (core, _conversations, events) = test_core(dir.path());
        core.cfg.registry.register(Arc::new(PanicTool));
        let (setup, _cancel_tx) = test_setup(&core, None);
        let call = ToolCall {
            id: "call-panic".into(),
            call_type: "function".into(),
            function: crate::model::FunctionCall {
                name: "panic_tool".into(),
                arguments: "{}".into(),
            },
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let exec = rt.block_on(execute_tool(&core, &setup, call));
        assert!(!exec.cancelled, "panic 应被转成错误结果而不是取消");

        let last = core.history.read().last().cloned().unwrap();
        assert_eq!(last.role, Role::Tool);
        assert_eq!(last.tool_ok, Some(false));
        assert!(
            last.text()
                .contains("工具执行时发生内部错误（已捕获，不影响程序）"),
            "模型可见的错误文案不对：{}",
            last.text()
        );
        assert!(
            events
                .lock()
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolFinished { ok: false, .. }))
        );
    }

    #[test]
    fn system_prompt_mentions_mcp_tools_and_hud_note() {
        let prompt = test_prompt_with(AgentMode::ComputerUse, false);
        assert!(prompt.contains("## 工具与 MCP"));
        assert!(
            prompt.contains("mcp_<server>_<tool>"),
            "应说明 MCP 工具名前缀"
        );
        assert!(prompt.contains("mcp_list_servers"));
        assert!(prompt.contains("HUD"), "应说明桌面活动提示");
        assert!(prompt.contains("不需要向用户解释"));
    }

    #[test]
    fn user_message_for_includes_uploaded_images() {
        let dir = tempfile::tempdir().unwrap();
        let (core, _conversations, _events) = test_core(dir.path());
        let (mut setup, _cancel_tx) = test_setup(&core, None);
        setup.user_text = "看图".into();
        assert_eq!(user_message_for(&setup).images.len(), 0);
        setup.user_images = vec![ImagePart::png("AAAA".into(), 10, 10, None)];
        let message = user_message_for(&setup);
        assert!(message.has_images());
        assert_eq!(message.text(), "看图");
        assert_eq!(message.images[0].width, 10);
    }

    #[test]
    fn vision_image_is_resized_and_encoded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        let rgba = image::RgbaImage::from_pixel(2000, 1000, image::Rgba::<u8>([10, 20, 30, 255]));
        image::DynamicImage::ImageRgba8(rgba).save(&path).unwrap();

        let part = encode_image_for_vision(&path, 1000).unwrap();
        assert_eq!((part.width, part.height), (1000, 500), "应按最大边等比缩放");
        assert_eq!(part.mime, "image/png");
        assert!(part.data_base64.len() > 100);
        assert!(
            part.source
                .as_deref()
                .unwrap_or_default()
                .ends_with("shot.png")
        );

        // 小于上限时不放大。
        let part = encode_image_for_vision(&path, 4000).unwrap();
        assert_eq!((part.width, part.height), (2000, 1000));

        // 坏文件返回错误，不 panic。
        let bad = dir.path().join("bad.png");
        std::fs::write(&bad, b"not an image").unwrap();
        assert!(encode_image_for_vision(&bad, 512).is_err());
    }

    #[test]
    fn image_tokens_count_toward_budget() {
        let plain = ChatMessage::user("hi");
        let with_image = ChatMessage::user_with_images(
            "hi",
            vec![ImagePart::png("AAAA".into(), 1000, 1000, None)],
        );
        assert!(
            estimate_message_tokens(&with_image) > estimate_message_tokens(&plain) + 1000,
            "图片 token 必须计入上下文预算，否则会超窗"
        );
    }

    #[test]
    fn push_message_persists_to_conversation_store() {
        let dir = tempfile::tempdir().unwrap();
        let (core, conversations, _events) = test_core(dir.path());
        let conv_id = conversations.ensure(None, "第一问", "default").unwrap();
        let (setup, _cancel_tx) = test_setup(&core, Some(conv_id.clone()));
        push_message(&core, &setup, ChatMessage::user("第一问"));
        push_message(&core, &setup, ChatMessage::assistant("第一答"));

        let stored = conversations.messages(&conv_id, 0).unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].text(), "第一问");
        assert_eq!(stored[1].text(), "第一答");
        assert_eq!(core.history.read().len(), 2);
    }

    #[test]
    fn screenshot_marker_attaches_image_message() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let shot = logs.join("shot.png");
        let rgba = image::RgbaImage::from_pixel(400, 200, image::Rgba::<u8>([12, 34, 56, 255]));
        image::DynamicImage::ImageRgba8(rgba).save(&shot).unwrap();

        let (core, conversations, events) = test_core(dir.path());
        let conv_id = conversations.ensure(None, "看屏幕", "default").unwrap();
        let (setup, _cancel_tx) = test_setup(&core, Some(conv_id.clone()));
        let text = format!("已截图: [screenshot:{}]", shot.display());
        attach_images_from_tool_output(&core, &setup, "screenshot", &text);

        {
            let history = core.history.read();
            assert_eq!(history.len(), 1, "应插入一条带图片的 user 消息");
            assert_eq!(history[0].images.len(), 1);
            assert_eq!(history[0].images[0].width, 400);
        }
        let stored = conversations.messages(&conv_id, 0).unwrap();
        assert_eq!(stored.len(), 1, "图片消息要同时进会话历史库");
        assert_eq!(stored[0].images.len(), 1);
        {
            let events = events.lock();
            assert!(events.iter().any(|event| matches!(
                event,
                AgentEvent::ImagesAttached { count, source }
                    if *count == 1 && source.as_str() == "screenshot"
            )));
        }
        // 同一路径的图片不重复附加。
        attach_images_from_tool_output(&core, &setup, "screenshot", &text);
        assert_eq!(core.history.read().len(), 1);
    }
}
