//! 把 MCP 工具包装成 [`neosonia_core::Tool`]，以及 `mcp_list_servers` 诊断工具。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use neosonia_core::util::{human_bytes, truncate_chars};
use neosonia_core::{Tool, ToolCtx, ToolOutcome, ToolRegistry, ToolSpec};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::server::{McpToolInfo, ServerState};

/// `mcp_list_servers` 的固定工具名。
pub(crate) const LIST_SERVERS_TOOL: &str = "mcp_list_servers";

/// 一个转发到 MCP `tools/call` 的工具。
pub(crate) struct McpTool {
    state: Arc<ServerState>,
    info: McpToolInfo,
}

impl McpTool {
    pub(crate) fn new(state: Arc<ServerState>, info: McpToolInfo) -> Self {
        Self { state, info }
    }

    /// 工具说明里带上服务器名：模型看到 `mcp_xxx_yyy` 时知道它来自哪台服务器。
    fn description(&self) -> String {
        let mut description = self.info.description.trim().to_string();
        if description.is_empty() {
            return format!(
                "由 MCP 服务器「{}」提供的工具 {}。",
                self.info.server, self.info.name
            );
        }
        if description.chars().count() > 900 {
            description = truncate_chars(&description, 900);
        }
        description.push_str(&format!("\n（由 MCP 服务器「{}」提供）", self.info.server));
        description
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            self.info.full_name.clone(),
            self.description(),
            self.info.schema.clone(),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        ctx.phase(format!("调用 MCP 工具 {}", self.info.full_name));
        match self.state.call_tool(&self.info.name, args).await {
            Ok(result) => {
                let (is_error, text) = format_tool_result(&result);
                if is_error {
                    ToolOutcome::err(text)
                } else {
                    ToolOutcome::ok(text)
                }
            }
            Err(error) => ToolOutcome::err(format!(
                "MCP 工具 {} 调用失败：{error:#}",
                self.info.full_name
            )),
        }
    }
}

/// 把一条工具信息注册进 registry；重名 / 超出全局上限时跳过并计数。
///
/// 返回 `true` 表示已注册（含"重启后覆盖本管理器自己的同名工具"）。
pub(crate) fn register_tool(
    registry: &ToolRegistry,
    registered: &Mutex<BTreeSet<String>>,
    state: &Arc<ServerState>,
    info: &McpToolInfo,
    total_limit: usize,
) -> bool {
    let mut registered = registered.lock();
    if registered.contains(&info.full_name) {
        // 重连场景：同名工具直接覆盖（schema / 描述可能已经变了）。
        registry.register(Arc::new(McpTool::new(state.clone(), info.clone())));
        return true;
    }
    if registry.get(&info.full_name).is_some() {
        tracing::warn!(
            tool = %info.full_name,
            server = %info.server,
            "MCP 工具与已存在的工具重名，已跳过注册"
        );
        state.note_skipped();
        return false;
    }
    if registered.len() >= total_limit {
        tracing::warn!(
            tool = %info.full_name,
            server = %info.server,
            limit = total_limit,
            "MCP 工具总数达到上限，已跳过注册"
        );
        state.note_skipped();
        return false;
    }
    registry.register(Arc::new(McpTool::new(state.clone(), info.clone())));
    registered.insert(info.full_name.clone());
    true
}

/// `mcp_list_servers`：给模型自查用的诊断工具（不占 `max_total_tools` 名额）。
pub(crate) struct ListServersTool {
    servers: Arc<BTreeMap<String, Arc<ServerState>>>,
    total_limit: usize,
}

impl ListServersTool {
    pub(crate) fn new(
        servers: Arc<BTreeMap<String, Arc<ServerState>>>,
        total_limit: usize,
    ) -> Self {
        Self {
            servers,
            total_limit,
        }
    }

    fn render(&self) -> String {
        let total = self.servers.len();
        let connected = self
            .servers
            .values()
            .filter(|state| state.snapshot().ok)
            .count();
        let tool_count: usize = self
            .servers
            .values()
            .map(|state| state.tool_infos().len())
            .sum();
        let mut out = format!(
            "MCP 客户端：共 {total} 台服务器，已连接 {connected} 台，工具 {tool_count} 个（全局上限 {} 个）。",
            self.total_limit
        );
        if total == 0 {
            out.push_str("\n当前没有配置任何 MCP 服务器。可在设置的 MCP 小节添加服务器后重启应用。");
            return out;
        }
        for (index, (_name, state)) in self.servers.iter().enumerate() {
            let snapshot = state.snapshot();
            out.push_str(&format!("\n{}. {}：", index + 1, snapshot.name));
            if snapshot.ok {
                out.push_str(&format!("已连接，工具 {} 个", snapshot.tools));
                if let Some(pid) = snapshot.pid {
                    out.push_str(&format!("，进程 pid {pid}"));
                }
                out.push('\n');
            } else {
                out.push_str("未连接");
                if let Some(error) = &snapshot.error {
                    out.push_str(&format!("，原因：{}", first_line(error)));
                }
                out.push('\n');
            }
            for info in state.tool_infos() {
                let summary = first_line(&info.description);
                if summary.is_empty() {
                    out.push_str(&format!("   - {}\n", info.full_name));
                } else {
                    out.push_str(&format!("   - {}：{}\n", info.full_name, summary));
                }
            }
            let skipped = state.skipped_count();
            if skipped > 0 {
                out.push_str(&format!(
                    "   （另有 {skipped} 个工具因重名或超出上限被跳过）\n"
                ));
            }
        }
        out.push_str(
            "\n调用 MCP 工具失败（离线、超时、参数错误）只返回该工具的中文错误，不影响其它工具。",
        );
        out
    }
}

#[async_trait]
impl Tool for ListServersTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            LIST_SERVERS_TOOL,
            "列出所有 MCP 服务器、它们的连接状态、错误原因和已注册的工具名（中文）。\
             当不确定某个 MCP 服务器是否在线、或需要确认 MCP 工具名时，先调用它。",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        )
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> ToolOutcome {
        ToolOutcome::ok(self.render())
    }
}

/// 把 MCP `tools/call` 的 result 转成给模型看的文本 + 是否失败。
///
/// MCP 的结果形状：`{content:[{type:"text",text},...], isError?:bool, structuredContent?}`。
pub(crate) fn format_tool_result(result: &Value) -> (bool, String) {
    let is_error = result
        .get("isError")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    if let Some(items) = result.get("content").and_then(|content| content.as_array()) {
        for item in items {
            let kind = item.get("type").and_then(|value| value.as_str()).unwrap_or("");
            match kind {
                "text" => {
                    if let Some(text) = item.get("text").and_then(|value| value.as_str()) {
                        parts.push(text.to_string());
                    }
                }
                "image" => {
                    let mime = item
                        .get("mimeType")
                        .and_then(|value| value.as_str())
                        .unwrap_or("未知类型");
                    let bytes = item
                        .get("data")
                        .and_then(|value| value.as_str())
                        .map(|data| data.len() * 3 / 4)
                        .unwrap_or(0);
                    parts.push(format!(
                        "[图片：{mime}，约 {}。本客户端不会把 MCP 图片转发给模型]",
                        human_bytes(bytes as u64)
                    ));
                }
                "audio" => {
                    let mime = item
                        .get("mimeType")
                        .and_then(|value| value.as_str())
                        .unwrap_or("未知类型");
                    parts.push(format!("[音频：{mime}，本客户端不支持回放]"));
                }
                "resource" => {
                    let uri = item
                        .pointer("/resource/uri")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    let text = item
                        .pointer("/resource/text")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    if text.is_empty() {
                        parts.push(format!("[资源：{uri}]"));
                    } else {
                        parts.push(format!("[资源：{uri}]\n{text}"));
                    }
                }
                "resource_link" => {
                    let uri = item.get("uri").and_then(|value| value.as_str()).unwrap_or("");
                    let name = item.get("name").and_then(|value| value.as_str()).unwrap_or("");
                    parts.push(format!("[资源链接：{name} {uri}]"));
                }
                other => parts.push(format!("[暂不支持的 MCP 内容类型：{other}]")),
            }
        }
    }
    if parts.is_empty() {
        let fallback = match result.get("structuredContent") {
            Some(structured) => {
                serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string())
            }
            None => serde_json::to_string(result).unwrap_or_default(),
        };
        parts.push(fallback);
    }
    let text = parts.join("\n");
    let text = text.trim().to_string();
    if text.is_empty() {
        (is_error, "（MCP 工具返回了空内容）".to_string())
    } else {
        (is_error, text)
    }
}

/// 只取第一行并截断，用于列表展示。
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    truncate_chars(line, 160)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use neosonia_core::McpServerConfig;

    fn config(name: &str) -> McpServerConfig {
        let mut config = McpServerConfig::default();
        config.name = name.to_string();
        config
    }

    fn info(full_name: &str, remote: &str) -> McpToolInfo {
        McpToolInfo {
            server: "demo".to_string(),
            name: remote.to_string(),
            full_name: full_name.to_string(),
            description: "测试工具".to_string(),
            schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn formats_text_result() {
        let (is_error, text) =
            format_tool_result(&json!({"content": [{"type": "text", "text": "结果"}]}));
        assert!(!is_error);
        assert_eq!(text, "结果");

        let (is_error, text) = format_tool_result(&json!({
            "content": [{"type": "text", "text": "参数错误"}],
            "isError": true
        }));
        assert!(is_error);
        assert_eq!(text, "参数错误");
    }

    #[test]
    fn formats_non_text_content() {
        let (_, text) = format_tool_result(&json!({
            "content": [
                {"type": "image", "mimeType": "image/png", "data": "AAAA"},
                {"type": "resource", "resource": {"uri": "file:///a.txt", "text": "内容"}},
                {"type": "resource_link", "uri": "https://example.com", "name": "示例"}
            ]
        }));
        assert!(text.contains("图片：image/png"), "{text}");
        assert!(text.contains("资源：file:///a.txt"), "{text}");
        assert!(text.contains("资源链接：示例"), "{text}");
    }

    #[test]
    fn formats_empty_and_structured() {
        let (_, text) = format_tool_result(&json!({"content": []}));
        assert!(!text.is_empty(), "空 content 也要给出可读文本");

        let (_, text) = format_tool_result(&json!({"structuredContent": {"sum": 5}}));
        assert!(text.contains("sum"), "{text}");
        assert!(text.contains('5'), "{text}");

        let (_, text) = format_tool_result(&json!({}));
        assert_eq!(text, "{}");
    }

    #[test]
    fn register_handles_conflict_and_limit() {
        let state = Arc::new(ServerState::new(config("demo")));
        let registry = ToolRegistry::new();
        let registered = Mutex::new(BTreeSet::new());

        assert!(register_tool(&registry, &registered, &state, &info("mcp_demo_add", "add"), 10));
        assert_eq!(registry.names(), vec!["mcp_demo_add".to_string()]);

        // 重启场景：本管理器自己的同名工具允许覆盖，不算冲突
        assert!(register_tool(&registry, &registered, &state, &info("mcp_demo_add", "add"), 10));
        assert_eq!(state.skipped_count(), 0);

        // 与外部工具重名：跳过并计数
        let external = ToolRegistry::new();
        external.register(Arc::new(DummyTool {
            name: "mcp_demo_echo",
        }));
        let state2 = Arc::new(ServerState::new(config("demo")));
        let registered2 = Mutex::new(BTreeSet::new());
        assert!(!register_tool(
            &external,
            &registered2,
            &state2,
            &info("mcp_demo_echo", "echo"),
            10
        ));
        assert_eq!(state2.skipped_count(), 1);
        assert!(external.get("mcp_demo_echo").is_some(), "原有工具不能被覆盖");

        // 超出全局上限：跳过并计数
        let empty = ToolRegistry::new();
        let state3 = Arc::new(ServerState::new(config("demo")));
        let registered3 = Mutex::new(BTreeSet::new());
        assert!(!register_tool(
            &empty,
            &registered3,
            &state3,
            &info("mcp_demo_add", "add"),
            0
        ));
        assert_eq!(state3.skipped_count(), 1);
        assert!(empty.is_empty());
    }

    #[test]
    fn list_servers_tool_renders_state() {
        let state = Arc::new(ServerState::new(config("demo")));
        state.mark_error("连接超时");
        let mut map = BTreeMap::new();
        map.insert("demo".to_string(), state);
        let tool = ListServersTool::new(Arc::new(map), 100);
        let text = tool.render();
        assert!(text.contains("共 1 台服务器"), "{text}");
        assert!(text.contains("demo：未连接"), "{text}");
        assert!(text.contains("连接超时"), "{text}");

        let empty = ListServersTool::new(Arc::new(BTreeMap::new()), 100);
        assert!(empty.render().contains("没有配置任何 MCP 服务器"));

        let spec = tool.spec();
        assert_eq!(spec.name, LIST_SERVERS_TOOL);
        assert_eq!(spec.parameters["type"], "object");
    }

    /// 只为占位用的外部工具（模拟与内置工具重名）。
    struct DummyTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for DummyTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(self.name, "占位工具", json!({"type": "object"}))
        }

        async fn call(&self, _ctx: &ToolCtx, _args: Value) -> ToolOutcome {
            ToolOutcome::ok("占位")
        }
    }
}
