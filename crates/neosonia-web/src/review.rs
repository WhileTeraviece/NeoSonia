//! 远程审查（审批请求）展示。
//!
//! 远程端**只能只读展示**正在等待桌面端确认的危险操作，**不能远程批准**——
//! 这是刻意的安全边界：审批必须由坐在电脑前的人完成。
//!
//! 数据流：
//! ```text
//! core AgentEvent::ApprovalRequested / 风险评分结果
//!        │  app bridge 事件循环
//!        ▼
//! neosonia_web::review_hub().push(ReviewItem{..})      ← app 调用
//!        │
//!        ├─ GET /api/review/pending  →  SPA 顶部警示条 + 列表
//!        └─ WebSocket 广播 {"type":"risk_review","data":{call_id,tool,risk_level,risk_reason,emphasized}}
//! ```
//! 审批结束后 app 调 [`ReviewHub::resolve`]（或 [`ReviewHub::clear`]），列表随即清空。
//!
//! hub 是进程级单例（[`review_hub`]），**不需要改 `WebConfig`**，app 只需少量接线。

use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// 一条等待桌面端审查/确认的操作。
///
/// 所有字段带默认值：app 侧只填知道的字段，其余用 `..ReviewItem::default()`。
/// 字段名即 `/api/review/pending` 的 JSON 形状，旧客户端可忽略未知字段。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewItem {
    /// 工具调用的 call_id（唯一；同 id 的 push 会覆盖旧条目）。
    pub call_id: String,
    /// 工具名，例如 `run_command` / `write_file` / `mouse_click`。
    pub tool: String,
    /// 参数摘要（一段人类可读文本，不要放完整敏感参数）。
    pub args_summary: String,
    /// 风险等级：`low` / `medium` / `high` / `critical`（未知可留空）。
    pub risk_level: String,
    /// 风险原因（中文一句话）。
    pub risk_reason: String,
    /// 操作分类：`shell` / `fs` / `network` / `input` / `system` / `other` 等。
    pub class: String,
    /// 是否需要在 UI 上强调（红色警示）。
    pub emphasized: bool,
}

impl ReviewItem {
    /// 便捷构造：只填必需字段，其余默认。
    pub fn new(call_id: impl Into<String>, tool: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            tool: tool.into(),
            ..Self::default()
        }
    }

    /// 规范化后的风险等级（未知一律按 `medium` 处理）。
    pub fn risk_rank(&self) -> u8 {
        match self.risk_level.trim().to_ascii_lowercase().as_str() {
            "low" => 1,
            "high" => 3,
            "critical" => 4,
            "medium" => 2,
            _ => 2,
        }
    }

    /// 是否高风险（红色警示）。
    pub fn is_high_risk(&self) -> bool {
        self.emphasized || self.risk_rank() >= 3
    }
}

#[derive(Debug, Default)]
struct HubState {
    pending: Vec<ReviewItem>,
}

/// 远程审查队列的变化通知（WebSocket 用）。
///
/// `Resolved`：桌面端已处理某个审批（同意/拒绝/超时），远程要清横幅并刷新消息；
/// `Cleared`：整个队列被清空（服务停止/用户清空）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewResolution {
    Resolved(String),
    Cleared,
}

/// 待审查队列：内存态 + 广播给 WebSocket。
///
/// 进程级单例由 [`review_hub`] 提供；测试/多实例场景可直接 [`ReviewHub::new`]。
pub struct ReviewHub {
    state: Mutex<HubState>,
    tx: broadcast::Sender<ReviewItem>,
    resolved_tx: broadcast::Sender<ReviewResolution>,
}

impl Default for ReviewHub {
    fn default() -> Self {
        Self::new()
    }
}

impl ReviewHub {
    /// 新建一个 hub（广播容量 64，足够覆盖慢客户端）。
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(64);
        let (resolved_tx, _rx2) = broadcast::channel(64);
        Self {
            state: Mutex::new(HubState::default()),
            tx,
            resolved_tx,
        }
    }

    /// 新增/更新一条待审查请求，并广播给所有 WebSocket 客户端。
    ///
    /// 同一个 `call_id` 会覆盖原条目（保持原顺序），避免事件重发导致重复。
    pub fn push(&self, item: ReviewItem) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = state
                .pending
                .iter_mut()
                .find(|it| it.call_id == item.call_id)
            {
                *existing = item.clone();
            } else {
                if state.pending.len() >= 64 {
                    state.pending.remove(0);
                }
                state.pending.push(item.clone());
            }
        }
        // 没有订阅者时 send 会失败，属正常情况，忽略。
        let _ = self.tx.send(item);
    }

    /// 审批结束（同意/拒绝/超时）：从待审列表移除，并通知远程清横幅+刷新消息。
    pub fn resolve(&self, call_id: &str) -> bool {
        let id = call_id.trim();
        if id.is_empty() {
            return false;
        }
        let removed = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let before = state.pending.len();
            state.pending.retain(|it| it.call_id != id);
            state.pending.len() != before
        };
        if removed {
            // 没有订阅者时 send 会失败，属正常情况，忽略。
            let _ = self
                .resolved_tx
                .send(ReviewResolution::Resolved(id.to_string()));
        }
        removed
    }

    /// 清空（服务停止 / 用户清空时调用），并通知远程清空横幅。
    pub fn clear(&self) {
        let had = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let had = !state.pending.is_empty();
            state.pending.clear();
            had
        };
        if had {
            let _ = self.resolved_tx.send(ReviewResolution::Cleared);
        }
    }

    /// 当前待审列表（按到达顺序）。
    pub fn pending(&self) -> Vec<ReviewItem> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending.clone()
    }

    /// 待审数量。
    pub fn len(&self) -> usize {
        self.pending().len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 订阅新审查事件（WebSocket 用）。
    pub fn subscribe(&self) -> broadcast::Receiver<ReviewItem> {
        self.tx.subscribe()
    }

    /// 订阅审查处理结果（WebSocket 用）：桌面端处理完审批后远程清横幅/刷新消息。
    pub fn subscribe_resolved(&self) -> broadcast::Receiver<ReviewResolution> {
        self.resolved_tx.subscribe()
    }
}

/// 进程级单例。app 在 bridge 里用它接线即可，WebServer 内部也用它。
pub fn review_hub() -> Arc<ReviewHub> {
    static HUB: OnceLock<Arc<ReviewHub>> = OnceLock::new();
    HUB.get_or_init(|| Arc::new(ReviewHub::new())).clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(id: &str, risk: &str) -> ReviewItem {
        ReviewItem {
            call_id: id.to_string(),
            tool: "run_command".to_string(),
            args_summary: "pwsh -Command ...".to_string(),
            risk_level: risk.to_string(),
            risk_reason: "会修改系统文件".to_string(),
            class: "shell".to_string(),
            emphasized: risk == "critical",
        }
    }

    #[test]
    fn dto_serializes_with_expected_fields() {
        let json = serde_json::to_value(item("c1", "high")).expect("序列化");
        assert_eq!(json["call_id"], "c1");
        assert_eq!(json["tool"], "run_command");
        assert_eq!(json["args_summary"], "pwsh -Command ...");
        assert_eq!(json["risk_level"], "high");
        assert_eq!(json["risk_reason"], "会修改系统文件");
        assert_eq!(json["class"], "shell");
        assert_eq!(json["emphasized"], false);
    }

    #[test]
    fn dto_deserializes_with_defaults() {
        let parsed: ReviewItem = serde_json::from_value(json!({"call_id": "x"})).expect("解析");
        assert_eq!(parsed.call_id, "x");
        assert!(parsed.tool.is_empty());
        assert!(!parsed.emphasized);
        assert_eq!(parsed.risk_rank(), 2, "未知风险默认按 medium");
    }

    #[test]
    fn push_dedupes_by_call_id_and_keeps_order() {
        let hub = ReviewHub::new();
        hub.push(item("a", "low"));
        hub.push(item("b", "high"));
        hub.push(item("a", "critical"));
        let pending = hub.pending();
        assert_eq!(pending.len(), 2, "同 call_id 应覆盖而不是重复");
        assert_eq!(pending[0].call_id, "a");
        assert_eq!(pending[0].risk_level, "critical");
        assert_eq!(pending[1].call_id, "b");
    }

    #[test]
    fn resolve_and_clear() {
        let hub = ReviewHub::new();
        hub.push(item("a", "low"));
        hub.push(item("b", "high"));
        assert!(hub.resolve("a"));
        assert!(!hub.resolve("a"), "重复 resolve 返回 false");
        assert!(!hub.resolve(""), "空 id 返回 false");
        assert_eq!(hub.len(), 1);
        hub.clear();
        assert!(hub.is_empty());
    }

    #[test]
    fn broadcasts_risk_review_to_subscribers() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("建 runtime");
        rt.block_on(async {
            let hub = ReviewHub::new();
            let mut rx = hub.subscribe();
            hub.push(item("c9", "high"));
            let got = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
                .await
                .expect("应收到广播")
                .expect("广播内容");
            assert_eq!(got.call_id, "c9");
            assert_eq!(got.risk_level, "high");
        });
    }

    #[test]
    fn resolve_and_clear_broadcast_to_remote() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("建 runtime");
        rt.block_on(async {
            let hub = ReviewHub::new();
            let mut resolved_rx = hub.subscribe_resolved();
            hub.push(item("a", "high"));
            assert!(hub.resolve("a"));
            let got = tokio::time::timeout(std::time::Duration::from_millis(500), resolved_rx.recv())
                .await
                .expect("应收到 resolve 广播")
                .expect("广播内容");
            assert_eq!(got, ReviewResolution::Resolved("a".to_string()));
            // 重复 resolve 不再广播。
            assert!(!hub.resolve("a"));

            hub.push(item("b", "low"));
            hub.clear();
            let got = tokio::time::timeout(std::time::Duration::from_millis(500), resolved_rx.recv())
                .await
                .expect("应收到 clear 广播")
                .expect("广播内容");
            assert_eq!(got, ReviewResolution::Cleared);
        });
    }

    #[test]
    fn risk_rank_and_highlight() {
        assert_eq!(ReviewItem::new("a", "t").risk_rank(), 2);
        assert!(item("a", "high").is_high_risk());
        assert!(item("a", "critical").is_high_risk());
        assert!(!item("a", "low").is_high_risk());
        let mut medium = item("a", "medium");
        medium.emphasized = true;
        assert!(medium.is_high_risk(), "emphasized 应触发红色警示");
    }

    #[test]
    fn global_hub_is_stable_singleton() {
        let a = review_hub();
        let b = review_hub();
        assert!(Arc::ptr_eq(&a, &b), "进程级 hub 必须是同一个实例");
        a.clear();
        assert!(a.is_empty());
    }
}
