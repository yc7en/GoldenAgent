use serde::{Deserialize, Serialize};

/// 富交互卡片类别（Generative UI / PresentCall）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiCardKind {
    /// 确认卡片：带危险等级的确认/取消按钮
    Confirmation,
    /// 代码 Diff 审查卡片：多文件修改对比与行级审查
    DiffReview,
    /// 表单输入卡片：多字段结构化参数输入
    FormInput,
    /// 任务清单卡片：多步骤进度动态清单
    ProgressChecklist,
    /// 自定义扩展卡片
    Custom,
}

/// 声明式富交互卡片规范。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiCardSpec {
    pub id: String,
    pub kind: UiCardKind,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub payload: serde_json::Value,
}

impl UiCardSpec {
    pub fn new(
        id: impl Into<String>,
        kind: UiCardKind,
        title: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            title: title.into(),
            description: None,
            payload,
        }
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }
}

/// 客户端回投的卡片二次交互事实。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardInteraction {
    pub card_id: String,
    pub action: String,
    pub data: serde_json::Value,
}

impl CardInteraction {
    pub fn new(card_id: impl Into<String>, action: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            card_id: card_id.into(),
            action: action.into(),
            data,
        }
    }
}
