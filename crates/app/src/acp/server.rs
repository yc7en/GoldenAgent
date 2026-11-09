use std::sync::Arc;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use goldenagent_core::{
    AcpNotification, AcpRequest, AcpResponse, CardInteraction, Event,
};
use crate::{AgentService, EventSink};

pub type NotificationSender = Arc<dyn Fn(AcpNotification) + Send + Sync>;

pub struct AcpServer {
    service: Arc<AgentService>,
}

impl AcpServer {
    pub fn new(service: Arc<AgentService>) -> Self {
        Self { service }
    }

    pub fn service(&self) -> &AgentService {
        &self.service
    }

    /// 处理单条传入的 ACP JSON-RPC 请求，返回对应的响应帧（若为通知则返回 None）。
    pub async fn handle_request(
        &self,
        req: AcpRequest,
        notify: Option<NotificationSender>,
    ) -> Option<AcpResponse> {
        let id = req.id.clone();
        let method = req.method.as_str();

        match method {
            "ping" => Some(AcpResponse::ok(id, json!({ "pong": true }))),

            "session/create" => {
                let space_id = req.params.get("space_id").and_then(Value::as_str);
                self.service.new_session_in(space_id).await;
                let id_str = self.service.ensure_session_id().unwrap_or_else(goldenagent_session::SessionStore::new_id);
                Some(AcpResponse::ok(id, json!({ "session_id": id_str })))
            }

            "session/list" => {
                let space_id = req.params.get("space_id").and_then(Value::as_str);
                let include_archived = req.params.get("include_archived").and_then(Value::as_bool).unwrap_or(false);
                match self.service.list_sessions(space_id, include_archived).await {
                    Ok(items) => {
                        let json_items: Vec<Value> = items.into_iter().map(|item| {
                            json!({
                                "id": item.id,
                                "title": item.title,
                                "messages": item.messages,
                                "updated_at_ms": item.updated_at_ms,
                                "space_id": item.space_id,
                                "workspace": item.workspace.to_string_lossy(),
                                "pinned": item.pinned,
                                "archived": item.archived,
                            })
                        }).collect();
                        Some(AcpResponse::ok(id, json!({ "sessions": json_items })))
                    }
                    Err(e) => Some(AcpResponse::err(id, -32000, e.to_string())),
                }
            }

            "agent/cancel" => {
                let cancelled = self.service.cancel_current();
                Some(AcpResponse::ok(id, json!({ "cancelled": cancelled })))
            }

            "policy/grant" => {
                let perm_id = req.params.get("id").and_then(Value::as_str).unwrap_or("");
                let allowed = req.params.get("allowed").and_then(Value::as_bool).unwrap_or(false);
                let resolved = self.service.resolve_permission(perm_id, allowed);
                Some(AcpResponse::ok(id, json!({ "resolved": resolved })))
            }

            "agent/chat" => {
                let prompt = req.params.get("prompt").and_then(Value::as_str).unwrap_or("").to_string();
                if prompt.is_empty() {
                    return Some(AcpResponse::err(id, -32602, "缺少 prompt 参数"));
                }

                let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
                let sink = EventSink::new(tx);

                // 启动后台事件中继通知
                let notify_clone = notify.clone();
                let events_collector = tokio::spawn(async move {
                    let mut collected = Vec::new();
                    while let Some(evt) = rx.recv().await {
                        if let Some(ref n) = notify_clone {
                            n(AcpNotification::new(
                                "event/stream",
                                serde_json::to_value(&evt).unwrap_or(Value::Null),
                            ));
                        }
                        collected.push(evt);
                    }
                    collected
                });

                let chat_result = self.service.chat(&prompt, &sink).await;
                drop(sink);

                let collected_events = events_collector.await.unwrap_or_default();

                match chat_result {
                    Ok(outcome) => {
                        let outcome_val = outcome_to_json(&outcome);
                        Some(AcpResponse::ok(id, json!({
                            "outcome": outcome_val,
                            "events_count": collected_events.len(),
                        })))
                    }
                    Err(e) => Some(AcpResponse::err(id, -32000, e.to_string())),
                }
            }

            "card/interact" => {
                let card_id = req.params.get("card_id").and_then(Value::as_str).unwrap_or("").to_string();
                let action = req.params.get("action").and_then(Value::as_str).unwrap_or("").to_string();
                let data = req.params.get("data").cloned().unwrap_or(json!({}));

                if card_id.is_empty() || action.is_empty() {
                    return Some(AcpResponse::err(id, -32602, "缺少 card_id 或 action 参数"));
                }

                let interaction = CardInteraction::new(&card_id, &action, data.clone());

                // 广播 card/interacted 通知
                if let Some(ref n) = notify {
                    n(AcpNotification::new(
                        "card/interacted",
                        serde_json::to_value(&interaction).unwrap_or(Value::Null),
                    ));
                }

                // 将用户卡片操作事实注入为新一轮会话输入
                let interaction_prompt = format!(
                    "[用户卡片交互] 针对卡片 '{}' 触发了动作 '{}'，回投数据：\n{}",
                    card_id,
                    action,
                    serde_json::to_string_pretty(&data).unwrap_or_default()
                );

                let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
                let sink = EventSink::new(tx);

                let notify_clone = notify.clone();
                let collector = tokio::spawn(async move {
                    let mut collected = Vec::new();
                    while let Some(evt) = rx.recv().await {
                        if let Some(ref n) = notify_clone {
                            n(AcpNotification::new(
                                "event/stream",
                                serde_json::to_value(&evt).unwrap_or(Value::Null),
                            ));
                        }
                        collected.push(evt);
                    }
                    collected
                });

                let chat_result = self.service.chat(&interaction_prompt, &sink).await;
                drop(sink);
                let _ = collector.await;

                match chat_result {
                    Ok(outcome) => {
                        let outcome_val = outcome_to_json(&outcome);
                        Some(AcpResponse::ok(id, json!({
                            "status": "interaction_processed",
                            "outcome": outcome_val
                        })))
                    }
                    Err(e) => Some(AcpResponse::err(id, -32000, e.to_string())),
                }
            }

            unknown => Some(AcpResponse::err(id, -32601, format!("未知的 ACP 方法: {unknown}"))),
        }
    }
}

fn outcome_to_json(outcome: &goldenagent_loop::Outcome) -> Value {
    match outcome {
        goldenagent_loop::Outcome::Completed { turns } => json!({ "status": "completed", "turns": turns }),
        goldenagent_loop::Outcome::MaxTurns => json!({ "status": "max_turns" }),
        goldenagent_loop::Outcome::Cancelled => json!({ "status": "cancelled" }),
    }
}
