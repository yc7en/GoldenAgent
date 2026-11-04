use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use goldenagent_core::{ContentBlock, Message, Result, Role, StopReason};

use crate::{Completion, CompletionRequest, Provider};

/// 离线 provider：按脚本逐轮回话，脚本用完就回显最后一条用户消息。
///
/// 存在的意义不是"能用"，而是让整个链路（循环、事件、策略、前端）
/// 在没有 API key、没有网络的情况下可以端到端跑起来并写测试。
pub struct MockProvider {
    script: Mutex<VecDeque<Message>>,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl MockProvider {
    pub fn new(script: Vec<Message>) -> Self {
        Self {
            script: Mutex::new(script.into()),
        }
    }

    /// 追加一轮预设回复。
    pub fn push(&self, message: Message) -> &Self {
        if let Ok(mut guard) = self.script.lock() {
            guard.push_back(message);
        }
        self
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &'static str {
        "mock"
    }

    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let scripted = self
            .script
            .lock()
            .ok()
            .and_then(|mut guard| guard.pop_front());

        let message = scripted.unwrap_or_else(|| {
            let last = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .map(Message::text)
                .unwrap_or_default();
            Message::assistant(vec![ContentBlock::Text {
                text: format!("（离线回显）{last}"),
            }])
        });

        Ok(Completion {
            message,
            stop_reason: StopReason::EndTurn,
        })
    }
}
