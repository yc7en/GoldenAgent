use serde::{Deserialize, Serialize};

/// 后台任务运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// 后台任务摘要与元信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskInfo {
    pub id: String,
    pub kind: String,
    pub description: String,
    pub status: TaskStatus,
    pub created_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub output: Option<String>,
    pub error: Option<String>,
}
