use serde::{Deserialize, Serialize};

/// 产物动作枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactAction {
    Created,
    Modified,
    Deleted,
}

/// 派生代码产物
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub path: String,
    pub action: ArtifactAction,
    pub updated_at_ms: u64,
}

/// 前端 UI 投影状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UIState {
    pub messages_count: usize,
    pub user_prompts_count: usize,
    pub assistant_turns_count: usize,
    pub tools_called_count: usize,
    pub has_compacted: bool,
    pub active_checkpoint: Option<String>,
    pub artifacts: Vec<Artifact>,
}

/// 会话事件流中的单一不可变物理事件（Append-Only Event Stream）。
///
/// 遵循 ARCHITECTURE.md A-19：
/// 物理存储永远纯追加写，绝对禁止物理覆写或截断历史；
/// 所有的消息（LLM 上下文）、UI 卡片和代码产物均由事件流经纯函数确定性折叠投影得出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// 世代版本头（文件首行可选元信息）
    GenerationHeader {
        version: u32,
        created_at_ms: u64,
    },
    /// 用户输入
    UserPrompt {
        id: String,
        text: String,
        timestamp_ms: u64,
    },
    /// 助手思考片段
    AssistantThinking {
        thought: String,
        timestamp_ms: u64,
    },
    /// 助手文本回复
    AssistantResponse {
        text: String,
        timestamp_ms: u64,
    },
    /// 工具调用请求
    ToolCall {
        call_id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        timestamp_ms: u64,
    },
    /// 工具执行结果
    ToolResult {
        call_id: String,
        ok: bool,
        output: String,
        timestamp_ms: u64,
    },
    /// 上下文压缩检查点标记（不删历史，仅记录折叠标记）
    CompactionCheckpoint {
        checkpoint_id: String,
        summary: String,
        #[serde(default)]
        compacted_event_ids: Vec<String>,
        timestamp_ms: u64,
    },
    /// 显式产物声明
    ArtifactDeclared {
        path: String,
        action: ArtifactAction,
        timestamp_ms: u64,
    },
}

impl SessionEvent {
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::UserPrompt { id, .. } => Some(id),
            Self::ToolCall { call_id, .. } => Some(call_id),
            Self::ToolResult { call_id, .. } => Some(call_id),
            Self::CompactionCheckpoint { checkpoint_id, .. } => Some(checkpoint_id),
            _ => None,
        }
    }

    pub fn timestamp_ms(&self) -> u64 {
        match self {
            Self::GenerationHeader { created_at_ms, .. } => *created_at_ms,
            Self::UserPrompt { timestamp_ms, .. }
            | Self::AssistantThinking { timestamp_ms, .. }
            | Self::AssistantResponse { timestamp_ms, .. }
            | Self::ToolCall { timestamp_ms, .. }
            | Self::ToolResult { timestamp_ms, .. }
            | Self::CompactionCheckpoint { timestamp_ms, .. }
            | Self::ArtifactDeclared { timestamp_ms, .. } => *timestamp_ms,
        }
    }
}
