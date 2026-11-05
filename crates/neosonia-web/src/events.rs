//! 把 `AgentEvent` 转成发给浏览器的 JSON。
//!
//! 契约（`docs/contracts-phase2.md` 0.2）：`AgentEvent` 用相邻标记
//! `#[serde(tag = "type", content = "data")]`，所以每个事件的形状都是
//! ```json
//! {"type":"text_delta","data":"..."}
//! {"type":"tool_started","data":{"call_id":"...","name":"..."}}
//! ```
//! WebSocket 直接把这里的 JSON 文本发出去；SPA 统一读 `ev.type` + `ev.data`。

use neosonia_core::AgentEvent;
use serde_json::{Value, json};

/// 序列化一条事件。失败时退化成一个 error 事件，绝不让 WS 循环崩掉。
pub fn event_json(event: &AgentEvent) -> Value {
    match serde_json::to_value(event) {
        Ok(v) => v,
        Err(e) => json!({
            "type": "error",
            "data": format!("事件序列化失败：{e}"),
        }),
    }
}

/// 序列化一条事件为 WS 文本帧内容。
pub fn event_text(event: &AgentEvent) -> String {
    event_json(event).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newtype_variants_use_data_field() {
        let v = event_json(&AgentEvent::TextDelta("你好".into()));
        assert_eq!(v["type"], "text_delta");
        assert_eq!(v["data"], "你好");

        let v = event_json(&AgentEvent::Phase("正在思考".into()));
        assert_eq!(v["type"], "phase");
        assert_eq!(v["data"], "正在思考");

        let v = event_json(&AgentEvent::Notice("提示".into()));
        assert_eq!(v["type"], "notice");
        assert_eq!(v["data"], "提示");

        let v = event_json(&AgentEvent::Error("出错了".into()));
        assert_eq!(v["type"], "error");
        assert_eq!(v["data"], "出错了");
    }

    #[test]
    fn struct_variants_use_data_object() {
        let v = event_json(&AgentEvent::TurnStarted {
            turn_id: "t1".into(),
            user_text: "读一下文件".into(),
        });
        assert_eq!(v["type"], "turn_started");
        assert_eq!(v["data"]["turn_id"], "t1");
        assert_eq!(v["data"]["user_text"], "读一下文件");

        let v = event_json(&AgentEvent::ToolFinished {
            call_id: "c1".into(),
            name: "read_file".into(),
            ok: true,
            output: "内容".into(),
            summary: Some("[ok] 12 行".into()),
            elapsed_ms: 34,
        });
        assert_eq!(v["type"], "tool_finished");
        assert_eq!(v["data"]["ok"], true);
        assert_eq!(v["data"]["elapsed_ms"], 34);
    }

    #[test]
    fn all_variants_serialize_without_panic() {
        use neosonia_core::AgentEvent as E;
        let samples = vec![
            E::TurnStarted { turn_id: "t".into(), user_text: "hi".into() },
            E::Phase("p".into()),
            E::ReasoningDelta("r".into()),
            E::TextDelta("t".into()),
            E::AssistantMessage { text: "a".into(), reasoning: None },
            E::ToolStarted {
                call_id: "c".into(),
                name: "n".into(),
                args: "{}".into(),
                preview: None,
            },
            E::ToolOutput { call_id: "c".into(), chunk: "o".into() },
            E::ToolFinished {
                call_id: "c".into(),
                name: "n".into(),
                ok: false,
                output: "x".into(),
                summary: None,
                elapsed_ms: 1,
            },
            E::SubagentStarted { id: "s".into(), label: "l".into(), task: "t".into() },
            E::SubagentActivity { id: "s".into(), line: "l".into() },
            E::SubagentFinished {
                id: "s".into(),
                label: "l".into(),
                ok: true,
                summary: "s".into(),
                elapsed_ms: 1,
            },
            E::MemoryChanged { action: "add".into(), content: "c".into() },
            E::Usage { prompt_tokens: 1, completion_tokens: 2, cached_tokens: 0 },
            E::ApprovalRequested {
                call_id: "c".into(),
                name: "n".into(),
                args: "{}".into(),
                reason: "r".into(),
                risk_level: 3,
                risk_reason: "高风险".into(),
                class: "shell".into(),
                emphasized: true,
            },
            E::RiskReviewed {
                tool: "run_command".into(),
                level: 3,
                class: "shell".into(),
                reason: "高风险".into(),
            },
            E::Cancelled { turn_id: "t".into() },
            E::Error("e".into()),
            E::Notice("n".into()),
            E::TurnFinished { turn_id: "t".into(), cancelled: false },
            E::ConversationStarted { conversation_id: "c".into(), title: "标题".into() },
            E::ImagesAttached { count: 1, source: "screenshot".into() },
        ];
        for ev in samples {
            let v = event_json(&ev);
            assert!(v.get("type").and_then(Value::as_str).is_some(), "事件缺少 type 字段");
            assert!(v.get("data").is_some(), "事件缺少 data 字段");
        }
    }
}
