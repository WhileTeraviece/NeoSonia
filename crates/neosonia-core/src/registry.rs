//! 工具注册表与 `Tool` trait。
//!
//! 工具实现在 `neosonia-tools` / `neosonia-mail` 里，通过 `ToolRegistry::register`
//! 挂进来；`neosonia-core` 不认识任何具体工具。

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::Value;

use crate::model::{ToolCtx, ToolOutcome, ToolSpec};

/// 一个可被模型调用的工具。
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome;

    /// 危险操作（写盘/执行命令/模拟输入）是否需要用户在 UI 上确认。
    /// `approval_mode = auto` 时由 agent 循环决定是否真的询问。
    fn requires_approval(&self, _args: &Value) -> bool {
        false
    }

    /// 工具结果回给模型前的裁剪钩子（例如截图只回路径不回二进制）。
    fn truncate_output(&self, output: &str, max_chars: usize) -> String {
        crate::util::middle_truncate(output, max_chars)
    }
}

/// 线程安全的工具集合。克隆开销小（内部 `Arc`）。
#[derive(Clone, Default)]
pub struct ToolRegistry {
    inner: Arc<RwLock<BTreeMap<String, Arc<dyn Tool>>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, tool: Arc<dyn Tool>) {
        let name = tool.spec().name;
        self.inner.write().insert(name, tool);
    }

    /// 链式注册，方便构造。
    pub fn with(self, tool: Arc<dyn Tool>) -> Self {
        self.register(tool);
        self
    }

    pub fn names(&self) -> Vec<String> {
        self.inner.read().keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.inner.read().get(name).cloned()
    }

    /// 全部工具规格（按名字排序，保证 prompt 稳定，利于 KV 缓存）。
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.inner.read().values().map(|t| t.spec()).collect()
    }

    /// 只取这些名字的规格；`allow` 为空则返回全部。
    pub fn specs_filtered(&self, allow: &[String]) -> Vec<ToolSpec> {
        if allow.is_empty() {
            return self.specs();
        }
        let guard = self.inner.read();
        allow
            .iter()
            .filter_map(|n| guard.get(n))
            .map(|t| t.spec())
            .collect()
    }

    /// 执行工具。未知工具返回错误结果而不是 panic。
    pub async fn call(&self, ctx: &ToolCtx, name: &str, args: &Value) -> ToolOutcome {
        let Some(tool) = self.get(name) else {
            return ToolOutcome::err(format!("未知工具: {name}"));
        };
        let started = std::time::Instant::now();
        let outcome = tool.call(ctx, args.clone()).await;
        let elapsed = started.elapsed().as_millis() as u64;
        if elapsed > 5_000 {
            tracing::debug!(
                tool = name,
                elapsed_ms = elapsed,
                ok = outcome.ok,
                "slow tool"
            );
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(
                "echo",
                "repeat input",
                serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}}),
            )
        }

        async fn call(&self, _ctx: &ToolCtx, args: Value) -> ToolOutcome {
            ToolOutcome::ok(args.get("text").and_then(|v| v.as_str()).unwrap_or(""))
        }
    }

    #[test]
    fn register_and_list() {
        let r = ToolRegistry::new().with(Arc::new(Echo));
        assert_eq!(r.names(), vec!["echo".to_string()]);
        assert_eq!(r.specs().len(), 1);
        assert!(r.get("nope").is_none());
    }
}
