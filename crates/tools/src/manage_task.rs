use async_trait::async_trait;
use goldenagent_core::{Error, Result, TaskStatus, ToolKind, ToolSpec};
use serde_json::json;

use crate::{required_str, Tool, ToolContext};

/// B21: 后台作业管理工具（manage_task）。
pub struct ManageTask;

#[async_trait]
impl Tool for ManageTask {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "manage_task".into(),
            kind: ToolKind::Read,
            description: "管理后台作业系统（Jobs/Tasks）：查看运行中的后台任务列表、检查特定任务的状态与输出日志、或中止正在执行的长耗时任务。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "status", "cancel"],
                        "description": "操作动作：'list'（列出全部任务）、'status'（查询任务详情）、'cancel'（终止运行中的任务）"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "任务 ID，当 action 为 status 或 cancel 时必填"
                    },
                    "summary": {
                        "type": "string",
                        "description": "4-8 字操作意图，如 '查询后台任务'、'终止编译作业'"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let action = required_str(&input, "action")?;

        match action {
            "list" => {
                let tasks = ctx.tasks.list();
                if tasks.is_empty() {
                    return Ok("当前没有任何后台任务。".into());
                }
                let mut out = format!("共 {} 个后台作业：\n", tasks.len());
                for t in tasks {
                    let status_str = match t.status {
                        TaskStatus::Running => "⏳ 运行中",
                        TaskStatus::Completed => "✅ 已完成",
                        TaskStatus::Failed => "❌ 失败",
                        TaskStatus::Cancelled => "🛑 已终止",
                    };
                    out.push_str(&format!(
                        "- ID: {} | 类型: {} | 状态: {} | 描述: {}\n",
                        t.id, t.kind, status_str, t.description
                    ));
                }
                Ok(out)
            }
            "status" => {
                let task_id = required_str(&input, "task_id")?;
                let task = ctx
                    .tasks
                    .get(task_id)
                    .ok_or_else(|| Error::Tool(format!("未找到任务 ID: {task_id}")))?;

                let status_str = match task.status {
                    TaskStatus::Running => "⏳ 运行中",
                    TaskStatus::Completed => "✅ 已完成",
                    TaskStatus::Failed => "❌ 失败",
                    TaskStatus::Cancelled => "🛑 已终止",
                };

                let mut out = format!(
                    "任务 ID: {}\n类型: {}\n描述: {}\n状态: {}\n",
                    task.id, task.kind, task.description, status_str
                );
                if let Some(res) = task.output {
                    out.push_str(&format!("\n执行产物/输出：\n{res}\n"));
                }
                if let Some(err) = task.error {
                    out.push_str(&format!("\n错误信息：\n{err}\n"));
                }
                Ok(out)
            }
            "cancel" => {
                let task_id = required_str(&input, "task_id")?;
                let cancelled = ctx.tasks.cancel(task_id);
                if cancelled {
                    Ok(format!("后台任务 {task_id} 已成功终止。"))
                } else {
                    let task = ctx.tasks.get(task_id);
                    match task {
                        Some(t) => Ok(format!(
                            "后台任务 {} 状态为 {:?}，无需终止。",
                            task_id, t.status
                        )),
                        None => Err(Error::Tool(format!("未找到任务 ID: {task_id}"))),
                    }
                }
            }
            other => Err(Error::Tool(format!(
                "不支持的 action: {other}，请使用 'list'、'status' 或 'cancel'"
            ))),
        }
    }
}
