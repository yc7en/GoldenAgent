use async_trait::async_trait;
use serde_json::{json, Value};
use goldenagent_core::{Error, Result, ToolKind, ToolSpec, UiCardKind, UiCardSpec};

use crate::{Tool, ToolContext};

pub struct PresentCard;

#[async_trait]
impl Tool for PresentCard {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "present_card",
            "向用户展示声明式富交互卡片（Generative UI）。支持 confirmation（操作确认）、diff_review（文件修改审查）、form_input（多字段配置表单）、progress_checklist（任务清单）。",
            json!({
                "type": "object",
                "properties": {
                    "card_id": { "type": "string", "description": "卡片唯一标识符" },
                    "kind": {
                        "type": "string",
                        "enum": ["confirmation", "diff_review", "form_input", "progress_checklist", "custom"],
                        "description": "卡片类别"
                    },
                    "title": { "type": "string", "description": "卡片标题" },
                    "description": { "type": "string", "description": "可选卡片描述" },
                    "payload": { "type": "object", "description": "卡片专有内容结构体（如 options、diff、fields 等）" },
                    "summary": { "type": "string", "description": "操作意图说明（4-8字，如'展示确认卡片'）" }
                },
                "required": ["card_id", "kind", "title"]
            }),
            ToolKind::Read,
        )
    }

    async fn run(&self, input: Value, _ctx: &ToolContext) -> Result<String> {
        let card_id = input
            .get("card_id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("缺少 card_id 参数".into()))?;

        let kind_str = input
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("缺少 kind 参数".into()))?;

        let kind = match kind_str {
            "confirmation" => UiCardKind::Confirmation,
            "diff_review" => UiCardKind::DiffReview,
            "form_input" => UiCardKind::FormInput,
            "progress_checklist" => UiCardKind::ProgressChecklist,
            _ => UiCardKind::Custom,
        };

        let title = input
            .get("title")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("缺少 title 参数".into()))?;

        let description = input.get("description").and_then(Value::as_str);
        let payload = input.get("payload").cloned().unwrap_or(json!({}));

        let mut card = UiCardSpec::new(card_id, kind, title, payload);
        if let Some(desc) = description {
            card = card.with_description(desc);
        }

        let card_json = serde_json::to_string_pretty(&card)
            .map_err(|e| Error::Tool(format!("序列化富交互卡片失败: {e}")))?;

        Ok(format!("富交互卡片已成功下发给客户端：\n{card_json}"))
    }
}
