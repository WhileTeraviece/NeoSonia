//! JSON-RPC 2.0 编解码与 SSE 解析（MCP 的线协议）。
//!
//! MCP 的 stdio 传输是 newline-delimited JSON-RPC：一行一条消息。
//! http 传输的响应可能是普通 JSON，也可能是单事件 SSE（`data: {...}`）。

use anyhow::{anyhow, bail};
use neosonia_core::util::truncate_chars;
use serde_json::{Value, json};

pub(crate) const JSONRPC_VERSION: &str = "2.0";

/// 构造 JSON-RPC 2.0 请求。
pub(crate) fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "method": method, "params": params })
}

/// 构造 JSON-RPC 2.0 通知（没有 id，服务器不回包）。
pub(crate) fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": JSONRPC_VERSION, "method": method, "params": params })
}

/// 服务器返回的 JSON-RPC 错误。
#[derive(Clone, Debug, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC 错误 {}：{}", self.code, self.message)?;
        if let Some(data) = &self.data {
            write!(f, "（{}）", truncate_chars(&data.to_string(), 300))?;
        }
        Ok(())
    }
}

impl std::error::Error for RpcError {}

/// 从服务器收到的一条消息。
#[derive(Clone, Debug)]
pub(crate) enum ServerMessage {
    /// 对某个请求的响应。
    Response { id: u64, result: Result<Value, RpcError> },
    /// 通知（例如 `notifications/tools/list_changed`），不需要回包。
    Notification,
    /// 服务器反向请求客户端（sampling / roots 等），本客户端不支持。
    Request { method: String },
}

/// 判断一条 JSON 消息是响应、通知还是服务器反向请求。
pub(crate) fn classify(value: &Value) -> ServerMessage {
    let Some(id) = value.get("id").and_then(|v| v.as_u64()) else {
        return ServerMessage::Notification;
    };
    if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
        return ServerMessage::Request {
            method: method.to_string(),
        };
    }
    if let Some(error) = value.get("error") {
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-32603);
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("未知错误")
            .to_string();
        let data = error.get("data").cloned();
        return ServerMessage::Response {
            id,
            result: Err(RpcError {
                code,
                message,
                data,
            }),
        };
    }
    let result = value.get("result").cloned().unwrap_or(Value::Null);
    ServerMessage::Response {
        id,
        result: Ok(result),
    }
}

/// 解析 stdio 上的一行 JSON-RPC 消息。
pub(crate) fn parse_line(line: &str) -> anyhow::Result<ServerMessage> {
    let value: Value =
        serde_json::from_str(line).map_err(|e| anyhow!("解析 JSON-RPC 行失败：{e}"))?;
    Ok(classify(&value))
}

/// 从 SSE 文本里取出所有事件的 data 负载（多行 `data:` 用换行拼接）。
pub(crate) fn sse_data_events(text: &str) -> Vec<String> {
    let mut events = Vec::new();
    let mut current = String::new();
    for raw_line in text.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            if !current.is_empty() {
                events.push(std::mem::take(&mut current));
            }
            continue;
        }
        // SSE 注释行直接忽略
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(rest);
        }
        // event: / id: / retry: 字段本客户端用不到，忽略
    }
    if !current.is_empty() {
        events.push(current);
    }
    events
}

/// 从 SSE 响应里挑出 id 匹配的 JSON-RPC 结果。
pub(crate) fn response_from_sse(text: &str, id: u64) -> anyhow::Result<Value> {
    let mut found: Option<Result<Value, RpcError>> = None;
    for data in sse_data_events(text) {
        let trimmed = data.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(e) => {
                tracing::debug!(error = %e, "SSE data 不是合法 JSON，已跳过");
                continue;
            }
        };
        if let ServerMessage::Response {
            id: response_id,
            result,
        } = classify(&value)
        {
            if response_id == id {
                found = Some(result);
            }
        }
    }
    match found {
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(anyhow::Error::new(error)),
        None => bail!("SSE 响应里没有找到 id = {id} 的 JSON-RPC 回包。"),
    }
}

/// 从普通 JSON 响应里挑出 id 匹配的 JSON-RPC 结果（也容忍批量数组）。
pub(crate) fn response_from_json(text: &str, id: u64) -> anyhow::Result<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("HTTP 响应体为空，预期是 JSON-RPC 回包。");
    }
    let value: Value = serde_json::from_str(trimmed).map_err(|e| {
        anyhow!(
            "解析 HTTP JSON-RPC 响应失败：{e}（响应前 200 字符：{}）",
            truncate_chars(trimmed, 200)
        )
    })?;
    let messages = match value {
        Value::Array(items) => items,
        other => vec![other],
    };
    for value in messages {
        if let ServerMessage::Response {
            id: response_id,
            result,
        } = classify(&value)
        {
            if response_id == id {
                return result.map_err(anyhow::Error::new);
            }
        }
    }
    bail!(
        "HTTP 响应里没有找到 id = {id} 的 JSON-RPC 回包（响应前 200 字符：{}）。",
        truncate_chars(trimmed, 200)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_and_notification_shape() {
        let request = request(7, "tools/list", json!({}));
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 7);
        assert_eq!(request["method"], "tools/list");
        assert!(request["params"].is_object());

        let notification = notification("notifications/initialized", json!({}));
        assert_eq!(notification["jsonrpc"], "2.0");
        assert!(notification.get("id").is_none());
        assert_eq!(notification["method"], "notifications/initialized");
    }

    #[test]
    fn classify_success_and_error() {
        let ok = classify(&json!({"jsonrpc":"2.0","id":1,"result":{"tools":[]}}));
        match ok {
            ServerMessage::Response { id, result } => {
                assert_eq!(id, 1);
                assert_eq!(result.unwrap()["tools"], json!([]));
            }
            other => panic!("应该是响应：{other:?}"),
        }

        let failed = classify(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "error": {"code": -32601, "message": "方法不存在", "data": {"method": "x"}}
        }));
        match failed {
            ServerMessage::Response { id, result } => {
                assert_eq!(id, 2);
                let err = result.unwrap_err();
                assert_eq!(err.code, -32601);
                assert!(err.message.contains("方法不存在"));
                assert_eq!(err.data.clone().unwrap()["method"], "x");
                assert!(err.to_string().contains("-32601"));
            }
            other => panic!("应该是错误响应：{other:?}"),
        }
    }

    #[test]
    fn classify_ignores_notification_and_flags_server_request() {
        assert!(matches!(
            classify(&json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"})),
            ServerMessage::Notification
        ));
        match classify(&json!({"jsonrpc":"2.0","id":9,"method":"sampling/createMessage"})) {
            ServerMessage::Request { method } => assert_eq!(method, "sampling/createMessage"),
            other => panic!("应该是服务器反向请求：{other:?}"),
        }
    }

    #[test]
    fn parse_line_reports_chinese_error() {
        let err = parse_line("{不是 JSON").unwrap_err();
        assert!(err.to_string().contains("解析 JSON-RPC 行失败"), "{err}");
    }

    #[test]
    fn sse_single_event() {
        let text = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let events = sse_data_events(text);
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("\"ok\":true"));
        assert_eq!(response_from_sse(text, 1).unwrap()["ok"], true);
    }

    #[test]
    fn sse_multiline_data_and_multiple_events() {
        let text = concat!(
            ": 这是一条注释\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\n",
            "data: \"method\":\"notifications/tools/list_changed\"}\n",
            "\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"value\":42}}\n",
            "\n"
        );
        let events = sse_data_events(text);
        assert_eq!(events.len(), 2, "事件数：{events:?}");
        assert!(events[0].contains('\n'), "多行 data 应该用换行拼接");
        assert_eq!(response_from_sse(text, 7).unwrap()["value"], 42);
    }

    #[test]
    fn sse_missing_id_errors() {
        let text = "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n\n";
        let err = response_from_sse(text, 3).unwrap_err();
        assert!(err.to_string().contains("没有找到 id = 3"), "{err}");
    }

    #[test]
    fn sse_error_payload_becomes_error() {
        let text = "data: {\"jsonrpc\":\"2.0\",\"id\":5,\"error\":{\"code\":-32000,\"message\":\"内部错误\"}}\n\n";
        let err = response_from_sse(text, 5).unwrap_err();
        assert!(err.to_string().contains("内部错误"), "{err}");
    }

    #[test]
    fn json_response_and_batch() {
        let single = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"a\":1}}";
        assert_eq!(response_from_json(single, 1).unwrap()["a"], 1);

        let batch = concat!(
            "[{\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"},",
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"b\":2}}]"
        );
        assert_eq!(response_from_json(batch, 4).unwrap()["b"], 2);

        assert!(response_from_json("", 1).unwrap_err().to_string().contains("为空"));
        assert!(
            response_from_json("{\"no\":\"id\"}", 1)
                .unwrap_err()
                .to_string()
                .contains("没有找到")
        );
    }
}
