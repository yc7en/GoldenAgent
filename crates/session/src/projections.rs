use std::collections::HashSet;

use goldenagent_core::{
    Artifact, ArtifactAction, ContentBlock, Message, Role, SessionEvent, ToolCall, ToolResult,
    UIState,
};

/// 检查点前导，与 agent-loop compaction 保持一致
const CHECKPOINT_PREAMBLE: &str = "\
下面这段是自动生成的检查点，把本次对话较早的部分压缩了，用于释放上下文空间。\
把它当作已经成立的背景来使用，不要在回答里复述它；直接从后面的消息继续任务，\
也不要提起这个检查点。";

/// 纯函数确定性投影：从不可变事件流中折叠计算出供 LLM 消费的消息列表。
///
/// 遵循 ARCHITECTURE.md A-19：
/// 状态不保存在消息里，而是从事件流动态计算得出。
/// 当存在上下文压缩标记（CompactionCheckpoint）时，被折叠的早期事件被动态替换为检查点消息，
/// 但底层的物理事件依然 100% 完整保留。
pub fn derive_messages(events: &[SessionEvent]) -> Vec<Message> {
    let mut compacted_ids = HashSet::new();
    let mut checkpoints = Vec::new();

    // 收集所有生效的检查点与其遮蔽的事件 ID
    for event in events {
        if let SessionEvent::CompactionCheckpoint {
            checkpoint_id,
            summary,
            compacted_event_ids,
            ..
        } = event
        {
            for id in compacted_event_ids {
                compacted_ids.insert(id.clone());
            }
            checkpoints.push((checkpoint_id.clone(), summary.clone()));
        }
    }

    let mut messages: Vec<Message> = Vec::new();

    // 如果有检查点，先在开头注入最新的检查点消息
    if let Some((_, summary)) = checkpoints.last() {
        let text = format!("{CHECKPOINT_PREAMBLE}\n\n<compacted-summary>\n{summary}\n</compacted-summary>");
        messages.push(Message::user(text));
    }

    for event in events {
        // 如果事件已被压缩检查点吸收，则在当前模型上下文中跳过
        if let Some(id) = event.id() {
            if compacted_ids.contains(id) {
                continue;
            }
        }

        match event {
            SessionEvent::GenerationHeader { .. } | SessionEvent::CompactionCheckpoint { .. } => {
                // 元信息与标记不在模型消息中直出
            }
            SessionEvent::ArtifactDeclared { .. } => {
                // 产物声明不直接作为聊天消息
            }
            SessionEvent::UserPrompt { text, .. } => {
                messages.push(Message::user(text.clone()));
            }
            SessionEvent::AssistantThinking { thought, .. } => {
                // 如果最后一条是 assistant，且没有工具调用，可选择追加；否则起一条 assistant
                if let Some(last) = messages.last_mut() {
                    if last.role == Role::Assistant && last.tool_calls().is_empty() {
                        last.blocks.push(ContentBlock::Text {
                            text: format!("<thinking>\n{thought}\n</thinking>\n"),
                        });
                        continue;
                    }
                }
                messages.push(Message::assistant(vec![ContentBlock::Text {
                    text: format!("<thinking>\n{thought}\n</thinking>\n"),
                }]));
            }
            SessionEvent::AssistantResponse { text, .. } => {
                if let Some(last) = messages.last_mut() {
                    if last.role == Role::Assistant && last.tool_calls().is_empty() {
                        last.blocks.push(ContentBlock::Text { text: text.clone() });
                        continue;
                    }
                }
                messages.push(Message::assistant(vec![ContentBlock::Text {
                    text: text.clone(),
                }]));
            }
            SessionEvent::ToolCall {
                call_id,
                name,
                input,
                ..
            } => {
                let call = ToolCall {
                    id: call_id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                };
                if let Some(last) = messages.last_mut() {
                    if last.role == Role::Assistant {
                        last.blocks.push(ContentBlock::ToolCall(call));
                        continue;
                    }
                }
                messages.push(Message::assistant(vec![ContentBlock::ToolCall(call)]));
            }
            SessionEvent::ToolResult {
                call_id,
                ok,
                output,
                ..
            } => {
                let result = ToolResult {
                    id: call_id.clone(),
                    ok: *ok,
                    output: output.clone(),
                };
                // 工具结果归在 User 消息中（符合模型厂商 Tool Results 契约）
                if let Some(last) = messages.last_mut() {
                    if last.role == Role::User && !last.get_tool_results().is_empty() {
                        last.blocks.push(ContentBlock::ToolResult(result));
                        continue;
                    }
                }
                messages.push(Message::tool_results(vec![result]));
            }
        }
    }

    messages
}

/// 纯函数确定性投影：从不可变事件流中折叠计算出前端 UI 状态与统计摘要。
pub fn derive_ui_state(events: &[SessionEvent]) -> UIState {
    let mut state = UIState::default();

    for event in events {
        match event {
            SessionEvent::UserPrompt { .. } => {
                state.user_prompts_count += 1;
                state.messages_count += 1;
            }
            SessionEvent::AssistantResponse { .. } => {
                state.assistant_turns_count += 1;
                state.messages_count += 1;
            }
            SessionEvent::ToolCall { .. } => {
                state.tools_called_count += 1;
            }
            SessionEvent::CompactionCheckpoint { checkpoint_id, .. } => {
                state.has_compacted = true;
                state.active_checkpoint = Some(checkpoint_id.clone());
            }
            _ => {}
        }
    }

    state.artifacts = derive_artifacts(events);
    state
}

/// 纯函数确定性投影：从不可变事件流中折叠计算出当前会话的代码产物与变更文件列表。
pub fn derive_artifacts(events: &[SessionEvent]) -> Vec<Artifact> {
    let mut artifacts: Vec<Artifact> = Vec::new();

    // 辅助追踪已发起的 ToolCall (call_id -> (name, input, timestamp))
    let mut pending_calls: std::collections::HashMap<String, (String, serde_json::Value, u64)> =
        std::collections::HashMap::new();

    for event in events {
        match event {
            SessionEvent::ArtifactDeclared {
                path,
                action,
                timestamp_ms,
            } => {
                if let Some(existing) = artifacts.iter_mut().find(|a| a.path == *path) {
                    existing.action = *action;
                    existing.updated_at_ms = *timestamp_ms;
                } else {
                    artifacts.push(Artifact {
                        path: path.clone(),
                        action: *action,
                        updated_at_ms: *timestamp_ms,
                    });
                }
            }
            SessionEvent::ToolCall {
                call_id,
                name,
                input,
                timestamp_ms,
                ..
            } => {
                pending_calls.insert(call_id.clone(), (name.clone(), input.clone(), *timestamp_ms));
            }
            SessionEvent::ToolResult {
                call_id,
                ok,
                timestamp_ms,
                ..
            } => {
                if *ok {
                    if let Some((name, input, _call_time)) = pending_calls.get(call_id) {
                        let path_opt = input.get("path").and_then(|v| v.as_str());
                        if let Some(raw_path) = path_opt {
                            let path = raw_path.to_string();
                            let action = if name == "write_file" {
                                if artifacts.iter().any(|a| a.path == path) {
                                    ArtifactAction::Modified
                                } else {
                                    ArtifactAction::Created
                                }
                            } else if name == "apply_patch" {
                                ArtifactAction::Modified
                            } else {
                                continue;
                            };

                            if let Some(existing) = artifacts.iter_mut().find(|a| a.path == path) {
                                existing.action = action;
                                existing.updated_at_ms = *timestamp_ms;
                            } else {
                                artifacts.push(Artifact {
                                    path,
                                    action,
                                    updated_at_ms: *timestamp_ms,
                                });
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    artifacts
}

/// 世代版本迁移链：将旧版 v1 消息或新版 v2 事件流无损统一转为 `Vec<SessionEvent>`。
pub fn migrate_lines_to_events(lines: &[&str], base_time_ms: u64) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    let cur_time = base_time_ms;

    for (idx, raw_line) in lines.iter().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        // 1. 尝试直接按 SessionEvent (v2) 反序列化
        if let Ok(event) = serde_json::from_str::<SessionEvent>(line) {
            events.push(event);
            continue;
        }

        // 2. 尝试按 legacy Message (v1) 反序列化
        if let Ok(msg) = serde_json::from_str::<Message>(line) {
            let step_time = cur_time + (idx as u64) * 10;
            let msg_events = message_to_events(&msg, step_time, idx);
            events.extend(msg_events);
            continue;
        }
    }

    events
}

/// 将单个 Message 拆解转换为等价的 SessionEvent 序列。
pub fn message_to_events(message: &Message, timestamp_ms: u64, seq: usize) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    match message.role {
        Role::User => {
            let mut prompt_texts = Vec::new();
            for block in &message.blocks {
                match block {
                    ContentBlock::Text { text } => prompt_texts.push(text.clone()),
                    ContentBlock::ToolResult(res) => {
                        events.push(SessionEvent::ToolResult {
                            call_id: res.id.clone(),
                            ok: res.ok,
                            output: res.output.clone(),
                            timestamp_ms,
                        });
                    }
                    _ => {}
                }
            }
            if !prompt_texts.is_empty() {
                events.push(SessionEvent::UserPrompt {
                    id: format!("usr-{seq}"),
                    text: prompt_texts.join("\n"),
                    timestamp_ms,
                });
            }
        }
        Role::Assistant => {
            for block in &message.blocks {
                match block {
                    ContentBlock::Text { text } => {
                        events.push(SessionEvent::AssistantResponse {
                            text: text.clone(),
                            timestamp_ms,
                        });
                    }
                    ContentBlock::ToolCall(call) => {
                        events.push(SessionEvent::ToolCall {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            input: call.input.clone(),
                            summary: None,
                            timestamp_ms,
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    events
}
