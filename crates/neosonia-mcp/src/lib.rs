//! NeoSonia 的 MCP（Model Context Protocol）客户端。
//!
//! 目标：把外部 MCP 服务器提供的工具变成 [`neosonia_core::Tool`]，让模型像调用
//! 本机工具一样调用它们；另外提供 `mcp_list_servers` 让模型自查服务器状态。
//!
//! 支持两种传输：
//! - `stdio`：`tokio::process::Command` 起子进程，newline-delimited JSON-RPC 2.0；
//! - `http`：POST JSON-RPC，兼容 `application/json` 与单事件 SSE 响应。
//!
//! 设计约定：
//! - 失败隔离：任何一台服务器启动 / 握手 / 调用失败都只影响它自己，绝不影响
//!   NeoSonia 的启动；所有错误都是中文文案 + anyhow 上下文；
//! - 串行请求：每台服务器一把异步 Mutex，同一时刻只有一个 JSON-RPC 请求在飞
//!   （MCP stdio 本身就是单通道，串行最稳）；
//! - 工具名：`<prefix><tool>`，prefix 默认 `mcp_<服务器名>_`；与已有工具重名时
//!   跳过并 warn，不覆盖别人的工具；
//! - 生命周期：调用 [`McpManager::shutdown`] 或管理器被 drop 时杀掉所有子进程。
//!
//! 用法（app 侧，必须在 tokio runtime 内调用）：
//! ```ignore
//! let manager = McpManager::start(settings.clone(), &registry, &paths).await?;
//! // ……模型循环里正常调用 registry 里的 mcp_* 工具……
//! manager.shutdown();
//! ```

mod http;
mod jsonrpc;
mod manager;
mod naming;
mod server;
mod stdio;
mod tool;
mod transport;

pub use manager::McpManager;
pub use server::{McpServerState, McpToolInfo};

/// 本客户端默认声明的 MCP 协议版本（真正的版本以 initialize 协商结果为准）。
pub const PROTOCOL_VERSION: &str = "2024-11-05";
