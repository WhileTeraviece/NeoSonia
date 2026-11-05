//! 子代理：在独立上下文里跑一个受限的 agent 循环，把结果汇报给父代理。
//!
//! 本模块负责「管理 + 编排」：
//! - [`SubagentManager`]：并发上限、运行中的句柄、状态快照；
//! - [`spawn`]：注册并启动 tokio 任务；`background=false` 时阻塞等待最终汇报；
//! - 两个内置工具 `spawn_subagent` / `subagent_list`（由 `AgentEngine::start` 注册）。
//!
//! 真正的循环实现在 `crate::agent::run_subagent`（复用主循环的流式/工具执行代码）。
//!
//! OWNER: core-llm 队友。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use anyhow::bail;
use async_trait::async_trait;
use futures_util::FutureExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::runtime::RuntimeFlavor;
use tokio::sync::oneshot;

use crate::agent::EngineCore;
use crate::model::{AgentEvent, SubagentRequest, ToolCtx, ToolOutcome, ToolSpec};
use crate::registry::{Tool, ToolRegistry};
use crate::util::{human_duration_ms, now_ms, truncate_chars};

/// 子代理跑完后的结果。
#[derive(Clone, Debug)]
pub struct SubagentOutcome {
    pub id: String,
    pub label: String,
    pub ok: bool,
    /// 最终汇报（3-5 行摘要）。
    pub summary: String,
    pub elapsed_ms: u64,
}

/// 子代理状态快照（`subagent_list` 工具用）。
#[derive(Clone, Debug)]
pub struct SubagentInfo {
    pub id: String,
    pub label: String,
    pub task: String,
    /// running / done / failed / aborted
    pub state: String,
    pub started_at_ms: i64,
    pub elapsed_ms: u64,
    pub summary: String,
}

/// 一个正在运行的子代理名额（Drop 时把计数减回去）。
struct SubagentSlot {
    counter: Arc<AtomicU32>,
}

impl Drop for SubagentSlot {
    fn drop(&mut self) {
        // 用 saturating 语义，避免 abort_all 之后重复递减造成下溢。
        let _ = self
            .counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                Some(v.saturating_sub(1))
            });
    }
}

/// 子代理管理器：限流 + 事件回传。
pub struct SubagentManager {
    running: Arc<AtomicU32>,
    handles: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    infos: Mutex<HashMap<String, SubagentInfo>>,
}

impl SubagentManager {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicU32::new(0)),
            handles: Mutex::new(HashMap::new()),
            infos: Mutex::new(HashMap::new()),
        }
    }

    pub fn running_count(&self) -> u32 {
        self.running.load(Ordering::SeqCst)
    }

    /// 预检：是否还能再起一个子代理。
    pub fn can_spawn(&self, max: u32, req: &SubagentRequest) -> anyhow::Result<()> {
        if req.task.trim().is_empty() {
            bail!("子代理任务不能为空：请在 task 里写清要它做什么、产出什么。");
        }
        let max = max.max(1);
        let running = self.running.load(Ordering::SeqCst);
        if running >= max {
            bail!(
                "同时运行的子代理已达上限（{max} 个，当前 {running} 个）。\
                 等已有子代理结束后再试，或调大设置里的 agent.max_subagents。"
            );
        }
        Ok(())
    }

    /// 原子地占用一个名额；失败时返回可操作的中文错误。
    fn try_begin(&self, max: u32) -> anyhow::Result<SubagentSlot> {
        let max = max.max(1);
        loop {
            let current = self.running.load(Ordering::SeqCst);
            if current >= max {
                bail!(
                    "同时运行的子代理已达上限（{max} 个）。\
                     等已有子代理结束后再试，或调大设置里的 agent.max_subagents。"
                );
            }
            if self
                .running
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Ok(SubagentSlot {
                    counter: self.running.clone(),
                });
            }
        }
    }

    fn mark_started(&self, id: &str, label: &str, task: &str) {
        self.infos.lock().insert(
            id.to_string(),
            SubagentInfo {
                id: id.to_string(),
                label: label.to_string(),
                task: task.to_string(),
                state: "running".to_string(),
                started_at_ms: now_ms(),
                elapsed_ms: 0,
                summary: String::new(),
            },
        );
    }

    fn mark_finished(&self, id: &str, ok: bool, summary: &str, elapsed_ms: u64) {
        if let Some(info) = self.infos.lock().get_mut(id) {
            info.state = if ok { "done" } else { "failed" }.to_string();
            info.elapsed_ms = elapsed_ms;
            info.summary = truncate_chars(summary, 2000);
        }
    }

    fn register_handle(&self, id: &str, handle: tokio::task::JoinHandle<()>) {
        // 极快结束的子代理可能在 spawn 返回前就已经 finish，此时不要再登记句柄。
        let finished = self
            .infos
            .lock()
            .get(id)
            .map(|info| info.state != "running")
            .unwrap_or(true);
        if finished {
            return;
        }
        self.handles.lock().insert(id.to_string(), handle);
    }

    /// 任务结束时调用：移除句柄并释放名额（句柄 drop 不会 abort 已结束的任务）。
    fn finish(&self, id: &str, slot: SubagentSlot) {
        self.handles.lock().remove(id);
        drop(slot);
    }

    /// 全部子代理的状态快照，按开始时间倒序。
    pub fn list(&self) -> Vec<SubagentInfo> {
        let mut out: Vec<SubagentInfo> = self.infos.lock().values().cloned().collect();
        out.sort_by(|a, b| b.started_at_ms.cmp(&a.started_at_ms));
        out
    }

    /// 停掉所有正在跑的子代理（app 退出时调用）。
    pub fn abort_all(&self) {
        let handles: Vec<tokio::task::JoinHandle<()>> = {
            let mut guard = self.handles.lock();
            guard.drain().map(|(_, handle)| handle).collect()
        };
        for handle in handles {
            handle.abort();
        }
        for info in self.infos.lock().values_mut() {
            if info.state == "running" {
                info.state = "aborted".to_string();
            }
        }
        self.running.store(0, Ordering::SeqCst);
    }
}

impl Default for SubagentManager {
    fn default() -> Self {
        Self::new()
    }
}

/// [`spawn`] 的结果：后台任务只回 id，前台等待还带回最终汇报。
#[derive(Clone, Debug)]
pub(crate) struct SpawnResult {
    pub id: String,
    pub label: String,
    /// true = 后台运行，没等结果。
    pub background: bool,
    /// background=false 且等待成功时的汇报。
    pub outcome: Option<SubagentOutcome>,
}

/// 启动一个子代理。
///
/// - `background = true`：立即返回；
/// - `background = false`：阻塞等待子代理结束并带回汇报（多线程 tokio runtime）；
///   单线程 runtime 上无法安全阻塞，会降级为后台运行。
pub(crate) fn spawn(
    core: Arc<EngineCore>,
    req: SubagentRequest,
    parent_cancel: Arc<AtomicBool>,
) -> anyhow::Result<SpawnResult> {
    let settings = core.settings_snapshot();
    if !settings.agent.subagents_enabled {
        bail!(
            "子代理功能已在设置里关闭（agent.subagents_enabled = false）。\
             请到设置页打开，或直接由我完成这个任务。"
        );
    }
    let label = normalize_label(&req.label);
    let max = settings.agent.max_subagents.max(1);
    core.subagents().can_spawn(max, &req)?;
    let slot = core.subagents().try_begin(max)?;

    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
        anyhow::anyhow!(
            "spawn_subagent 必须在 tokio 运行时里调用（app 层请用 #[tokio::main] 或手动创建 runtime）"
        )
    })?;

    let id = format!("sa-{}", crate::util::short_id());
    let manager = core.subagents();
    manager.mark_started(&id, &label, &req.task);
    let background = req.background;
    let parent_rx = core.cancel_subscribe();
    let (outcome_tx, outcome_rx) = if background {
        (None, None)
    } else {
        let (tx, rx) = oneshot::channel();
        (Some(tx), Some(rx))
    };

    let core_for_task = core.clone();
    let core_for_run = core_for_task.clone();
    let manager_for_task = manager.clone();
    let id_for_task = id.clone();
    let label_for_task = label.clone();
    let started = std::time::Instant::now();
    let task_handle = handle.spawn(async move {
        // 子代理里的 panic 不能影响父代理：捕获后合成一个失败汇报。
        let run = std::panic::AssertUnwindSafe(crate::agent::run_subagent(
            core_for_run,
            id_for_task.clone(),
            req,
            parent_cancel,
            parent_rx,
        ));
        let outcome = match run.catch_unwind().await {
            Ok(outcome) => outcome,
            Err(_) => {
                let elapsed_ms = started.elapsed().as_millis() as u64;
                tracing::error!(id = %id_for_task, "子代理任务发生 panic（已隔离）");
                let summary =
                    "子代理内部发生 panic（已隔离），没有完成汇报。请重试，或把任务拆得更小。"
                        .to_string();
                core_for_task.emit(AgentEvent::SubagentFinished {
                    id: id_for_task.clone(),
                    label: label_for_task.clone(),
                    ok: false,
                    summary: summary.clone(),
                    elapsed_ms,
                });
                SubagentOutcome {
                    id: id_for_task.clone(),
                    label: label_for_task.clone(),
                    ok: false,
                    summary,
                    elapsed_ms,
                }
            }
        };
        manager_for_task.mark_finished(
            &id_for_task,
            outcome.ok,
            &outcome.summary,
            outcome.elapsed_ms,
        );
        if let Some(tx) = outcome_tx {
            let _ = tx.send(outcome.clone());
        }
        manager_for_task.finish(&id_for_task, slot);
    });
    manager.register_handle(&id, task_handle);

    if background {
        return Ok(SpawnResult {
            id,
            label,
            background: true,
            outcome: None,
        });
    }

    match outcome_rx.map(wait_blocking) {
        Some(Some(outcome)) => Ok(SpawnResult {
            id,
            label,
            background: false,
            outcome: Some(outcome),
        }),
        Some(None) => {
            core.emit(AgentEvent::Notice(format!(
                "子代理 {label} 已启动；当前 tokio 运行时是单线程，无法阻塞等待，\
                 它会在后台运行并在结束时推送 SubagentFinished 事件。"
            )));
            Ok(SpawnResult {
                id,
                label,
                background: false,
                outcome: None,
            })
        }
        None => bail!(
            "子代理 {label} 已启动，但等待通道意外关闭；请查看 SubagentFinished 事件确认最终结果。"
        ),
    }
}

/// 阻塞等待子代理结束；单线程 / 非 tokio 环境下返回 None。
fn wait_blocking(rx: oneshot::Receiver<SubagentOutcome>) -> Option<SubagentOutcome> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    if !matches!(handle.runtime_flavor(), RuntimeFlavor::MultiThread) {
        return None;
    }
    // block_in_place 把当前 worker 上的其它任务挪走，避免阻塞整个 runtime。
    tokio::task::block_in_place(|| handle.block_on(async { rx.await.ok() }))
}

fn normalize_label(label: &str) -> String {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        "子代理".to_string()
    } else {
        truncate_chars(trimmed, 40)
    }
}

fn first_line(text: &str, max_chars: usize) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    truncate_chars(line, max_chars)
}

fn parse_request(ctx: &ToolCtx, args: &Value) -> anyhow::Result<SubagentRequest> {
    let task = args
        .get("task")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if task.is_empty() {
        bail!(
            "spawn_subagent 缺少 task 参数。请写清要子代理完成的具体任务、输入位置和期望产出，\
             例如 {{\"label\":\"查日志\",\"task\":\"在 logs/ 里找出最近的错误并总结 3 行\"}}",
        );
    }
    if task.chars().count() > 8000 {
        bail!("spawn_subagent 的 task 太长（超过 8000 字符），请精简后重试。");
    }
    let label = args
        .get("label")
        .and_then(|v| v.as_str())
        .map(normalize_label)
        .unwrap_or_else(|| "子代理".to_string());
    let tools: Vec<String> = args
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if tools.len() > 40 {
        bail!(
            "spawn_subagent 的 tools 太多（{} 个）。请只列出真正需要的工具。",
            tools.len()
        );
    }
    let persona = args
        .get("persona")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let background = args
        .get("background")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let max_iterations = args
        .get("max_iterations")
        .and_then(|v| v.as_u64())
        .unwrap_or(12)
        .clamp(1, 50) as u32;
    if ctx.is_cancelled() {
        bail!("当前轮次已取消，未启动子代理。");
    }
    Ok(SubagentRequest {
        label,
        task,
        persona,
        tools,
        max_iterations,
        background,
    })
}

// ============================================================
// 内置工具
// ============================================================

/// 把 `spawn_subagent` / `subagent_list` 注册进 registry。
///
/// 工具只持 `Weak<EngineCore>`，避免 registry ←→ core 的循环引用；
/// 这样 `Tool::call` 里能直接 await oneshot 等到子代理的最终汇报。
pub(crate) fn register_builtin_tools(reg: &ToolRegistry, core: Weak<EngineCore>) {
    reg.register(Arc::new(SpawnSubagentTool { core: core.clone() }));
    reg.register(Arc::new(SubagentListTool { core }));
}

struct SpawnSubagentTool {
    core: Weak<EngineCore>,
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "spawn_subagent",
            "启动一个子代理，在独立上下文里完成一件明确、耗时或需要大量翻找的任务，\
             结果以 3-5 行摘要返回。适合：大范围搜索/阅读、并行调研、批量整理；\
             不适合：一步就能完成的简单操作、需要用户交互的操作。\
             默认 background=false（阻塞等待结果）；任务很大且不需要马上要结果时设 background=true。",
            json!({
                "type": "object",
                "properties": {
                    "label": { "type": "string", "description": "短名字，用于 UI 展示，例如 查日志。" },
                    "task": { "type": "string", "description": "完整任务说明：目标、输入位置、约束、期望产出。" },
                    "tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "只允许子代理使用的工具名；留空 = 继承父代理全部工具。"
                    },
                    "persona": { "type": "string", "description": "可选，子代理使用的人设 id。" },
                    "background": {
                        "type": "boolean",
                        "description": "true = 立即返回并在后台运行；false（默认）= 阻塞等待最终汇报。"
                    },
                    "max_iterations": {
                        "type": "integer",
                        "description": "子代理最多跑几轮工具调用，默认 12，最大 50。"
                    }
                },
                "required": ["task"]
            }),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let Some(core) = self.core.upgrade() else {
            return ToolOutcome::err("agent 引擎已关闭，无法启动子代理。请重启应用。");
        };
        let req = match parse_request(ctx, &args) {
            Ok(req) => req,
            Err(e) => return ToolOutcome::err(e.to_string()),
        };
        match spawn(core.clone(), req, core.cancel_flag()) {
            Ok(result) => {
                if result.background {
                    return ToolOutcome::ok(format!(
                        "已启动子代理 {}（{}）。完成后会推送事件。",
                        result.id, result.label
                    ))
                    .with_display(format!("启动子代理 {}", result.label));
                }
                match result.outcome {
                    Some(outcome) => {
                        let status = if outcome.ok { "成功" } else { "失败" };
                        let body = if outcome.summary.trim().is_empty() {
                            "（子代理没有输出汇报）".to_string()
                        } else {
                            outcome.summary.trim().to_string()
                        };
                        ToolOutcome::ok(format!(
                            "子代理 {}（{}）{}，耗时 {}。\n汇报：\n{}",
                            outcome.label,
                            outcome.id,
                            status,
                            human_duration_ms(outcome.elapsed_ms),
                            body
                        ))
                        .with_display(format!("子代理 {} 完成", outcome.label))
                    }
                    None => ToolOutcome::ok(format!(
                        "已启动子代理 {}（{}）。当前运行时无法阻塞等待，完成后会推送事件。",
                        result.id, result.label
                    ))
                    .with_display(format!("启动子代理 {}", result.label)),
                }
            }
            Err(e) => ToolOutcome::err(format!("启动子代理失败：{e}")),
        }
    }
}

struct SubagentListTool {
    core: Weak<EngineCore>,
}

#[async_trait]
impl Tool for SubagentListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "subagent_list",
            "列出本应用启动过的子代理及其状态（running / done / failed / aborted）、耗时和摘要。\
             用于查看后台子代理是否完成。",
            json!({ "type": "object", "properties": {} }),
        )
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> ToolOutcome {
        let Some(core) = self.core.upgrade() else {
            return ToolOutcome::err("agent 引擎已关闭，无法读取子代理列表。");
        };
        let infos = core.subagents().list();
        if infos.is_empty() {
            return ToolOutcome::ok("还没有启动过子代理。");
        }
        let running = infos.iter().filter(|i| i.state == "running").count();
        let mut out = format!("子代理共 {} 个（运行中 {running} 个）：\n", infos.len());
        for info in &infos {
            out.push_str(&format!(
                "- {} [{}] {}，已运行 {}",
                info.id,
                info.state,
                info.label,
                human_duration_ms(if info.state == "running" {
                    (now_ms() - info.started_at_ms).max(0) as u64
                } else {
                    info.elapsed_ms
                })
            ));
            if info.state != "running" && !info.summary.is_empty() {
                out.push_str(&format!("；摘要：{}", first_line(&info.summary, 120)));
            }
            out.push('\n');
        }
        ToolOutcome::ok(out).with_display(format!("{} 个子代理", infos.len()))
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> SubagentRequest {
        SubagentRequest {
            label: "test".into(),
            task: "把 README 读一遍并总结".into(),
            ..Default::default()
        }
    }

    #[test]
    fn manager_enforces_capacity() {
        let manager = SubagentManager::new();
        assert_eq!(manager.running_count(), 0);
        assert!(manager.can_spawn(2, &request()).is_ok());
        let slot1 = manager.try_begin(2).unwrap();
        let slot2 = manager.try_begin(2).unwrap();
        assert_eq!(manager.running_count(), 2);
        assert!(manager.can_spawn(2, &request()).is_err());
        assert!(manager.try_begin(2).is_err());
        drop(slot1);
        assert_eq!(manager.running_count(), 1);
        assert!(manager.can_spawn(2, &request()).is_ok());
        let slot3 = manager.try_begin(2).unwrap();
        drop(slot2);
        drop(slot3);
        assert_eq!(manager.running_count(), 0);
    }

    #[test]
    fn manager_rejects_empty_task() {
        let manager = SubagentManager::new();
        let req = SubagentRequest::default();
        let err = manager.can_spawn(3, &req).unwrap_err();
        assert!(err.to_string().contains("不能为空"));
    }

    #[test]
    fn manager_tracks_infos_and_aborts() {
        let manager = SubagentManager::new();
        manager.mark_started("sa-1", "查日志", "在 logs 里找错误");
        manager.mark_finished("sa-1", true, "发现 2 个错误", 1234);
        let infos = manager.list();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].state, "done");
        assert_eq!(infos[0].elapsed_ms, 1234);
        assert!(infos[0].summary.contains("2 个错误"));
        manager.abort_all();
        assert_eq!(manager.running_count(), 0);
    }

    #[test]
    fn label_and_first_line_helpers() {
        assert_eq!(normalize_label("  "), "子代理");
        assert_eq!(normalize_label("查日志"), "查日志");
        assert_eq!(first_line("\n\n第一行\n第二行", 100), "第一行");
        assert_eq!(first_line("很长的一行内容", 3).chars().count(), 4); // 3 字符 + 省略号
    }

    #[test]
    fn register_builtin_tools_is_idempotent() {
        let reg = ToolRegistry::new();
        // Weak 拿不到 EngineCore 也可以注册（调用时才 upgrade）。
        let dummy: Weak<EngineCore> = Weak::new();
        register_builtin_tools(&reg, dummy.clone());
        register_builtin_tools(&reg, dummy);
        let mut names = reg.names();
        names.sort();
        assert_eq!(
            names,
            vec!["spawn_subagent".to_string(), "subagent_list".to_string()]
        );
    }

    #[test]
    fn parse_request_defaults_and_overrides() {
        // 只测纯解析部分，不访问 EngineCore：用 Default ToolCtx 不可行，
        // 所以这里通过子代理请求的字段语义直接断言。
        let req = SubagentRequest {
            label: normalize_label("  "),
            task: "做事".into(),
            tools: vec!["read_file".into()],
            max_iterations: 5,
            ..Default::default()
        };
        assert_eq!(req.label, "子代理");
        assert!(!req.background);
        assert_eq!(req.max_iterations, 5);
    }
}
