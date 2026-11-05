//! http 传输：POST JSON-RPC 2.0，兼容 `application/json` 与单事件 SSE 响应。
//!
//! 这是 MCP 2025 规范的 Streamable HTTP 的最小客户端实现：
//! - 每次请求都带 `Accept: application/json, text/event-stream`；
//! - 服务器在 initialize 响应里给 `Mcp-Session-Id` 时，后续请求自动带上；
//! - 响应是 `text/event-stream` 时从 `data:` 行里取 JSON-RPC 回包；
//! - 通知（`notifications/initialized`）只检查 2xx，不解析响应体。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use neosonia_core::McpServerConfig;
use neosonia_core::util::truncate_chars;
use parking_lot::Mutex;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::Value;

use crate::jsonrpc;
use crate::transport::Session;

/// 安全网：单次 HTTP 请求本身再套一层超时（外层 `request_with_timeout` 通常更短）。
const RAW_HTTP_TIMEOUT: Duration = Duration::from_secs(300);

/// 一条 http 传输的 MCP 连接（无子进程，靠 reqwest 发 POST）。
pub(crate) struct HttpSession {
    server_name: String,
    url: String,
    client: reqwest::Client,
    session_id: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl HttpSession {
    pub(crate) fn new(config: &McpServerConfig) -> anyhow::Result<Self> {
        let url = config.url.trim().to_string();
        if url.is_empty() {
            bail!("MCP 服务器「{}」的 http url 为空。", config.name);
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            bail!(
                "MCP 服务器「{}」的 http url 必须以 http:// 或 https:// 开头：{url}",
                config.name
            );
        }
        let client = reqwest::Client::builder()
            .user_agent(format!("neosonia-mcp/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .with_context(|| format!("创建 MCP 服务器「{}」的 HTTP 客户端失败", config.name))?;
        Ok(Self {
            server_name: config.name.clone(),
            url,
            client,
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(0),
        })
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn session_header(&self) -> Option<String> {
        self.session_id.lock().clone()
    }

    /// 服务器给了 `Mcp-Session-Id` 就记下来，后续请求带上。
    fn remember_session(&self, response: &reqwest::Response) {
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .filter(|sid| !sid.is_empty())
            .map(|sid| sid.to_string());
        if let Some(session_id) = session_id {
            *self.session_id.lock() = Some(session_id);
        }
    }

    async fn post(&self, body: &Value) -> anyhow::Result<reqwest::Response> {
        let mut request = self
            .client
            .post(self.url.as_str())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .header("MCP-Protocol-Version", crate::PROTOCOL_VERSION)
            .body(body.to_string());
        if let Some(session_id) = self.session_header() {
            request = request.header("Mcp-Session-Id", session_id);
        }
        let response = tokio::time::timeout(RAW_HTTP_TIMEOUT, request.send())
            .await
            .map_err(|_| {
                anyhow!(
                    "向 MCP 服务器「{}」发 HTTP 请求超时（{} 秒）。",
                    self.server_name,
                    RAW_HTTP_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| {
                format!(
                    "向 MCP 服务器「{}」发 HTTP 请求失败（{}）",
                    self.server_name, self.url
                )
            })?;
        self.remember_session(&response);
        Ok(response)
    }
}

#[async_trait]
impl Session for HttpSession {
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id();
        let response = self.post(&jsonrpc::request(id, method, params)).await?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let text = response.text().await.with_context(|| {
            format!(
                "读取 MCP 服务器「{}」的 HTTP 响应体失败",
                self.server_name
            )
        })?;
        if !status.is_success() {
            bail!(
                "MCP 服务器「{}」返回 HTTP {}：{}",
                self.server_name,
                status.as_u16(),
                truncate_chars(text.trim(), 500)
            );
        }
        if content_type.contains("text/event-stream") {
            jsonrpc::response_from_sse(&text, id).with_context(|| {
                format!(
                    "解析 MCP 服务器「{}」的 SSE 响应失败（{method}）",
                    self.server_name
                )
            })
        } else {
            jsonrpc::response_from_json(&text, id).with_context(|| {
                format!(
                    "解析 MCP 服务器「{}」的 JSON 响应失败（{method}）",
                    self.server_name
                )
            })
        }
    }

    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        let response = self.post(&jsonrpc::notification(method, params)).await?;
        let status = response.status();
        // 通知的响应体一般是空的（202 Accepted），不解析。
        let _ = response.text().await;
        if !status.is_success() {
            bail!(
                "MCP 服务器「{}」拒绝了通知 {method}（HTTP {}）。",
                self.server_name,
                status.as_u16()
            );
        }
        Ok(())
    }

    fn pid(&self) -> Option<u32> {
        None
    }

    async fn close(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosonia_core::McpTransport;

    fn config(url: &str) -> McpServerConfig {
        let mut config = McpServerConfig::default();
        config.name = "remote".to_string();
        config.transport = McpTransport::Http;
        config.url = url.to_string();
        config
    }

    #[test]
    fn rejects_missing_or_bad_url() {
        let error = HttpSession::new(&config("")).err().expect("空 url 应报错");
        assert!(error.to_string().contains("url"), "{error}");

        let error = HttpSession::new(&config("ftp://example.com/mcp"))
            .err()
            .expect("非 http url 应报错");
        assert!(error.to_string().contains("http://"), "{error}");
    }

    #[test]
    fn builds_session_for_http_url() {
        let session = HttpSession::new(&config("http://127.0.0.1:9/mcp")).expect("合法 url");
        assert_eq!(session.pid(), None);
        assert!(session.stderr_tail().is_empty());
        assert!(session.session_header().is_none());
        assert_eq!(session.next_id(), 1);
        assert_eq!(session.next_id(), 2);
    }
}
