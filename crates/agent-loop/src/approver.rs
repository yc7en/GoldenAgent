//! 授权询问（A-7）。
//!
//! 循环需要人工点头时，只能通过这个 trait 提问：
//! 不弹窗、不碰 UI、不直接读 stdin。谁来回答由上层决定——
//! 桌面端接事件 + 用户点击，CLI 接终端提示，测试接固定答案。

use async_trait::async_trait;

/// 一次授权请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub id: String,
    pub name: String,
    pub summary: String,
}

#[async_trait]
pub trait Approver: Send + Sync {
    /// 返回 `true` 表示放行。
    async fn approve(&self, request: ApprovalRequest) -> bool;
}

/// 一律拒绝。一期默认行为：Ask 档不授权。
pub struct AutoDeny;

#[async_trait]
impl Approver for AutoDeny {
    async fn approve(&self, _request: ApprovalRequest) -> bool {
        false
    }
}

/// 一律放行。只用于测试与"信任模式"预演，不要作为默认值。
pub struct AutoAllow;

#[async_trait]
impl Approver for AutoAllow {
    async fn approve(&self, _request: ApprovalRequest) -> bool {
        true
    }
}
