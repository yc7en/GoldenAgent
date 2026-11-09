use goldenagent_core::{
    ArtifactAction, ContentBlock, Message, SessionEvent,
};
use goldenagent_session::{
    derive_artifacts, derive_messages, derive_ui_state, migrate_lines_to_events,
    Session,
};

#[test]
fn test_event_sourcing_deterministic_projection() {
    let events = vec![
        SessionEvent::UserPrompt {
            id: "usr-1".into(),
            text: "写一个 hello.rs".into(),
            timestamp_ms: 1000,
        },
        SessionEvent::AssistantThinking {
            thought: "需要先规划代码结构".into(),
            timestamp_ms: 1010,
        },
        SessionEvent::ToolCall {
            call_id: "call-1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/hello.rs",
                "content": "fn main() { println!(\"Hello\"); }"
            }),
            summary: Some("创建源文件".into()),
            timestamp_ms: 1020,
        },
        SessionEvent::ToolResult {
            call_id: "call-1".into(),
            ok: true,
            output: "写入 32 字节".into(),
            timestamp_ms: 1030,
        },
        SessionEvent::AssistantResponse {
            text: "文件已成功创建。".into(),
            timestamp_ms: 1040,
        },
    ];

    // 多次投影计算结果必须逐字节严格一致（纯函数无副作用）
    let messages1 = derive_messages(&events);
    let messages2 = derive_messages(&events);
    assert_eq!(messages1, messages2);

    // 校验投影出的消息结构
    assert_eq!(messages1.len(), 4);
    assert_eq!(messages1[0].text(), "写一个 hello.rs");
    assert_eq!(messages1[1].tool_calls().len(), 1);
    assert_eq!(messages1[2].get_tool_results().len(), 1);
    assert_eq!(messages1[3].text(), "文件已成功创建。");

    // 验证 UI 状态投影
    let ui_state = derive_ui_state(&events);
    assert_eq!(ui_state.user_prompts_count, 1);
    assert_eq!(ui_state.assistant_turns_count, 1);
    assert_eq!(ui_state.tools_called_count, 1);
    assert!(!ui_state.has_compacted);

    // 验证代码产物投影
    let artifacts = derive_artifacts(&events);
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].path, "src/hello.rs");
    assert_eq!(artifacts[0].action, ArtifactAction::Created);
}

#[test]
fn test_compaction_checkpoint_preserves_full_history() {
    let mut session = Session::new();
    session.push_user("第一轮问题");
    session.push(Message::assistant(vec![ContentBlock::Text {
        text: "第一轮回答".into(),
    }]));
    session.push_user("第二轮问题");
    session.push(Message::assistant(vec![ContentBlock::Text {
        text: "第二轮回答".into(),
    }]));

    assert_eq!(session.events().len(), 4);
    assert_eq!(session.messages().len(), 4);

    // 模拟压缩事件（记录检查点标记，不物理抹除历史事件）
    session.record_event(SessionEvent::CompactionCheckpoint {
        checkpoint_id: "ckpt-1".into(),
        summary: "这是前两轮的浓缩检查点".into(),
        compacted_event_ids: vec!["usr-0".into()],
        timestamp_ms: 2000,
    });

    // 物理事件流依然完整保存了 5 个事件（含被遮蔽的事件与检查点事件）
    assert_eq!(session.events().len(), 5);

    // 投影出的供 LLM 消费的消息列表已折叠
    let projected_messages = session.messages();
    assert!(projected_messages[0].text().contains("<compacted-summary>"));
    assert!(projected_messages[0].text().contains("这是前两轮的浓缩检查点"));

    // UI 状态正确反映压缩标记
    let ui = session.derive_ui_state();
    assert!(ui.has_compacted);
    assert_eq!(ui.active_checkpoint.as_deref(), Some("ckpt-1"));
}

#[test]
fn test_generation_migration_chain_v1_to_v2() {
    // 旧版 v1 格式：每一行为一个完整的 Message JSON（带 role 和 blocks）
    let v1_jsonl = r#"{"role":"user","blocks":[{"type":"text","text":"legacy prompt"}]}
{"role":"assistant","blocks":[{"type":"text","text":"legacy reply"}]}"#;

    let lines: Vec<&str> = v1_jsonl.lines().collect();
    let migrated_events = migrate_lines_to_events(&lines, 1000);

    assert_eq!(migrated_events.len(), 2);
    assert!(matches!(
        &migrated_events[0],
        SessionEvent::UserPrompt { text, .. } if text == "legacy prompt"
    ));
    assert!(matches!(
        &migrated_events[1],
        SessionEvent::AssistantResponse { text, .. } if text == "legacy reply"
    ));

    // 从 v1 文本反序列化为 Session
    let session = Session::from_jsonl(v1_jsonl).expect("v1 无损升级");
    assert_eq!(session.len(), 2);
    assert_eq!(session.messages()[0].text(), "legacy prompt");
    assert_eq!(session.messages()[1].text(), "legacy reply");

    // 序列化为新版 v2 格式
    let v2_jsonl = session.to_jsonl();
    assert!(v2_jsonl.contains("\"kind\":\"user_prompt\""));
    assert!(v2_jsonl.contains("\"kind\":\"assistant_response\""));

    // 再从 v2 文本反序列化回来，逐字节一致
    let roundtrip_session = Session::from_jsonl(&v2_jsonl).expect("v2 反序列化");
    assert_eq!(roundtrip_session.messages(), session.messages());
}
