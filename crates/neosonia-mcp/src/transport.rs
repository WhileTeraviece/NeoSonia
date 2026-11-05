//! 传输层抽象：stdio 与 http 都实现 [`Session`]。
//!
//! 超时统一在 [`request_with_timeout`] 里做，这样两条传输路径的行为完全一致，
//! 也便于用假 Session 做单测（不需要真的起子进程）。

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

/// 一条已建立的 MCP 连接（只负责收发 JSON-RPC，不管 initialize 等语义）。
#[async_trait]
pub(crate) trait Session: Send + Sync {
    /// 发请求并等回包；超时由调用方用 [`request_with_timeout`] 控制。
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value>;

    /// 发通知，不等回包。
    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()>;

    /// 子进程 pid（http 传输为 `None`）。
    fn pid(&self) -> Option<u32> {
        None
    }

    /// 子进程 stderr 的末尾内容（http 传输为空），握手失败时拼进错误里便于排查。
    fn stderr_tail(&self) -> String {
        String::new()
    }

    /// 关闭连接（杀子进程 / 释放连接）。
    async fn close(&self);
}

/// 带超时的请求：超时返回中文错误。
pub(crate) async fn request_with_timeout(
    session: &dyn Session,
    method: &str,
    params: Value,
    timeout: Duration,
) -> anyhow::Result<Value> {
    match tokio::time::timeout(timeout, session.request(method, params)).await {
        Ok(result) => result,
        Err(_) => anyhow::bail!("请求超时（{} 秒）。", timeout.as_secs()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    /// 永远不回包的假服务器：用来验证超时链路。
    struct NeverRespond;

    #[async_trait]
    impl Session for NeverRespond {
        async fn request(&self, _method: &str, _params: Value) -> anyhow::Result<Value> {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(Value::Null)
        }

        async fn notify(&self, _method: &str, _params: Value) -> anyhow::Result<()> {
            Ok(())
        }

        fn pid(&self) -> Option<u32> {
            None
        }

        async fn close(&self) {}
    }

    /// 立刻回包的假服务器。
    struct Echo;

    #[async_trait]
    impl Session for Echo {
        async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
            Ok(json!({"method": method, "params": params}))
        }

        async fn notify(&self, _method: &str, _params: Value) -> anyhow::Result<()> {
            Ok(())
        }

        fn pid(&self) -> Option<u32> {
            Some(4242)
        }

        async fn close(&self) {}
    }

    #[tokio::test]
    async fn timeout_reports_chinese_error() {
        let started = std::time::Instant::now();
        let err = request_with_timeout(
            &NeverRespond,
            "tools/call",
            json!({}),
            Duration::from_millis(40),
        )
        .await
        .expect_err("应该超时");
        assert!(err.to_string().contains("超时"), "{err}");
        assert!(err.to_string().contains("秒"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn success_passthrough_and_pid() {
        let value = request_with_timeout(
            &Echo,
            "tools/list",
            json!({"cursor": null}),
            Duration::from_secs(1),
        )
        .await
        .expect("应该成功");
        assert_eq!(value["method"], "tools/list");
        assert_eq!(value["params"]["cursor"], Value::Null);
        assert_eq!(Echo.pid(), Some(4242));
    }
}
