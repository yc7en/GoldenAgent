use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTurns,
    MaxTokens,
    Cancelled,
    BudgetExceeded,
    Error,
}

/// 系统对外唯一的进度出口。
///
/// 上层（Tauri / CLI / 测试）只认识这个枚举，不认识 AgentLoop 的内部字段。
/// 想换前端、加 HTTP 接口、做回放调试，都不用改循环本身。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TurnStarted {
        turn: u32,
    },
    TextDelta {
        text: String,
    },
    /// 推理模型的思考过程，与 `TextDelta` **并列而不是包含**。
    ///
    /// 前端可以把它画成一块可折叠的"思考中"，也可以完全忽略；
    /// 但绝不能把它当正文——它不进消息、不落盘、不回灌给模型。
    /// 分开的理由很实际：正文和思考混在一起，下一次请求就会把
    /// 上一轮的思考当成自己说过的话发回去，既浪费额度又会误导模型。
    ThinkingDelta {
        text: String,
    },
    ToolStarted {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolFinished {
        id: String,
        name: String,
        ok: bool,
        output: String,
    },
    PermissionRequested {
        id: String,
        name: String,
        summary: String,
    },
    ContextCompacted {
        removed: usize,
    },
    TurnFinished {
        turn: u32,
    },
    Finished {
        reason: StopReason,
    },
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 这些 JSON 是**跨语言契约**：前端、日志、回放、测试都按这份形状消费。
    /// 改这里之前先想清楚——变体名和字段名一旦上线就不能再改，只能加新变体。
    #[test]
    fn event_json_shape_is_stable() {
        let cases = vec![
            (Event::TurnStarted { turn: 1 }, json!({"type": "turn_started", "turn": 1})),
            (Event::TextDelta { text: "hi".into() }, json!({"type": "text_delta", "text": "hi"})),
            (
                Event::ThinkingDelta { text: "嗯".into() },
                json!({"type": "thinking_delta", "text": "嗯"}),
            ),
            (
                Event::ToolStarted {
                    id: "c1".into(),
                    name: "read_file".into(),
                    input: json!({"path": "a.txt"}),
                },
                json!({"type": "tool_started", "id": "c1", "name": "read_file", "input": {"path": "a.txt"}}),
            ),
            (
                Event::ToolFinished {
                    id: "c1".into(),
                    name: "read_file".into(),
                    ok: false,
                    output: "越界".into(),
                },
                json!({"type": "tool_finished", "id": "c1", "name": "read_file", "ok": false, "output": "越界"}),
            ),
            (
                Event::PermissionRequested {
                    id: "c1".into(),
                    name: "write_file".into(),
                    summary: "写 a.txt".into(),
                },
                json!({"type": "permission_requested", "id": "c1", "name": "write_file", "summary": "写 a.txt"}),
            ),
            (
                Event::ContextCompacted { removed: 4 },
                json!({"type": "context_compacted", "removed": 4}),
            ),
            (Event::TurnFinished { turn: 1 }, json!({"type": "turn_finished", "turn": 1})),
            (
                Event::Finished { reason: StopReason::MaxTurns },
                json!({"type": "finished", "reason": "max_turns"}),
            ),
            (Event::Error { message: "boom".into() }, json!({"type": "error", "message": "boom"})),
        ];

        for (event, expected) in cases {
            let actual = serde_json::to_value(&event).expect("serialize");
            assert_eq!(actual, expected, "事件 JSON 形状变化: {event:?}");
        }
    }

    #[test]
    fn event_roundtrip() {
        let event = Event::ToolFinished {
            id: "c1".into(),
            name: "list_dir".into(),
            ok: true,
            output: "a.txt".into(),
        };
        let text = serde_json::to_string(&event).unwrap();
        let back: Event = serde_json::from_str(&text).unwrap();
        assert_eq!(back, event);
    }
}
