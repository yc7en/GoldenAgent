//! 授权：把"要不要批准"变成一次事件往返。
//!
//! 循环发 `Event::PermissionRequested` 后调用 [`Approver::approve`]，
//! 这里挂一个 `oneshot` 等着；前端看到事件、用户点了按钮，
//! 通过 `AgentService::resolve_permission(id, allowed)` 把答案送回来。
//!
//! **没有答案就是拒绝**：超时、窗口关掉、前端崩了，一律按拒绝处理。
//! 默认放行才是危险的默认值。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use goldenagent_loop::{ApprovalRequest, Approver};
use tokio::sync::oneshot;

pub struct EventApprover {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    timeout: Duration,
}

impl EventApprover {
    pub fn new(timeout: Duration) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            timeout,
        }
    }

    /// 前端回话。返回 `false` 表示这个 id 已经不在等待了（超时或重复点击）。
    pub fn resolve(&self, id: &str, allowed: bool) -> bool {
        let sender = self.pending.lock().ok().and_then(|mut map| map.remove(id));
        match sender {
            Some(sender) => sender.send(allowed).is_ok(),
            None => false,
        }
    }

    pub fn pending_count(&self) -> usize {
        self.pending.lock().map(|map| map.len()).unwrap_or(0)
    }
}

#[async_trait]
impl Approver for EventApprover {
    async fn approve(&self, request: ApprovalRequest) -> bool {
        let (tx, rx) = oneshot::channel();
        if let Ok(mut map) = self.pending.lock() {
            map.insert(request.id.clone(), tx);
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(allowed)) => allowed,
            // 超时或发送端被丢弃：都算拒绝
            _ => {
                if let Ok(mut map) = self.pending.lock() {
                    map.remove(&request.id);
                }
                false
            }
        }
    }
}
