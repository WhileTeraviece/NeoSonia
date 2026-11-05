//! WebSocket 桥：把引擎事件广播实时推给浏览器。
//!
//! - 连接：`GET /ws?token=<device token>`（浏览器不能给 `new WebSocket` 加
//!   Authorization 头，所以 token 走 query；**服务端日志不打印 token 原文**）。
//! - 可选 `conv_id` / `since_seq`：客户端重连时带上当前会话与最后一条消息 seq，
//!   hello 里会原样回显 `resume`，客户端据此重新拉取会话消息补上断线期间的内容。
//! - 连接成功先推 `{"type":"hello","data":{device,server,pending_reviews,resume,ts_ms}}`；
//! - 之后把 `AgentEvent` 序列化为 `{"type":"...","data":...}` 原样转发；
//! - 客户端可发 `{"type":"cancel"}` 取消当前轮、`{"type":"ping"}` 心跳（服务端回 `pong`）。
//!   客户端每 25s ping，连续 3 次无响应才判定断开；短暂断开先静默重试 2 次。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use neosonia_core::TrustedDevice;
use neosonia_core::util::now_ms;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::VERSION;
use crate::auth::reject;
use crate::events::event_text;
use crate::review::ReviewResolution;
use crate::server::Inner;

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    #[serde(default)]
    pub token: Option<String>,
    /// 重连时带上当前会话 id（可选）。
    #[serde(default)]
    pub conv_id: Option<String>,
    /// 重连时带上已知的最后一条消息 seq（可选）。
    #[serde(default)]
    pub since_seq: Option<i64>,
}

/// hello 里的 `resume` 字段：告诉客户端服务端已收到重连上下文。
fn resume_payload(query: &WsQuery) -> Option<Value> {
    let conv = query
        .conv_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some(json!({
        "conv_id": conv,
        "since_seq": query.since_seq.unwrap_or(-1),
    }))
}

/// `GET /ws?token=`。
pub async fn ws_handler(
    State(inner): State<Arc<Inner>>,
    Query(query): Query<WsQuery>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let token = query
        .token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|raw| raw.trim().split_once(' '))
                .and_then(|(scheme, token)| {
                    if scheme.eq_ignore_ascii_case("bearer") {
                        Some(token.trim().to_string())
                    } else {
                        None
                    }
                })
        });
    let resume = resume_payload(&query);
    let addr = peer.ip().to_string();
    let device = token
        .as_deref()
        .and_then(|t| inner.devices.authenticate(t, Some(&addr)));
    match device {
        Some(device) => ws.on_upgrade(move |socket| bridge(socket, inner, device, resume)),
        None => reject(
            StatusCode::UNAUTHORIZED,
            "WebSocket 未授权：请先配对，并在 /ws?token= 带上设备 token。",
        ),
    }
}

/// select 一轮的结果。
enum Next {
    /// 要发给浏览器的一帧文本。
    Out(String),
    /// 关闭连接。
    Close,
}

async fn bridge(
    mut socket: WebSocket,
    inner: Arc<Inner>,
    device: TrustedDevice,
    resume: Option<Value>,
) {
    let mut rx = inner.events.subscribe();
    // 审查队列的独立广播通道（只读展示，远程不能批准）。
    let mut review_rx = crate::review::review_hub().subscribe();
    let mut resolved_rx = crate::review::review_hub().subscribe_resolved();
    let pending_reviews = crate::review::review_hub().pending();

    let hello = json!({
        "type": "hello",
        "data": {
            "device": device,
            "server": {
                "hostname": crate::net::hostname(),
                "version": VERSION,
                "busy": inner.engine.is_busy(),
                "model": inner.settings.read().server.model.clone(),
            },
            "pending_reviews": pending_reviews,
            "resume": resume,
            "ts_ms": now_ms(),
        }
    });
    if socket.send(Message::Text(hello.to_string().into())).await.is_err() {
        return;
    }

    loop {
        let next = tokio::select! {
            biased;
            event = rx.recv() => match event {
                Ok(event) => Next::Out(event_text(&event)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "WebSocket 事件积压，已跳过部分旧事件");
                    Next::Out(json!({
                        "type": "notice",
                        "data": format!("事件积压，已跳过 {skipped} 条旧事件（不影响当前状态）。"),
                    }).to_string())
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => Next::Close,
            },
            review = review_rx.recv() => match review {
                Ok(item) => Next::Out(risk_review_text(&item)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => continue,
            },
            resolved = resolved_rx.recv() => match resolved {
                Ok(update) => Next::Out(review_resolution_text(&update)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => continue,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    match client_command(&inner, text.as_str()) {
                        Some(reply) => Next::Out(reply),
                        None => continue,
                    }
                }
                Some(Ok(Message::Close(_))) | None => Next::Close,
                Some(Err(_)) => Next::Close,
                Some(Ok(_)) => continue,
            },
        };
        match next {
            Next::Out(text) => {
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            Next::Close => break,
        }
    }
    // 让 tungstenite 发完关闭帧。
    let _ = socket.send(Message::Close(None)).await;
}

/// 处理浏览器发来的控制指令；返回要回复的一帧（None = 不回）。
fn client_command(inner: &Arc<Inner>, raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let kind = value.get("type").and_then(Value::as_str)?;
    match kind {
        "cancel" => {
            inner.engine.cancel();
            tracing::info!("远程客户端请求取消当前轮");
            Some(json!({ "type": "notice", "data": "已请求取消当前任务。" }).to_string())
        }
        "ping" => Some(json!({ "type": "pong", "data": now_ms() }).to_string()),
        "close" => Some(json!({ "type": "pong", "data": "bye" }).to_string()),
        _ => None,
    }
}

/// 审查处理结果帧（桌面端同意/拒绝/超时后）：远程据此清横幅并刷新消息。
fn review_resolution_text(update: &ReviewResolution) -> String {
    match update {
        ReviewResolution::Resolved(call_id) => json!({
            "type": "review_resolved",
            "data": { "call_id": call_id },
        })
        .to_string(),
        ReviewResolution::Cleared => json!({
            "type": "review_cleared",
            "data": {},
        })
        .to_string(),
    }
}

/// 审查事件帧：`{"type":"risk_review","data":{call_id,tool,risk_level,risk_reason,emphasized}}`。
///
/// 远程端只读展示，**不含任何批准入口**；批准只能在桌面端完成。
fn risk_review_text(item: &crate::review::ReviewItem) -> String {
    json!({
        "type": "risk_review",
        "data": {
            "call_id": item.call_id.as_str(),
            "tool": item.tool.as_str(),
            "risk_level": item.risk_level.as_str(),
            "risk_reason": item.risk_reason.as_str(),
            "emphasized": item.emphasized,
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_shape_is_type_data() {
        let hello = json!({"type": "hello", "data": {"server": {"version": VERSION}}});
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["data"]["server"]["version"], VERSION);
    }

    #[test]
    fn risk_review_frame_shape() {
        let item = crate::review::ReviewItem {
            call_id: "c1".into(),
            tool: "run_command".into(),
            args_summary: "rm -rf /".into(),
            risk_level: "critical".into(),
            risk_reason: "会删除系统目录".into(),
            class: "shell".into(),
            emphasized: true,
        };
        let value: Value = serde_json::from_str(&risk_review_text(&item)).expect("解析");
        assert_eq!(value["type"], "risk_review");
        assert_eq!(value["data"]["call_id"], "c1");
        assert_eq!(value["data"]["tool"], "run_command");
        assert_eq!(value["data"]["risk_level"], "critical");
        assert_eq!(value["data"]["risk_reason"], "会删除系统目录");
        assert_eq!(value["data"]["emphasized"], true);
        // 只读：帧里不应该有任何批准/同意字段。
        assert!(value["data"].get("approve").is_none());
        assert!(value["data"].get("approved").is_none());
    }

    #[test]
    fn review_resolution_frame_shape() {
        let resolved: Value =
            serde_json::from_str(&review_resolution_text(&ReviewResolution::Resolved("c1".into())))
                .expect("解析");
        assert_eq!(resolved["type"], "review_resolved");
        assert_eq!(resolved["data"]["call_id"], "c1");

        let cleared: Value =
            serde_json::from_str(&review_resolution_text(&ReviewResolution::Cleared)).expect("解析");
        assert_eq!(cleared["type"], "review_cleared");
    }

    #[test]
    fn ws_query_parses_resume_params() {
        let q: WsQuery =
            serde_json::from_value(json!({"token": "t", "conv_id": "c1", "since_seq": 42}))
                .expect("解析 ws query");
        assert_eq!(q.token.as_deref(), Some("t"));
        assert_eq!(q.conv_id.as_deref(), Some("c1"));
        assert_eq!(q.since_seq, Some(42));

        // 旧客户端不传新参数也能连。
        let q: WsQuery = serde_json::from_value(json!({"token": "t"})).expect("解析旧 query");
        assert!(q.conv_id.is_none());
        assert!(q.since_seq.is_none());
    }

    #[test]
    fn resume_payload_echoes_context() {
        let q = WsQuery {
            token: None,
            conv_id: Some(" conv-1 ".into()),
            since_seq: Some(120),
        };
        let payload = resume_payload(&q).expect("应有 resume");
        assert_eq!(payload["conv_id"], "conv-1");
        assert_eq!(payload["since_seq"], 120);

        // 没给 conv_id 时不产生 resume 字段。
        let empty = WsQuery { token: None, conv_id: None, since_seq: Some(1) };
        assert!(resume_payload(&empty).is_none());
        // 空白 conv_id 也忽略，since_seq 默认 -1。
        let blank = WsQuery { token: None, conv_id: Some("  ".into()), since_seq: None };
        assert!(resume_payload(&blank).is_none());
        let no_seq = WsQuery {
            token: None,
            conv_id: Some("c".into()),
            since_seq: None,
        };
        assert_eq!(resume_payload(&no_seq).unwrap()["since_seq"], -1);
    }

    #[test]
    fn cancel_command_is_parsed() {
        let value: Value = serde_json::from_str(r#"{"type":"cancel"}"#).expect("解析");
        assert_eq!(value["type"], "cancel");
        // 未知指令不崩。
        let value: Value = serde_json::from_str(r#"{"type":"whatever"}"#).expect("解析");
        assert_eq!(value["type"], "whatever");
        assert!(serde_json::from_str::<Value>("not json").is_err());
    }
}


