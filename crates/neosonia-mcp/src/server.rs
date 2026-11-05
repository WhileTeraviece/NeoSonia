//! 单台 MCP 服务器的运行态：连接、握手、列工具、调工具、关闭。
//!
//! 每台服务器用一把 tokio Mutex 串行化所有 JSON-RPC 请求（这把锁同时是"当前连接"
//! 的存放处）：同一时刻只有一个请求在飞，重连也不会和正在进行的调用打架。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use neosonia_core::{McpServerConfig, McpTransport, Paths};
use parking_lot::Mutex as BlockingMutex;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::http;
use crate::naming;
use crate::stdio;
use crate::transport::{Session, request_with_timeout};

/// 对外暴露的服务器运行态（字段与 app 的 `McpServerView` 保持一致）。
#[derive(Clone, Debug, PartialEq)]
pub struct McpServerState {
    pub name: String,
    pub ok: bool,
    pub tools: usize,
    pub error: Option<String>,
    pub pid: Option<u32>,
}

/// 对外暴露的 MCP 工具信息。
#[derive(Clone, Debug)]
pub struct McpToolInfo {
    /// 所属服务器名。
    pub server: String,
    /// MCP 服务器里的原始工具名（`tools/call` 用）。
    pub name: String,
    /// 注册进 NeoSonia registry 的完整工具名：`<prefix><name>`。
    pub full_name: String,
    pub description: String,
    /// `inputSchema`（JSON Schema）；服务器没给时补一个空对象 schema。
    pub schema: Value,
}

impl McpToolInfo {
    fn from_json(server: &str, prefix: &str, raw: &Value) -> Option<Self> {
        let name = raw
            .get("name")
            .and_then(|value| value.as_str())?
            .trim()
            .to_string();
        if name.is_empty() {
            return None;
        }
        let description = raw
            .get("description")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let schema = match raw.get("inputSchema") {
            Some(schema @ Value::Object(_)) => schema.clone(),
            _ => json!({ "type": "object", "properties": {} }),
        };
        Some(Self {
            server: server.to_string(),
            full_name: naming::full_tool_name(prefix, &name),
            name,
            description,
            schema,
        })
    }
}

/// 一台 MCP 服务器的内部状态。
pub(crate) struct ServerState {
    config: McpServerConfig,
    prefix: String,
    /// 当前连接；同时充当"串行请求"的锁。
    session: Mutex<Option<Arc<dyn Session>>>,
    /// 最近一次 `tools/list` 的结果。
    tools: BlockingMutex<Vec<McpToolInfo>>,
    error: BlockingMutex<Option<String>>,
    pid: BlockingMutex<Option<u32>>,
    /// 因重名 / 超出全局上限而跳过的工具数。
    skipped: AtomicUsize,
    connected: AtomicBool,
}

impl ServerState {
    pub(crate) fn new(config: McpServerConfig) -> Self {
        let prefix = naming::resolve_prefix(&config);
        Self {
            config,
            prefix,
            session: Mutex::new(None),
            tools: BlockingMutex::new(Vec::new()),
            error: BlockingMutex::new(None),
            pid: BlockingMutex::new(None),
            skipped: AtomicUsize::new(0),
            connected: AtomicBool::new(false),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.config.name
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    pub(crate) fn tool_infos(&self) -> Vec<McpToolInfo> {
        self.tools.lock().clone()
    }

    pub(crate) fn skipped_count(&self) -> usize {
        self.skipped.load(Ordering::Relaxed)
    }

    pub(crate) fn note_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// 记录错误并把连接标记为不可用。
    pub(crate) fn mark_error(&self, message: impl Into<String>) {
        *self.error.lock() = Some(message.into());
        self.connected.store(false, Ordering::SeqCst);
    }

    /// 开始重连（UI 上能立刻看到"正在重连"）。
    pub(crate) fn mark_reconnecting(&self) {
        *self.error.lock() = Some("正在重连……".to_string());
        self.connected.store(false, Ordering::SeqCst);
    }

    pub(crate) fn snapshot(&self) -> McpServerState {
        McpServerState {
            name: self.config.name.clone(),
            ok: self.connected.load(Ordering::SeqCst),
            tools: self.tools.lock().len(),
            error: self.error.lock().clone(),
            pid: *self.pid.lock(),
        }
    }

    /// 建连接 → initialize → notifications/initialized → tools/list。
    ///
    /// 任何一步失败都返回中文错误，并在 `error` 里留下原因；旧连接不会被破坏
    /// （只有握手成功才替换当前连接）。
    pub(crate) async fn connect(&self, paths: &Paths) -> anyhow::Result<Vec<McpToolInfo>> {
        let server_name = self.config.name.clone();
        let startup = Duration::from_secs(self.config.startup_timeout_secs.max(1));
        let session: Arc<dyn Session> = match self.config.transport {
            McpTransport::Stdio => Arc::new(
                stdio::StdioSession::spawn(&self.config, paths)
                    .await
                    .with_context(|| format!("启动 MCP 服务器「{server_name}」失败"))?,
            ),
            McpTransport::Http => Arc::new(
                http::HttpSession::new(&self.config)
                    .with_context(|| format!("创建 MCP 服务器「{server_name}」的 HTTP 连接失败"))?,
            ),
        };
        let pid = session.pid();
        let prefix = self.prefix.clone();
        let handshake_session = session.clone();
        let handshake_name = server_name.clone();
        let handshake = async move {
            let init_params = json!({
                "protocolVersion": crate::PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "neosonia",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            });
            request_with_timeout(
                handshake_session.as_ref(),
                "initialize",
                init_params,
                startup,
            )
            .await
            .context("initialize 握手失败")?;
            handshake_session
                .notify("notifications/initialized", json!({}))
                .await
                .context("发送 notifications/initialized 失败")?;
            let mut infos = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                let mut params = json!({});
                if let Some(cursor_value) = &cursor {
                    params["cursor"] = json!(cursor_value);
                }
                let result = request_with_timeout(
                    handshake_session.as_ref(),
                    "tools/list",
                    params,
                    startup,
                )
                .await
                .context("tools/list 失败")?;
                let page = result
                    .get("tools")
                    .and_then(|tools| tools.as_array())
                    .cloned()
                    .unwrap_or_default();
                for raw in page {
                    match McpToolInfo::from_json(&handshake_name, &prefix, &raw) {
                        Some(info) => infos.push(info),
                        None => tracing::warn!(
                            server = %handshake_name,
                            "MCP 服务器返回了没有名字的工具，已跳过"
                        ),
                    }
                }
                cursor = result
                    .get("nextCursor")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string());
                if cursor.is_none() {
                    break;
                }
            }
            Ok::<Vec<McpToolInfo>, anyhow::Error>(infos)
        };
        let handshake_result = tokio::time::timeout(startup, handshake).await;
        // 子进程 stderr 末尾内容：握手失败时拼进错误，方便排查命令行参数问题。
        let stderr_tail = session.stderr_tail();
        match handshake_result {
            Ok(Ok(infos)) => {
                {
                    let mut guard = self.session.lock().await;
                    if let Some(old) = guard.replace(session) {
                        old.close().await;
                    }
                }
                *self.pid.lock() = pid;
                *self.tools.lock() = infos.clone();
                *self.error.lock() = None;
                self.connected.store(true, Ordering::SeqCst);
                tracing::info!(server = %server_name, tools = infos.len(), "MCP 服务器已连接");
                Ok(infos)
            }
            Ok(Err(error)) => {
                session.close().await;
                let mut message = format!("{error:#}");
                if !stderr_tail.trim().is_empty() {
                    message.push_str(&format!(
                        "\n子进程 stderr（末尾）：{}",
                        neosonia_core::util::truncate_chars(stderr_tail.trim(), 1200)
                    ));
                }
                self.mark_error(message.clone());
                tracing::warn!(server = %server_name, error = %message, "MCP 服务器连接失败");
                Err(anyhow!("{message}"))
            }
            Err(_) => {
                session.close().await;
                let message = format!("启动握手超时（{} 秒）。", startup.as_secs());
                self.mark_error(message.clone());
                tracing::warn!(server = %server_name, "MCP 服务器启动握手超时");
                Err(anyhow!("{message}"))
            }
        }
    }

    /// 转发一次 `tools/call`；超时用 `call_timeout_secs`。
    pub(crate) async fn call_tool(
        &self,
        remote_name: &str,
        args: Value,
    ) -> anyhow::Result<Value> {
        let server_name = self.config.name.clone();
        let timeout = Duration::from_secs(self.config.call_timeout_secs.max(1));
        let guard = self.session.lock().await;
        let Some(session) = guard.as_ref() else {
            bail!("MCP 服务器「{server_name}」当前未连接。");
        };
        let params = json!({ "name": remote_name, "arguments": args });
        request_with_timeout(session.as_ref(), "tools/call", params, timeout)
            .await
            .with_context(|| {
                format!(
                    "调用 MCP 服务器「{server_name}」的 tools/call 失败（工具 {remote_name}，超时上限 {} 秒）",
                    timeout.as_secs()
                )
            })
    }

    /// 关闭连接（幂等）。
    pub(crate) async fn close(&self) {
        let session = self.session.lock().await.take();
        self.connected.store(false, Ordering::SeqCst);
        *self.pid.lock() = None;
        if let Some(session) = session {
            session.close().await;
        }
    }

    /// 同步兜底关闭：不能在异步上下文里 await 时（比如进程退出）直接丢连接杀进程。
    pub(crate) fn force_abort(&self) {
        if let Ok(mut guard) = self.session.try_lock() {
            if let Some(session) = guard.take() {
                drop(session);
            }
        }
        self.connected.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(name: &str) -> McpServerConfig {
        let mut config = McpServerConfig::default();
        config.name = name.to_string();
        config.command = "node".to_string();
        config
    }

    #[test]
    fn tool_info_from_json_defaults_schema() {
        let info = McpToolInfo::from_json("demo", "mcp_demo_", &json!({"name": "add"}))
            .expect("有名字就应解析出来");
        assert_eq!(info.full_name, "mcp_demo_add");
        assert_eq!(info.server, "demo");
        assert_eq!(info.schema["type"], "object");

        let info = McpToolInfo::from_json(
            "demo",
            "mcp_demo_",
            &json!({"name": "echo", "description": " 回显 ", "inputSchema": {"type":"object","properties":{"text":{"type":"string"}}}}),
        )
        .expect("完整工具应解析出来");
        assert_eq!(info.description, "回显");
        assert_eq!(info.schema["properties"]["text"]["type"], "string");

        assert!(McpToolInfo::from_json("demo", "mcp_demo_", &json!({})).is_none());
        assert!(McpToolInfo::from_json("demo", "mcp_demo_", &json!({"name": "  "})).is_none());
    }

    #[test]
    fn snapshot_of_fresh_state() {
        let state = ServerState::new(config("demo"));
        let snapshot = state.snapshot();
        assert_eq!(snapshot.name, "demo");
        assert!(!snapshot.ok);
        assert_eq!(snapshot.tools, 0);
        assert!(snapshot.error.is_none());
        assert!(snapshot.pid.is_none());
        assert!(state.is_enabled());

        state.mark_error("启动失败");
        let snapshot = state.snapshot();
        assert!(!snapshot.ok);
        assert_eq!(snapshot.error.as_deref(), Some("启动失败"));

        state.mark_reconnecting();
        assert!(state.snapshot().error.unwrap().contains("正在重连"));
    }

    #[tokio::test]
    async fn call_tool_without_connection_is_chinese_error() {
        let state = ServerState::new(config("demo"));
        let error = state
            .call_tool("add", json!({}))
            .await
            .err()
            .expect("未连接应该报错");
        assert!(error.to_string().contains("未连接"), "{error}");
    }
}
